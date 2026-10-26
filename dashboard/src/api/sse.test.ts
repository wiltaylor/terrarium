import { describe, expect, it } from "vitest";
import { ApiError } from "./http";
import { createSseParser, createTranscriptStream, encodeTranscriptFrame, isEndStatus, toTranscriptFrame, type SseMessage, type TranscriptStreamState } from "./sse";
import type { Block, TranscriptFrame } from "./types";

const block = (seq: number, text = `b${seq}`): Block => ({
  id: `s-b${seq}`,
  session: "s",
  seq,
  at: null,
  role: "assistant",
  kind: "text",
  text,
  source: { harness: "claude", file: "x.jsonl", line: seq },
});

const parseAll = (chunks: string[]) => {
  const out: SseMessage[] = [];
  const p = createSseParser((m) => out.push(m));
  chunks.forEach((c) => p.push(c));
  p.end();
  return out;
};

describe("SSE parser", () => {
  it("reads events split across chunks at any point", () => {
    const text = encodeTranscriptFrame({ type: "block", block: block(1) }) + encodeTranscriptFrame({ type: "status", status: "idle" });
    const whole = parseAll([text]);
    for (let cut = 1; cut < text.length; cut += 7) expect(parseAll([text.slice(0, cut), text.slice(cut)])).toEqual(whole);
    expect(whole.map((m) => m.event)).toEqual(["block", "status"]);
    expect(whole[0]!.id).toBe("1");
  });

  it("takes CRLF and CR line ends, also a CRLF split across chunks", () => {
    expect(parseAll(["event: status\r\ndata: {\"status\":\"x\"}\r", "\n\r\n"])).toEqual([{ event: "status", data: '{"status":"x"}' }]);
    expect(parseAll(["data: a\rdata: b\r\r"])).toEqual([{ event: "message", data: "a\nb" }]);
  });

  it("skips comments and keep-alives, and drops an event with no blank line at the end", () => {
    expect(parseAll([": keep-alive\n\n", "event: block\ndata: 1\n\n", "event: block\ndata: 2"])).toEqual([{ event: "block", data: "1" }]);
  });

  it("reads frames: block, update, status; drops bad JSON and unknown events", () => {
    expect(toTranscriptFrame({ event: "block", data: JSON.stringify(block(3)) })).toMatchObject({ type: "block", block: { seq: 3 } });
    expect(toTranscriptFrame({ event: "update", data: JSON.stringify(block(3, "more")) })).toMatchObject({ type: "update", block: { text: "more" } });
    expect(toTranscriptFrame({ event: "status", data: '{"status":"ended"}' })).toEqual({ type: "status", status: "ended" });
    expect(toTranscriptFrame({ event: "block", data: "{nope" })).toBeNull();
    expect(toTranscriptFrame({ event: "block", data: '{"seq":"1"}' })).toBeNull();
    expect(toTranscriptFrame({ event: "other", data: "{}" })).toBeNull();
    expect(isEndStatus("ended")).toBe(true);
    expect(isEndStatus("working")).toBe(false);
  });
});

/** A controllable server: each open gets a queue the test writes to. */
function fakeServer() {
  const opens: { after: number | undefined; push: (s: string) => void; end: () => void; fail: (e: unknown) => void }[] = [];
  let refuse: unknown = null;
  const open = async (after: number | undefined, signal: AbortSignal): Promise<AsyncIterable<string>> => {
    if (refuse) throw refuse;
    const q: string[] = [];
    let wake: (() => void) | null = null;
    let done = false;
    let err: unknown = null;
    const kick = () => wake?.();
    signal.addEventListener("abort", () => ((done = true), kick()));
    opens.push({ after, push: (s) => (q.push(s), kick()), end: () => ((done = true), kick()), fail: (e) => ((err = e), kick()) });
    return {
      async *[Symbol.asyncIterator]() {
        for (;;) {
          if (err) throw err;
          if (q.length) {
            yield q.shift()!;
            continue;
          }
          if (done) return;
          await new Promise<void>((r) => (wake = r));
        }
      },
    };
  };
  return { open, opens, refuse: (e: unknown) => (refuse = e) };
}

const tick = () => new Promise((r) => setTimeout(r, 0));

describe("transcript stream", () => {
  it("resumes after the highest block seq and drops a replayed block", async () => {
    const srv = fakeServer();
    const timers: (() => void)[] = [];
    const s = createTranscriptStream(srv.open, { after: 4, setTimer: (fn) => timers.push(fn), clearTimer: () => {} });
    const got: TranscriptFrame[] = [];
    s.subscribe((f) => got.push(f));
    await tick();
    expect(srv.opens[0]!.after).toBe(4);
    srv.opens[0]!.push(encodeTranscriptFrame({ type: "block", block: block(5) }));
    srv.opens[0]!.push(encodeTranscriptFrame({ type: "update", block: block(5, "grown") }));
    await tick();
    srv.opens[0]!.end();
    await tick();
    expect(timers).toHaveLength(1);
    timers.shift()!();
    await tick();
    expect(srv.opens[1]!.after).toBe(5);
    // The server replays 5 (an overlap), then 6.
    srv.opens[1]!.push(encodeTranscriptFrame({ type: "block", block: block(5) }) + encodeTranscriptFrame({ type: "block", block: block(6) }));
    await tick();
    expect(got.map((f) => (f.type === "status" ? f.status : `${f.type}:${f.block.seq}`))).toEqual(["block:5", "update:5", "block:6"]);
    s.close();
  });

  it("does not retry a refusal (404, 401); reports it as failed", async () => {
    const srv = fakeServer();
    srv.refuse(new ApiError(404, "no such API route", undefined, "not_found"));
    const timers: (() => void)[] = [];
    const s = createTranscriptStream(srv.open, { setTimer: (fn) => timers.push(fn) });
    const states: [TranscriptStreamState, unknown][] = [];
    s.onState((st, e) => states.push([st, e]));
    await tick();
    expect(states.at(-1)![0]).toBe("failed");
    expect((states.at(-1)![1] as ApiError).status).toBe(404);
    expect(timers).toHaveLength(0);
  });

  it("retries a network error with backoff", async () => {
    const srv = fakeServer();
    srv.refuse(new TypeError("fetch failed"));
    const delays: number[] = [];
    createTranscriptStream(srv.open, { setTimer: (_fn, ms) => delays.push(ms), random: () => 0 });
    await tick();
    expect(delays).toEqual([400]);
  });

  it("stops after an end status: no reconnect when the server closes", async () => {
    const srv = fakeServer();
    const timers: (() => void)[] = [];
    const s = createTranscriptStream(srv.open, { setTimer: (fn) => timers.push(fn) });
    const states: TranscriptStreamState[] = [];
    s.onState((st) => states.push(st));
    await tick();
    srv.opens[0]!.push(encodeTranscriptFrame({ type: "status", status: "ended" }));
    await tick();
    srv.opens[0]!.end();
    await tick();
    expect(timers).toHaveLength(0);
    expect(states.at(-1)).toBe("closed");
  });

  it("close aborts the open request and delivers nothing after", async () => {
    const srv = fakeServer();
    const s = createTranscriptStream(srv.open);
    const got: TranscriptFrame[] = [];
    s.subscribe((f) => got.push(f));
    await tick();
    s.close();
    srv.opens[0]!.push(encodeTranscriptFrame({ type: "block", block: block(1) }));
    await tick();
    expect(got).toEqual([]);
  });
});

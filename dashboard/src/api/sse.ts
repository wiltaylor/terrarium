// Server-sent events for the session transcript (docs/api.md §6, transcripts).
//
// `GET /api/agents/{id}/sessions/{session}/transcript/stream?after=<seq>`
// answers `text/event-stream` with three event names:
//   block   data: a Block, new at the end of the transcript
//   update  data: a Block that grew (the same seq and id)
//   status  data: {"status": "..."}
//
// The browser's `EventSource` cannot send `Authorization`, so the client
// reads the stream with `fetch` (cookie or bearer, as every other call) and
// parses it here. The stream reconnects with backoff and resumes with
// `?after=<the highest block seq seen>`, so a drop loses no block.

import { ApiError } from "./http";
import { backoffDelay } from "./sockets";
import type { Block, TranscriptFrame } from "./types";

export interface SseMessage {
  event: string;
  data: string;
  id?: string;
}

/**
 * An incremental `text/event-stream` parser (WHATWG HTML §9.2.6). Feed it
 * text chunks in any split; it calls `onMessage` once for each complete event.
 */
export function createSseParser(onMessage: (m: SseMessage) => void): { push(chunk: string): void; end(): void } {
  let buf = "";
  let event = "";
  let data: string[] = [];
  let id: string | undefined;
  let sawCr = false;

  const line = (l: string) => {
    if (l === "") {
      if (data.length) onMessage({ event: event || "message", data: data.join("\n"), ...(id !== undefined ? { id } : {}) });
      event = "";
      data = [];
      return;
    }
    if (l.startsWith(":")) return; // comment, for example a keep-alive
    const colon = l.indexOf(":");
    const field = colon < 0 ? l : l.slice(0, colon);
    let value = colon < 0 ? "" : l.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") event = value;
    else if (field === "data") data.push(value);
    else if (field === "id" && !value.includes("\u0000")) id = value;
    // `retry` and unknown fields: ignored.
  };

  return {
    push(chunk: string) {
      let s = chunk;
      // A CR at the end of the last chunk and an LF at the start of this one are one line end.
      if (sawCr && s.startsWith("\n")) s = s.slice(1);
      sawCr = false;
      buf += s;
      for (;;) {
        const m = /\r\n|\r|\n/.exec(buf);
        if (!m) break;
        // A lone CR at the very end may be the first half of a CRLF.
        if (m[0] === "\r" && m.index === buf.length - 1) {
          line(buf.slice(0, m.index));
          buf = "";
          sawCr = true;
          break;
        }
        line(buf.slice(0, m.index));
        buf = buf.slice(m.index + m[0].length);
      }
    },
    end() {
      // An event without its blank line at the end of the stream is dropped (the spec says so).
      buf = "";
      event = "";
      data = [];
    },
  };
}

/** Read one SSE message as a transcript frame. Null for an unknown event or bad JSON. */
export function toTranscriptFrame(m: SseMessage): TranscriptFrame | null {
  let v: unknown;
  try {
    v = JSON.parse(m.data);
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null) return null;
  if (m.event === "block" || m.event === "update") {
    const b = v as Block;
    if (typeof b.seq !== "number" || typeof b.id !== "string") return null;
    return { type: m.event, block: b };
  }
  if (m.event === "status") {
    const st = (v as { status?: unknown }).status;
    return typeof st === "string" ? { type: "status", status: st } : null;
  }
  return null;
}

/** Encode a frame as the daemon sends it. The mock and the tests use it. */
export function encodeTranscriptFrame(f: TranscriptFrame): string {
  if (f.type === "status") return `event: status\ndata: ${JSON.stringify({ status: f.status })}\n\n`;
  return `event: ${f.type}\nid: ${f.block.seq}\ndata: ${JSON.stringify(f.block)}\n\n`;
}

/** A `status` frame that says the session will not grow again. The stream then stops. */
export const isEndStatus = (s: string): boolean => ["ended", "inactive", "stopped", "closed"].includes(s);

export type TranscriptStreamState = "connecting" | "open" | "closed" | "failed";

export interface TranscriptStream {
  subscribe(fn: (f: TranscriptFrame) => void): () => void;
  /** Called at once with the current state. `failed` carries the error; the stream does not retry it. */
  onState(fn: (s: TranscriptStreamState, error?: unknown) => void): () => void;
  close(): void;
}

/** Open the stream after `after`. The async iterable yields text chunks. */
export type OpenTranscript = (after: number | undefined, signal: AbortSignal) => Promise<AsyncIterable<string>>;

export interface TranscriptStreamOptions {
  after?: number;
  minDelay?: number;
  maxDelay?: number;
  random?: () => number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (t: unknown) => void;
}

/** A refusal that a retry does not fix: 4xx but a timeout or a rate limit. */
const permanent = (e: unknown) => e instanceof ApiError && e.status >= 400 && e.status < 500 && e.status !== 408 && e.status !== 429;

export function createTranscriptStream(open: OpenTranscript, opts: TranscriptStreamOptions = {}): TranscriptStream {
  const setTimer = opts.setTimer ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));
  const clearTimer = opts.clearTimer ?? ((t: unknown) => clearTimeout(t as ReturnType<typeof setTimeout>));
  const listeners = new Set<(f: TranscriptFrame) => void>();
  const stateListeners = new Set<(s: TranscriptStreamState, error?: unknown) => void>();
  let state: TranscriptStreamState = "connecting";
  let lastError: unknown;
  let lastSeq = opts.after;
  let attempt = 0;
  let closed = false;
  let ended = false;
  let timer: unknown;
  let abort: AbortController | null = null;

  const setState = (s: TranscriptStreamState, error?: unknown) => {
    state = s;
    lastError = error;
    stateListeners.forEach((fn) => fn(s, error));
  };

  const deliver = (f: TranscriptFrame) => {
    if (f.type === "block") {
      if (lastSeq !== undefined && f.block.seq <= lastSeq) return; // seen already (a replay after a resume)
      lastSeq = f.block.seq;
    }
    if (f.type === "status" && isEndStatus(f.status)) ended = true;
    listeners.forEach((fn) => fn(f));
  };

  const retry = () => {
    if (closed) return;
    if (ended) return setState("closed");
    const delay = backoffDelay(attempt, opts.minDelay, opts.maxDelay, opts.random);
    attempt++;
    setState("closed");
    timer = setTimer(() => void connect(), delay);
  };

  async function connect() {
    if (closed) return;
    timer = undefined;
    setState("connecting");
    const ctl = new AbortController();
    abort = ctl;
    try {
      const chunks = await open(lastSeq, ctl.signal);
      if (closed || abort !== ctl) return;
      attempt = 0;
      setState("open");
      const parser = createSseParser((m) => {
        const f = toTranscriptFrame(m);
        if (f && !closed && abort === ctl) deliver(f);
      });
      for await (const chunk of chunks) {
        if (closed || abort !== ctl) return;
        parser.push(chunk);
      }
      parser.end();
    } catch (e) {
      if (closed || abort !== ctl) return;
      if (permanent(e)) return setState("failed", e);
    }
    if (abort === ctl) abort = null;
    retry();
  }
  void connect();

  return {
    subscribe(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onState(fn) {
      stateListeners.add(fn);
      fn(state, lastError);
      return () => stateListeners.delete(fn);
    },
    close() {
      if (closed) return;
      closed = true;
      if (timer !== undefined) clearTimer(timer);
      abort?.abort();
      abort = null;
      setState("closed");
    },
  };
}

/** Text chunks of a fetch body. */
export async function* readBody(body: ReadableStream<Uint8Array>): AsyncIterable<string> {
  const reader = body.getReader();
  const dec = new TextDecoder();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      yield dec.decode(value, { stream: true });
    }
    const rest = dec.decode();
    if (rest) yield rest;
  } finally {
    reader.releaseLock();
  }
}

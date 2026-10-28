// The mock's transcripts must answer in the contract's shapes (docs/api.md
// §6, transcripts): the session list fields, the transcript page, the block,
// and the SSE stream (block, update, status).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { TranscriptStreamState } from "../sse";
import type { Block, TranscriptFrame } from "../types";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));

const BLOCK_KEYS = new Set(["id", "session", "seq", "at", "role", "kind", "text", "tool", "result", "redacted", "origin", "parts", "model", "usage", "source"]);
const HARNESS_AGENTS = { claude: "terrarium/lead", codex: "terrarium/adapter-codex", pi: "terrarium/reviewer", copilot: "webshop/shop-lead" } as const;

function checkBlock(b: Block, harness: string) {
  for (const k of Object.keys(b)) expect(BLOCK_KEYS.has(k), `unknown block field ${k}`).toBe(true);
  expect(typeof b.id).toBe("string");
  expect(typeof b.seq).toBe("number");
  expect(b.at === null || typeof b.at === "string").toBe(true);
  expect(["user", "assistant", "system"]).toContain(b.role);
  expect(["text", "thinking", "tool_call", "tool_result", "system"]).toContain(b.kind);
  expect(b.source).toMatchObject({ harness, file: expect.any(String), line: expect.any(Number) });
  if (b.kind === "tool_call") expect(b.tool).toMatchObject({ id: expect.any(String), name: expect.any(String), summary: expect.any(String) });
  if (b.kind === "tool_call") expect(b.tool).toHaveProperty("input");
  if (b.kind === "tool_result") expect(b.result).toMatchObject({ tool_id: expect.any(String), output: expect.any(String), is_error: expect.any(Boolean) });
  if (b.origin) expect(["operator", "agent_mail_pointer", "system", "user"]).toContain(b.origin);
  for (const p of b.parts ?? []) {
    expect(["user", "mailbox", "system"]).toContain(p.source);
    expect(typeof p.body).toBe("string");
  }
  if (b.parts?.length) expect(b.origin).toBe({ user: "operator", mailbox: "agent_mail_pointer", system: "system" }[b.parts[0]!.source]);
  if (b.usage) expect(Object.keys(b.usage).sort()).toEqual(["cache_read", "cache_write", "input", "output"]);
}

describe("transcripts in the mock (docs/api.md §6)", () => {
  it("lists sessions newest first with transcript, blocks, title, and active", async () => {
    const api = mock();
    const lead = await api.agents.get("terrarium/lead");
    const list = (await api.agents.sessions(lead.id)).items;
    expect(list.length).toBe(2);
    expect(list[0]!.started_at > list[1]!.started_at).toBe(true);
    for (const s of list) {
      expect(s).toHaveProperty("transcript");
      expect(typeof s.blocks).toBe("number");
      expect(s).toHaveProperty("title");
      expect(typeof s.active).toBe("boolean");
    }
    expect(list[0]).toMatchObject({ current: true, active: true, transcript: true });
    expect(list[0]!.title).toMatch(/^\[Mailbox:operator\] Subject: Transcript view/);
    expect(list[1]).toMatchObject({ current: false, active: false });
    // A current session of an agent in error is not active: no harness runs it.
    const css = await api.agents.get("webshop/css-worker");
    expect((await api.agents.sessions(css.id)).items.find((s) => s.current)!.active).toBe(false);
  });

  for (const [harness, key] of Object.entries(HARNESS_AGENTS)) {
    it(`answers a ${harness} transcript page in the contract's shape`, async () => {
      const api = mock();
      const a = await api.agents.get(key);
      const ses = (await api.agents.sessions(a.id)).items.find((s) => s.current)!;
      const page = await api.agents.transcript(a.id, ses.id, { after: 0, limit: 500 });
      expect(Object.keys(page).sort()).toEqual(["active", "blocks", "complete", "harness", "next_after", "session"]);
      expect(page).toMatchObject({ session: ses.id, harness, complete: true });
      expect(page.blocks.length).toBe(ses.blocks);
      expect(page.blocks.map((b) => b.seq)).toEqual(page.blocks.map((_, i) => i + 1));
      expect(page.next_after).toBe(page.blocks.at(-1)!.seq);
      for (const b of page.blocks) checkBlock(b, harness);
      // Every result has its call before it.
      const calls = new Set<string>();
      for (const b of page.blocks) {
        if (b.kind === "tool_call") calls.add(b.tool!.id);
        if (b.kind === "tool_result") expect(calls.has(b.result!.tool_id)).toBe(true);
      }
      expect(page.blocks.some((b) => b.kind === "tool_call")).toBe(true);
      expect(page.blocks.some((b) => b.role === "assistant" && b.kind === "text" && b.model && b.usage)).toBe(true);
    });
  }

  it("covers a failed tool result, redacted thinking, a mail-pointer turn, and a truncated result", async () => {
    const api = mock();
    const all: Block[] = [];
    for (const key of Object.values(HARNESS_AGENTS)) {
      const a = await api.agents.get(key);
      const ses = (await api.agents.sessions(a.id)).items.find((s) => s.current)!;
      all.push(...(await api.agents.transcript(a.id, ses.id, { after: 0, limit: 500 })).blocks);
    }
    expect(all.some((b) => b.result?.is_error)).toBe(true);
    expect(all.some((b) => b.kind === "thinking" && b.redacted && !b.text)).toBe(true);
    expect(all.some((b) => b.kind === "thinking" && b.text)).toBe(true);
    expect(all.some((b) => b.role === "user" && b.origin === "agent_mail_pointer" && b.parts?.[0]?.source === "mailbox")).toBe(true);
    expect(all.some((b) => b.parts?.[0]?.source === "system")).toBe(true);
    expect(all.some((b) => b.result?.truncated)).toBe(true);
    expect(all.some((b) => b.kind === "system")).toBe(true);
    expect(all.some((b) => b.at === null)).toBe(true);
  });

  it("pages with after, limit, next_after, and complete", async () => {
    const api = mock();
    const a = await api.agents.get("terrarium/runner-dev");
    const ses = (await api.agents.sessions(a.id)).items.find((s) => s.current)!;
    expect(ses.blocks).toBeGreaterThan(500);
    const p1 = await api.agents.transcript(a.id, ses.id, { after: 0, limit: 200 });
    expect(p1.blocks).toHaveLength(200);
    expect(p1).toMatchObject({ next_after: 200, complete: false });
    const last = await api.agents.transcript(a.id, ses.id, { after: ses.blocks! - 10, limit: 200 });
    expect(last.blocks).toHaveLength(10);
    expect(last.complete).toBe(true);
    const past = await api.agents.transcript(a.id, ses.id, { after: ses.blocks! + 5 });
    expect(past).toMatchObject({ blocks: [], next_after: ses.blocks! + 5, complete: true });
    await expect(api.agents.transcript(a.id, "nope")).rejects.toMatchObject({ status: 404, code: "not_found" });
  });

  it("streams the backlog after ?after=, then a send: the user block, the reply, an update, and status frames", async () => {
    const api = mock();
    const a = await api.agents.get("terrarium/lead");
    const ses = (await api.agents.sessions(a.id)).items.find((s) => s.current)!;
    const n = ses.blocks!;
    const frames: TranscriptFrame[] = [];
    const s = api.agents.transcriptStream(a.id, ses.id, { after: n - 2 });
    s.subscribe((f) => frames.push(f));
    await wait();
    expect(frames.map((f) => f.type === "block" && f.block.seq)).toEqual([n - 1, n]);
    frames.length = 0;
    await api.agents.send(a.id, "Run the tests, please.");
    await wait(40);
    const blocks = frames.filter((f): f is Extract<TranscriptFrame, { type: "block" }> => f.type === "block").map((f) => f.block);
    expect(blocks[0]).toMatchObject({ seq: n + 1, role: "user", kind: "text", text: "[User] Run the tests, please.", origin: "operator", parts: [{ source: "user", body: "Run the tests, please." }], session: ses.id });
    expect(blocks.map((b) => b.kind)).toEqual(["text", "thinking", "tool_call", "tool_result", "text"]);
    expect(blocks.map((b) => b.seq)).toEqual([n + 1, n + 2, n + 3, n + 4, n + 5]);
    for (const b of blocks) checkBlock(b, "claude");
    const update = frames.find((f): f is Extract<TranscriptFrame, { type: "update" }> => f.type === "update")!;
    expect(update.block.seq).toBe(n + 5);
    expect(update.block.text!.length).toBeGreaterThan(blocks[4]!.text!.length);
    expect(update.block.text).toContain("Run the tests, please.");
    const statuses = frames.filter((f) => f.type === "status").map((f) => (f.type === "status" ? f.status : ""));
    expect(statuses).toEqual(["working", "idle"]);
    // The page now has them too.
    const page = await api.agents.transcript(a.id, ses.id, { after: n });
    expect(page.blocks.map((b) => b.seq)).toEqual([n + 1, n + 2, n + 3, n + 4, n + 5]);
    expect(page.blocks[4]!.text).toBe(update.block.text);
    s.close();
  });

  it("queues a turn to a working agent until it is idle", async () => {
    const api = mock();
    const a = await api.agents.get("terrarium/runner-dev");
    const ses = (await api.agents.sessions(a.id)).items.find((s) => s.current)!;
    const frames: TranscriptFrame[] = [];
    const s = api.agents.transcriptStream(a.id, ses.id, { after: ses.blocks });
    s.subscribe((f) => frames.push(f));
    await api.agents.send(a.id, "after this turn");
    await wait(20);
    expect(frames.filter((f) => f.type === "block")).toEqual([]);
    s.close();
  });

  it("ends the stream with status ended when the agent stops", async () => {
    const api = mock();
    const a = await api.agents.get("terrarium/lead");
    const ses = (await api.agents.sessions(a.id)).items.find((s) => s.current)!;
    const frames: TranscriptFrame[] = [];
    const states: TranscriptStreamState[] = [];
    const s = api.agents.transcriptStream(a.id, ses.id, { after: ses.blocks });
    s.subscribe((f) => frames.push(f));
    s.onState((st) => states.push(st));
    await wait();
    await api.agents.stop(a.id);
    await wait(20);
    expect(frames).toContainEqual({ type: "status", status: "ended" });
    const after = (await api.agents.sessions(a.id)).items.find((x) => x.id === ses.id)!;
    expect(after).toMatchObject({ active: false, current: false });
    s.close();
  });

  it("a restart opens a new active session with its own transcript", async () => {
    const api = mock();
    const a = await api.agents.get("terrarium/lead");
    await api.agents.restart(a.id);
    await wait(20);
    const list = (await api.agents.sessions(a.id)).items;
    expect(list[0]).toMatchObject({ current: true, active: true, blocks: 1, title: null });
    const page = await api.agents.transcript(a.id, list[0]!.id);
    expect(page.blocks[0]).toMatchObject({ seq: 1, role: "system", kind: "system" });
    expect(list.filter((s) => s.active)).toHaveLength(1);
  });

  it("refuses the stream for an unknown session (404) and without credentials (401), with no retry", async () => {
    const api = mock();
    const a = await api.agents.get("terrarium/lead");
    const seen: [TranscriptStreamState, unknown][] = [];
    api.agents.transcriptStream(a.id, "nope").onState((st, e) => seen.push([st, e]));
    await wait();
    expect(seen.at(-1)![0]).toBe("failed");
    expect(seen.at(-1)![1]).toMatchObject({ status: 404, code: "not_found" });

    const browser = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, client: { token: () => null, csrf: () => null } });
    const denied: [TranscriptStreamState, unknown][] = [];
    browser.agents.transcriptStream(a.id, "x").onState((st, e) => denied.push([st, e]));
    await wait();
    expect(denied.at(-1)![0]).toBe("failed");
    expect(denied.at(-1)![1]).toMatchObject({ status: 401 });
    await expect(browser.agents.transcript(a.id, "x")).rejects.toMatchObject({ status: 401 });
  });
});

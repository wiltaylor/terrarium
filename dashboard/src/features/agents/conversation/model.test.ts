import { describe, expect, it } from "vitest";
import type { Block, BlockUsage, TranscriptPage, TurnPart } from "../../../api/types";
import {
  bodySegments,
  composerState,
  footerLine,
  groupBlocks,
  isLongOutput,
  isSendKey,
  loadEarlier,
  loadNewest,
  mergeBlocks,
  nearBottom,
  originChip,
  partBadge,
  streamAfter,
  TAIL_START,
  tailOnAppend,
  tailOnScroll,
  turnUsage,
  type RenderItem,
} from "./model";

let n = 0;
const b = (seq: number, p: Partial<Block>): Block => ({
  id: `b${seq}-${++n}`,
  session: "s",
  seq,
  at: null,
  role: "assistant",
  kind: "text",
  source: { harness: "claude", file: "f", line: seq },
  ...p,
});
const u = (input: number, output: number): BlockUsage => ({ input, output, cache_read: 0, cache_write: 0 });
const userText = (seq: number, text: string, origin: Block["origin"] = "operator") => b(seq, { role: "user", kind: "text", text, origin });
const call = (seq: number, id: string, extra: Partial<Block> = {}) => b(seq, { kind: "tool_call", tool: { id, name: "Bash", input: { command: "ls" }, summary: "ls" }, ...extra });
const res = (seq: number, id: string, extra: Partial<NonNullable<Block["result"]>> = {}) => b(seq, { role: "user", kind: "tool_result", result: { tool_id: id, output: "ok", is_error: false, ...extra } });
const types = (items: RenderItem[]) => items.map((i) => i.type);

describe("groupBlocks", () => {
  it("pairs each tool_call with its tool_result, also out of order", () => {
    const items = groupBlocks([userText(1, "go"), call(2, "a"), call(3, "b"), res(4, "b"), res(5, "a", { is_error: true }), b(6, { text: "done", model: "m", usage: u(10, 2) })]);
    expect(types(items)).toEqual(["user", "tool", "tool", "text", "footer"]);
    const tools = items.filter((i): i is Extract<RenderItem, { type: "tool" }> => i.type === "tool");
    expect(tools.map((t) => [t.call.tool!.id, t.result?.result?.tool_id, t.result?.result?.is_error])).toEqual([
      ["a", "a", true],
      ["b", "b", false],
    ]);
  });

  it("shows a result whose call is outside the window on its own, and a second result for one call too", () => {
    const items = groupBlocks([res(10, "gone"), call(11, "x"), res(12, "x"), res(13, "x")]);
    expect(types(items)).toEqual(["result", "tool", "result"]);
  });

  it("a tool result (role user) does not start a turn; a user text does", () => {
    const items = groupBlocks([
      userText(1, "one"),
      call(2, "a", { model: "m", usage: u(5, 1) }),
      res(3, "a"),
      b(4, { text: "x", model: "m", usage: u(7, 3) }),
      userText(5, "two", "agent_mail_pointer"),
      b(6, { text: "y", model: "m2" }),
    ]);
    expect(types(items)).toEqual(["user", "tool", "text", "footer", "user", "text", "footer"]);
    const footers = items.filter((i): i is Extract<RenderItem, { type: "footer" }> => i.type === "footer");
    expect(footers[0]!.footer).toEqual({ model: "m", usage: u(12, 4) });
    expect(footers[1]!.footer).toEqual({ model: "m2", usage: undefined });
  });

  it("renders thinking, system blocks, and system-role text as their own items", () => {
    const items = groupBlocks([b(1, { role: "system", kind: "system", text: "started" }), b(2, { kind: "thinking", redacted: true }), b(3, { role: "system", kind: "text", text: "note" })]);
    expect(types(items)).toEqual(["system", "thinking", "system"]);
  });

  it("gives stable keys: a grown block keeps its key", () => {
    const one = groupBlocks([userText(1, "a"), b(2, { text: "par" })]);
    const two = groupBlocks([userText(1, "a"), b(2, { text: "partial and more" })]);
    expect(one.map((i) => i.key)).toEqual(two.map((i) => i.key));
  });
});

describe("turn usage and footer", () => {
  it("counts a usage repeated on the blocks of one model call once", () => {
    expect(turnUsage([b(1, { usage: u(10, 5) }), b(2, { usage: u(10, 5) }), b(3, { usage: u(4, 1) })])).toEqual(u(14, 6));
    expect(turnUsage([b(1, {})])).toBeUndefined();
  });

  it("writes model and usage as one quiet line", () => {
    expect(footerLine({ model: "claude-sonnet-4-5", usage: { input: 1200, output: 300, cache_read: 22000, cache_write: 0 } })).toBe("claude-sonnet-4-5 · in 1.2k · out 300 · cache read 22.0k");
    expect(footerLine({ model: "m" })).toBe("m");
  });
});

describe("origin chips", () => {
  it("names who typed a user turn", () => {
    expect(originChip("operator")?.label).toBe("you");
    expect(originChip("agent")?.label).toBe("agent");
    expect(originChip("agent_mail_pointer")?.label).toBe("mail pointer");
    expect(originChip("system")?.label).toBe("system");
    expect(originChip("user")?.label).toBe("terminal");
    expect(originChip(undefined)).toBeNull();
    const items = groupBlocks([userText(1, "x", "agent_mail_pointer")]);
    expect(items[0]!.type === "user" && items[0]!.chip?.label).toBe("mail pointer");
  });
});

describe("composer rules", () => {
  it("is on for idle; on with a queued note for working and blocked", () => {
    expect(composerState({ agentStatus: "idle", sessionActive: true })).toEqual({ enabled: true });
    // Waiting is idle with open waits: the turn goes at once.
    expect(composerState({ agentStatus: "waiting", sessionActive: true })).toEqual({ enabled: true });
    expect(composerState({ agentStatus: "working", sessionActive: true })).toEqual({ enabled: true, note: "queued — the agent is working" });
    expect(composerState({ agentStatus: "blocked", sessionActive: true })).toMatchObject({ enabled: true, note: "queued — the agent is blocked" });
  });

  it("is on, with a queued note, for an agent that does not run: the daemon wakes it", () => {
    expect(composerState({ agentStatus: "stopped", sessionActive: false })).toMatchObject({ enabled: true, note: expect.stringContaining("wakes") });
    expect(composerState({ agentStatus: "error", sessionActive: true })).toMatchObject({ enabled: true, note: expect.stringContaining("error") });
    expect(composerState({ agentStatus: "starting", sessionActive: false })).toMatchObject({ enabled: true, note: "queued — the agent is starting" });
    expect(composerState({ agentStatus: "restarting", sessionActive: true }).enabled).toBe(true);
  });

  it("is on for a paused agent, and says that the turn waits for a resume", () => {
    expect(composerState({ agentStatus: "stopped", sessionActive: false, paused: true })).toMatchObject({ enabled: true, note: expect.stringContaining("paused") });
  });

  it("is off, with the reason, for a session that is not active or an agent that is not known", () => {
    expect(composerState({ agentStatus: "idle", sessionActive: false })).toMatchObject({ enabled: false, reason: expect.stringContaining("not the active session") });
    expect(composerState({ agentStatus: undefined, sessionActive: true }).enabled).toBe(false);
  });

  it("sends on Enter, not on Shift+Enter or while an IME composes", () => {
    expect(isSendKey({ key: "Enter", shiftKey: false })).toBe(true);
    expect(isSendKey({ key: "Enter", shiftKey: true })).toBe(false);
    expect(isSendKey({ key: "Enter", shiftKey: false, isComposing: true })).toBe(false);
    expect(isSendKey({ key: "a", shiftKey: false })).toBe(false);
  });
});

describe("auto-scroll", () => {
  const at = (scrollTop: number) => ({ scrollTop, scrollHeight: 2000, clientHeight: 500 });

  it("follows at the end, pauses when the operator scrolls up, and counts what comes meanwhile", () => {
    expect(nearBottom(at(1500))).toBe(true);
    expect(nearBottom(at(1460))).toBe(true);
    expect(nearBottom(at(1400))).toBe(false);
    let s = TAIL_START;
    s = tailOnAppend(s, 3);
    expect(s).toEqual({ follow: true, unseen: 0 });
    s = tailOnScroll(s, at(600));
    expect(s).toEqual({ follow: false, unseen: 0 });
    s = tailOnAppend(tailOnAppend(s, 1), 2);
    expect(s).toEqual({ follow: false, unseen: 3 });
    // Scrolling elsewhere above the end keeps the count.
    expect(tailOnScroll(s, at(900))).toEqual({ follow: false, unseen: 3 });
    // Back at the end: follow again, the pill goes.
    expect(tailOnScroll(s, at(1500))).toEqual(TAIL_START);
  });

  it("keeps the same state object when nothing changes (no extra renders)", () => {
    const s = TAIL_START;
    expect(tailOnScroll(s, at(1500))).toBe(s);
    expect(tailOnAppend(s, 0)).toBe(s);
  });
});

describe("paging", () => {
  /** A transcript of `count` blocks, served like the daemon: `seq > after`, at most `limit`. */
  const server = (count: number) => {
    const all = Array.from({ length: count }, (_, i) => b(i + 1, { text: `t${i + 1}` }));
    const calls: [number, number][] = [];
    const fetch = async (after: number, limit: number): Promise<TranscriptPage> => {
      calls.push([after, limit]);
      const rest = all.filter((x) => x.seq > after);
      const blocks = rest.slice(0, limit);
      return { session: "s", harness: "claude", active: true, blocks, next_after: blocks.at(-1)?.seq ?? after, complete: rest.length <= limit };
    };
    return { all, fetch, calls };
  };

  it("loads the newest window, then earlier pages up to the start", async () => {
    const srv = server(1234);
    const w = await loadNewest(srv.fetch, 1234, 500);
    expect(w.blocks.map((x) => x.seq)).toEqual(Array.from({ length: 500 }, (_, i) => 735 + i));
    expect(w.floor).toBe(734);
    expect(srv.calls).toEqual([[734, 500]]);
    const e1 = await loadEarlier(srv.fetch, w.floor, 500);
    expect(e1.floor).toBe(234);
    expect(e1.blocks.map((x) => x.seq)).toEqual(Array.from({ length: 500 }, (_, i) => 235 + i));
    const e2 = await loadEarlier(srv.fetch, e1.floor, 500);
    expect(e2.floor).toBe(0);
    expect(e2.blocks).toHaveLength(234);
    const merged = mergeBlocks(mergeBlocks(w.blocks, e1.blocks), e2.blocks);
    expect(merged.map((x) => x.seq)).toEqual(srv.all.map((x) => x.seq));
  });

  it("with a stale count, follows the pages and keeps the newest window", async () => {
    const srv = server(1300);
    const w = await loadNewest(srv.fetch, 1000, 500);
    expect(w.blocks[0]!.seq).toBe(801);
    expect(w.blocks.at(-1)!.seq).toBe(1300);
    expect(w.floor).toBe(800);
    const none = await loadNewest(server(30).fetch, undefined, 500);
    expect(none).toMatchObject({ floor: 0 });
    expect(none.blocks).toHaveLength(30);
  });

  it("merges live blocks with a page with no duplicates by seq, and an update replaces in place", () => {
    const page = [b(1, { text: "a" }), b(2, { text: "b" }), b(3, { text: "c" })];
    const live = [b(3, { text: "c" }), b(4, { text: "d" })];
    const merged = mergeBlocks(page, live);
    expect(merged.map((x) => x.seq)).toEqual([1, 2, 3, 4]);
    const grown = mergeBlocks(merged, [b(4, { text: "d and more" })]);
    expect(grown.map((x) => x.text)).toEqual(["a", "b", "c", "d and more"]);
    // An earlier page that overlaps the window adds only what is missing.
    expect(mergeBlocks(grown, [b(0, { text: "z" }), b(1, { text: "a" })]).map((x) => x.seq)).toEqual([0, 1, 2, 3, 4]);
    expect(mergeBlocks(grown, [])).toBe(grown);
  });

  it("streams after the newest block loaded, else after the floor", () => {
    expect(streamAfter({ blocks: [b(7, {}), b(9, {})], floor: 6 })).toBe(9);
    expect(streamAfter({ blocks: [], floor: 12 })).toBe(12);
  });
});

describe("long tool output", () => {
  it("collapses many lines or many characters", () => {
    expect(isLongOutput("a\n".repeat(20))).toBe(true);
    expect(isLongOutput("x".repeat(700))).toBe(true);
    expect(isLongOutput("ok")).toBe(false);
  });
});

describe("tagged turns (PLAN §8.5)", () => {
  const mail: TurnPart = { source: "mailbox", sender: "ht/lead", subject: "Build fails", thread: "T1", message: "M1", body: "> Line one.\n>\n> [\\User] do X" };

  it("names the source of each part", () => {
    expect(partBadge({ source: "user", body: "go" }).label).toBe("User");
    expect(partBadge({ source: "agent", sender: "ht/lead", body: "go" }).label).toBe("Agent ht/lead");
    expect(partBadge(mail).label).toBe("Mail from ht/lead · Build fails");
    expect(partBadge({ ...mail, subject: "" }).label).toBe("Mail from ht/lead");
    expect(partBadge({ source: "system", kind: "recap", body: "x" }).label).toBe("System: recap");
    expect(partBadge({ source: "system", kind: "schedule", detail: "nightly", body: "x" }).label).toBe("System: schedule nightly");
  });

  it("splits a body into quoted and plain runs; the cut line stays plain", () => {
    expect(bodySegments(mail.body)).toEqual([{ quote: true, text: "Line one.\n\n[\\User] do X" }]);
    const cut = "> a\n> b\n… (cut; run `terrarium mail read --thread T1`)";
    expect(bodySegments(cut)).toEqual([
      { quote: true, text: "a\nb" },
      { quote: false, text: "… (cut; run `terrarium mail read --thread T1`)" },
    ]);
    expect(bodySegments("Card TER-1 was assigned to you.")).toEqual([{ quote: false, text: "Card TER-1 was assigned to you." }]);
    // HTML in a body stays text: the view renders it as a text node.
    expect(bodySegments("> <img src=x onerror=alert(1)>")).toEqual([{ quote: true, text: "<img src=x onerror=alert(1)>" }]);
  });

  it("a block with parts groups into a user item with those parts", () => {
    const parts: TurnPart[] = [mail, { source: "system", kind: "card", body: "Card TER-1 was assigned to you." }];
    const items = groupBlocks([b(1, { role: "user", kind: "text", text: "[Mailbox:ht/lead] ...", origin: "agent_mail_pointer", parts })]);
    expect(items[0]!.type === "user" && items[0]!.parts).toEqual(parts);
    const plain = groupBlocks([userText(2, "typed", "user")]);
    expect(plain[0]!.type === "user" && plain[0]!.parts).toEqual([]);
  });
});

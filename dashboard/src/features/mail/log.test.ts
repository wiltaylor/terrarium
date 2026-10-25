import { describe, expect, it } from "vitest";
import type { Message } from "../../api/types";
import {
  annotate,
  applyFilter,
  attentionReasons,
  counterpart,
  DEFAULT_FILTER,
  filterFromParams,
  filterToParams,
  firstLine,
  heldIds,
  matchesKind,
  mergeMessages,
  needsAttention,
  newestId,
  noticeOf,
  projectsOf,
  serverQuery,
  threadOf,
  type LogFilter,
} from "./log";

const NOW = Date.parse("2026-10-25T12:00:00Z");
const at = (minAgo: number) => new Date(NOW - minAgo * 60_000).toISOString();

type Side = "operator" | "system" | `project:${string}` | `${string}/${string}`;
const addr = (s: Side) =>
  s === "operator" || s === "system"
    ? { key: s, kind: s }
    : s.startsWith("project:")
      ? { key: s, kind: "project" as const }
      : { key: `ULID-${s}`, kind: "agent" as const };

let n = 0;
function msg(from: Side, to: Side, opts: Partial<Message> & { min?: number } = {}): Message {
  const f = addr(from);
  const t = addr(to);
  const { min = 10, ...rest } = opts;
  n++;
  return {
    id: `01M${String(n).padStart(6, "0")}`,
    thread_id: "T1",
    from: f.key,
    from_kind: f.kind,
    from_name: from,
    to: t.key,
    to_kind: t.kind as Message["to_kind"],
    to_name: to,
    subject: "hello",
    body: "line one\nline two",
    state: "delivered",
    created_at: at(min),
    ...rest,
  };
}

const filter = (f: Partial<LogFilter> = {}): LogFilter => ({ ...DEFAULT_FILTER, ...f });

describe("mail log: classification", () => {
  it("matches each direction, and a message can match more than one", () => {
    const sys = msg("system", "operator");
    expect(matchesKind(sys, "system")).toBe(true);
    expect(matchesKind(sys, "to_operator")).toBe(true);
    expect(matchesKind(sys, "agents")).toBe(false);
    const a2a = msg("terrarium/lead", "terrarium/dev");
    expect(matchesKind(a2a, "agents")).toBe(true);
    expect(matchesKind(a2a, "inbox")).toBe(false);
    expect(matchesKind(msg("project:ops", "terrarium/dev"), "inbox")).toBe(true);
    expect(matchesKind(msg("terrarium/dev", "project:ops"), "inbox")).toBe(true);
    expect(matchesKind(msg("operator", "terrarium/dev"), "from_operator")).toBe(true);
    expect(matchesKind(a2a, "")).toBe(true);
  });

  it("names the daemon's notices by their subjects", () => {
    expect(noticeOf(msg("system", "operator", { subject: "Thread 01ABC needs a human" }))).toBe("needs_human");
    expect(noticeOf(msg("system", "operator", { subject: "TER-15 blocked: Codex adapter" }))).toBe("blocked");
    expect(noticeOf(msg("system", "operator", { subject: "Quota: max 5h window at 85%" }))).toBe("quota");
    expect(noticeOf(msg("system", "operator", { subject: "Budget: project:ops day at 100%" }))).toBe("quota");
    expect(noticeOf(msg("system", "operator", { subject: "Login expired: max" }))).toBe("login");
    expect(noticeOf(msg("system", "operator", { subject: "Something else" }))).toBe("notice");
    // Only `system` sends notices.
    expect(noticeOf(msg("terrarium/lead", "operator", { subject: "TER-15 blocked: x" }))).toBeNull();
  });

  it("finds the projects from the addresses, a card prefix, or a budget scope", () => {
    expect(projectsOf(msg("terrarium/lead", "project:ops"))).toEqual(["ops", "terrarium"]);
    expect(projectsOf(msg("system", "operator", { subject: "TER-15 blocked: x" }), { TER: "terrarium" })).toEqual(["terrarium"]);
    expect(projectsOf(msg("system", "operator", { subject: "Budget: project:ops/max day at 90%" }))).toEqual(["ops"]);
    expect(projectsOf(msg("system", "operator", { subject: "Quota: max 5h window at 85%" }))).toEqual([]);
  });

  it("takes the first non-empty line and the other party", () => {
    expect(firstLine("\n  \nfirst\nsecond")).toBe("first");
    expect(firstLine("")).toBe("");
    expect(counterpart(msg("terrarium/lead", "operator"))).toBe("terrarium/lead");
    expect(counterpart(msg("operator", "project:ops"))).toBe("project:ops");
    expect(counterpart(msg("system", "operator"))).toBeUndefined();
  });
});

describe("mail log: the loop guard", () => {
  /** Seven agent replies, the daemon's notice, then one more held reply. */
  const loop = () => {
    const out: Message[] = [];
    for (let i = 0; i < 7; i++) out.push(msg(i % 2 ? "t/dev" : "t/lead", i % 2 ? "t/lead" : "t/dev", { thread_id: "L", min: 40 - i, thread_paused: true }));
    out.push(msg("system", "operator", { thread_id: "L", min: 34, subject: "Thread L needs a human", thread_paused: true }));
    out.push(msg("t/dev", "t/lead", { thread_id: "L", min: 33, thread_paused: true }));
    return out;
  };

  it("holds the message before the notice and every agent message after it", () => {
    const l = loop();
    expect([...heldIds(l)].sort()).toEqual([l[6]!.id, l[8]!.id].sort());
  });

  it("stops holding after an operator message, and takes held IDs from events", () => {
    const l = loop();
    const op = msg("operator", "t/dev", { thread_id: "L", min: 32 });
    const after = msg("t/dev", "t/lead", { thread_id: "L", min: 31 });
    const held = heldIds([...l, op, after]);
    expect(held.has(after.id)).toBe(false);
    expect(held.has(l[8]!.id)).toBe(true);
    // A live `mail` event with `held: true` counts, when the message is in the list.
    expect(heldIds([after], new Set([after.id, "not-loaded"]))).toEqual(new Set([after.id]));
  });

  it("sorts a notice after the agent message of the same instant", () => {
    const a = msg("t/lead", "t/dev", { thread_id: "S", min: 5, id: "01MZZZZ" });
    const notice = msg("system", "operator", { thread_id: "S", min: 5, id: "01MAAAA", subject: "Thread S needs a human" });
    expect(heldIds([notice, a])).toEqual(new Set([a.id]));
  });
});

describe("mail log: the attention rule", () => {
  it("flags unread mail to the operator, with the kind of a system notice", () => {
    expect(attentionReasons(msg("t/lead", "operator"), false, null, false)).toEqual(["unread"]);
    expect(attentionReasons(msg("t/lead", "operator", { state: "read" }), false, null, false)).toEqual([]);
    expect(attentionReasons(msg("t/lead", "operator", { state: "replied" }), false, null, false)).toEqual([]);
    expect(attentionReasons(msg("system", "operator"), false, "blocked", false)).toEqual(["unread", "blocked"]);
    expect(attentionReasons(msg("system", "operator"), false, "quota", false)).toEqual(["unread", "quota"]);
    expect(attentionReasons(msg("system", "operator"), false, "needs_human", true)).toEqual(["unread", "needs_human"]);
    // A login notice is unread mail, with no extra reason.
    expect(attentionReasons(msg("system", "operator"), false, "login", false)).toEqual(["unread"]);
    // A blocked notice to an agent is not the operator's.
    expect(attentionReasons(msg("system", "t/lead"), false, "blocked", false)).toEqual([]);
  });

  it("flags held mail and the newest message of a paused thread while the pause lasts", () => {
    expect(attentionReasons(msg("t/a", "t/b", { thread_paused: true, state: "read" }), true, null, false)).toEqual(["held"]);
    expect(attentionReasons(msg("t/a", "t/b", { thread_paused: true, state: "read" }), false, null, true)).toEqual(["paused"]);
    expect(attentionReasons(msg("t/a", "t/b", { thread_paused: true, state: "read" }), false, null, false)).toEqual([]);
    // After the operator replies, the thread is not paused: nothing is left.
    expect(attentionReasons(msg("t/a", "t/b", { state: "read" }), true, null, true)).toEqual([]);
  });

  it("annotates newest first and leaves read, unpaused mail out of the attention view", () => {
    const quiet = msg("t/a", "t/b", { thread_id: "Q", min: 1, state: "read" });
    const toMe = msg("t/a", "operator", { thread_id: "M", min: 3 });
    const read = msg("t/a", "operator", { thread_id: "R", min: 2, state: "read", read_by: "operator", read_by_name: "operator" });
    const entries = annotate([toMe, read, quiet]);
    expect(entries.map((e) => e.message.id)).toEqual([quiet.id, read.id, toMe.id]);
    expect(entries.filter(needsAttention).map((e) => e.message.id)).toEqual([toMe.id]);
    expect(applyFilter(entries, filter(), NOW).map((e) => e.message.id)).toEqual([toMe.id]);
    expect(applyFilter(entries, filter({ attention: false }), NOW)).toHaveLength(3);
  });
});

describe("mail log: filters", () => {
  const list = [
    msg("ops/lead", "project:terrarium", { id: "01MA", thread_id: "A", min: 30, subject: "bug: card ls", state: "read" }),
    msg("terrarium/lead", "terrarium/dev", { id: "01MB", thread_id: "B", min: 120, subject: "Start TER-2", state: "read" }),
    msg("system", "operator", { id: "01MC", thread_id: "C", min: 60 * 30, subject: "TER-1 blocked: parser", state: "read" }),
    msg("operator", "terrarium/dev", { id: "01MD", thread_id: "D", min: 60 * 24 * 10, subject: "Status", body: "How is the PARSER going?", state: "read" }),
  ];
  const entries = annotate(list, { prefixes: { TER: "terrarium" } });
  const ids = (f: Partial<LogFilter>) => applyFilter(entries, filter({ attention: false, ...f }), NOW).map((e) => e.message.id);

  it("filters by direction, project, text, and time range, newest first", () => {
    expect(ids({})).toEqual(["01MA", "01MB", "01MC", "01MD"]);
    expect(ids({ kind: "inbox" })).toEqual(["01MA"]);
    expect(ids({ kind: "agents" })).toEqual(["01MB"]);
    expect(ids({ kind: "system" })).toEqual(["01MC"]);
    expect(ids({ kind: "from_operator" })).toEqual(["01MD"]);
    expect(ids({ project: "ops" })).toEqual(["01MA"]);
    expect(ids({ project: "terrarium" })).toEqual(["01MA", "01MB", "01MC", "01MD"]);
    expect(ids({ q: "parser" })).toEqual(["01MC", "01MD"]);
    expect(ids({ q: "  " })).toHaveLength(4);
    expect(ids({ range: "hour" })).toEqual(["01MA"]);
    expect(ids({ range: "day" })).toEqual(["01MA", "01MB"]);
    expect(ids({ range: "week" })).toEqual(["01MA", "01MB", "01MC"]);
    expect(ids({ project: "terrarium", kind: "agents", range: "day", q: "start" })).toEqual(["01MB"]);
  });

  it("sends the daemon only the filters it has", () => {
    expect(serverQuery(filter({ kind: "to_operator" }))).toEqual({ to: "operator" });
    expect(serverQuery(filter({ kind: "from_operator" }))).toEqual({ from: "operator" });
    expect(serverQuery(filter({ kind: "from_operator", project: "x", q: "y" }))).toEqual({ from: "operator" });
    // The daemon answers `from=system` with 404 (resolve does not know it).
    expect(serverQuery(filter({ kind: "system" }))).toEqual({});
    expect(serverQuery(filter({ kind: "agents" }))).toEqual({});
    expect(serverQuery(filter({ kind: "inbox", project: "ops" }))).toEqual({});
    expect(serverQuery(filter())).toEqual({});
  });

  it("keeps the filter in the URL, with defaults left out and bad values dropped", () => {
    expect(filterFromParams({})).toEqual(DEFAULT_FILTER);
    expect(DEFAULT_FILTER.attention).toBe(true);
    const f = filter({ project: "ops", kind: "agents", q: "x", range: "day", attention: false });
    expect(filterToParams(f)).toEqual({ project: "ops", kind: "agents", q: "x", range: "day", attention: "0" });
    expect(filterFromParams(filterToParams(f))).toEqual(f);
    expect(filterToParams(DEFAULT_FILTER)).toEqual({ project: undefined, kind: undefined, q: undefined, range: undefined, attention: undefined });
    expect(filterFromParams({ kind: "nope", range: "year" })).toEqual(DEFAULT_FILTER);
  });
});

describe("mail log: ordering and live insert", () => {
  it("merges new and changed messages newest first, the newer copy winning", () => {
    const a = msg("t/a", "operator", { id: "01MA", min: 3 });
    const b = msg("t/a", "operator", { id: "01MB", min: 2 });
    const c = msg("t/a", "operator", { id: "01MC", min: 1 });
    const list = mergeMessages([], [a, b]);
    expect(list.map((m) => m.id)).toEqual(["01MB", "01MA"]);
    const next = mergeMessages(list, [c, { ...a, state: "read" }]);
    expect(next.map((m) => m.id)).toEqual(["01MC", "01MB", "01MA"]);
    expect(next.find((m) => m.id === "01MA")!.state).toBe("read");
    // Same instant: the bigger ID first.
    const d = msg("t/a", "operator", { id: "01MD", min: 1 });
    expect(mergeMessages(next, [d]).slice(0, 2).map((m) => m.id)).toEqual(["01MD", "01MC"]);
    expect(newestId(next)).toBe("01MC");
    expect(newestId([])).toBeUndefined();
  });

  it("lists a thread oldest first", () => {
    const x = msg("t/a", "t/b", { id: "01MX", thread_id: "X", min: 1 });
    const y = msg("t/b", "t/a", { id: "01MY", thread_id: "X", min: 5 });
    const z = msg("t/b", "t/a", { id: "01MZ", thread_id: "Z", min: 3 });
    expect(threadOf([x, z, y], "X").map((m) => m.id)).toEqual(["01MY", "01MX"]);
  });
});

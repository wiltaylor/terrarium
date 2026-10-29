import { describe, expect, it } from "vitest";
import type { Message } from "../../api/types";
import {
  applyFilter,
  counterpart,
  DEFAULT_FILTER,
  filterFromParams,
  filterToParams,
  firstLine,
  mergeMessages,
  namesOf,
  newestId,
  openRowOf,
  openThreadOf,
  projectsOf,
  serverQuery,
  setTail,
  shortThread,
  TAIL_START,
  tailEvent,
  threadOf,
  windowOf,
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

describe("mail log: parts of a message", () => {
  it("finds the projects from the addresses, a card prefix, or a budget scope", () => {
    expect(projectsOf(msg("terrarium/lead", "project:ops"))).toEqual(["ops", "terrarium"]);
    expect(projectsOf(msg("system", "operator", { subject: "TER-15 blocked: x" }), { TER: "terrarium" })).toEqual(["terrarium"]);
    expect(projectsOf(msg("system", "operator", { subject: "Budget: project:ops/max day at 90%" }))).toEqual(["ops"]);
    expect(projectsOf(msg("system", "operator", { subject: "Quota: max 5h window at 85%" }))).toEqual([]);
  });

  it("takes the first non-empty line, the other party, the names, and a short thread", () => {
    expect(firstLine("\n  \nfirst\nsecond")).toBe("first");
    expect(firstLine("")).toBe("");
    expect(counterpart(msg("terrarium/lead", "operator"))).toBe("terrarium/lead");
    expect(counterpart(msg("operator", "project:ops"))).toBe("project:ops");
    expect(counterpart(msg("system", "operator"))).toBeUndefined();
    expect(namesOf([msg("t/b", "operator"), msg("operator", "t/a")])).toEqual(["operator", "t/a", "t/b"]);
    expect(shortThread("01JBQTHR000000000000000007")).toBe("…000007");
    expect(shortThread("T1")).toBe("T1");
  });
});

describe("mail log: the open row", () => {
  // Mail to the operator, then the operator's reply on the same thread: the newest row is the reply.
  const ask = msg("svc/lead", "operator", { min: 30, subject: "SVC-2 is ready to merge" });
  const answer = msg("operator", "svc/lead", { min: 20, subject: "Re: SVC-2 is ready to merge" });
  const other = msg("ops/meta", "operator", { min: 10, thread_id: "T2" });
  const rows = mergeMessages([], [ask, answer, other]);

  it("opens the clicked mail to the operator, not the newest row of its thread", () => {
    expect(openRowOf(rows, ask.id)).toBe(ask);
    expect(openRowOf(rows, answer.id)).toBe(answer);
    expect(openThreadOf(rows, ask.id)).toBe("T1");
  });

  it("opens the newest shown row of a linked thread, and nothing without `open`", () => {
    expect(openRowOf(rows, "T1")).toBe(answer);
    expect(openRowOf(rows.filter((m) => m !== answer), "T1")).toBe(ask);
    expect(openThreadOf(rows, "T1")).toBe("T1");
    expect(openRowOf(rows, undefined)).toBeUndefined();
    expect(openThreadOf(rows, undefined)).toBeUndefined();
  });
});

describe("mail log: filters", () => {
  const list = [
    msg("ops/lead", "project:terrarium", { id: "01MA", thread_id: "A", min: 30, subject: "bug: card ls" }),
    msg("terrarium/lead", "terrarium/dev", { id: "01MB", thread_id: "B", min: 120, subject: "Start TER-2" }),
    msg("system", "operator", { id: "01MC", thread_id: "C", min: 60 * 30, subject: "TER-1 blocked: parser" }),
    msg("operator", "terrarium/dev", { id: "01MD", thread_id: "D", min: 60 * 24 * 10, subject: "Status", body: "How is the PARSER going?" }),
  ];
  const ids = (f: Partial<LogFilter>) => applyFilter(list, filter(f), NOW, { TER: "terrarium" }).map((m) => m.id);

  it("filters by from, to, project, thread, text, and time range", () => {
    expect(ids({})).toEqual(["01MA", "01MB", "01MC", "01MD"]);
    expect(ids({ from: "LEAD" })).toEqual(["01MA", "01MB"]);
    expect(ids({ from: "system" })).toEqual(["01MC"]);
    expect(ids({ to: "terrarium/dev" })).toEqual(["01MB", "01MD"]);
    expect(ids({ to: "project:" })).toEqual(["01MA"]);
    // The stored address matches too: an agent ULID.
    expect(ids({ to: "ULID-terrarium/dev" })).toEqual(["01MB", "01MD"]);
    expect(ids({ project: "ops" })).toEqual(["01MA"]);
    expect(ids({ project: "terrarium" })).toEqual(["01MA", "01MB", "01MC", "01MD"]);
    expect(ids({ thread: "C" })).toEqual(["01MC"]);
    expect(ids({ q: "parser" })).toEqual(["01MC", "01MD"]);
    expect(ids({ q: "  " })).toHaveLength(4);
    expect(ids({ range: "hour" })).toEqual(["01MA"]);
    expect(ids({ range: "day" })).toEqual(["01MA", "01MB"]);
    expect(ids({ range: "week" })).toEqual(["01MA", "01MB", "01MC"]);
    expect(ids({ project: "terrarium", from: "terrarium/", range: "day", q: "start" })).toEqual(["01MB"]);
  });

  it("filters by a custom time range, each end optional", () => {
    const iso = (min: number) => new Date(NOW - min * 60_000).toISOString();
    expect(ids({ range: "custom", since: iso(60 * 31), until: iso(60) })).toEqual(["01MB", "01MC"]);
    expect(ids({ range: "custom", until: iso(60 * 31) })).toEqual(["01MD"]);
    expect(ids({ range: "custom", since: "not a time" })).toHaveLength(4);
    expect(windowOf({ range: "all", since: "", until: "" }, NOW)).toEqual({});
    expect(windowOf({ range: "hour", since: "", until: "" }, NOW)).toEqual({ since: NOW - 3_600_000 });
  });

  it("sends the daemon only a whole thread ID", () => {
    const th = "01JBQTHR000000000000000007";
    expect(serverQuery(filter({ thread: th, from: "x", project: "y", q: "z" }))).toEqual({ thread: th });
    expect(serverQuery(filter({ thread: "01JBQTH" }))).toEqual({});
    expect(serverQuery(filter({ from: "operator", to: "operator" }))).toEqual({});
    expect(serverQuery(filter())).toEqual({});
  });

  it("keeps the filter in the URL, with defaults left out and bad values dropped", () => {
    expect(filterFromParams({})).toEqual(DEFAULT_FILTER);
    const f = filter({ from: "lead", to: "operator", project: "ops", thread: "T1", q: "x", range: "day" });
    expect(filterToParams(f)).toEqual({ from: "lead", to: "operator", project: "ops", thread: "T1", q: "x", range: "day", since: undefined, until: undefined });
    expect(filterFromParams(filterToParams(f))).toEqual(f);
    const c = filter({ range: "custom", since: "2026-10-01T09:00", until: "2026-10-02T09:00" });
    expect(filterFromParams(filterToParams(c))).toEqual(c);
    // The ends of a custom range go only with the custom range.
    expect(filterToParams({ ...c, range: "day" })).toMatchObject({ since: undefined, until: undefined });
    expect(filterFromParams({ range: "day", since: "2026-10-01T09:00" }).since).toBe("");
    expect(filterToParams(DEFAULT_FILTER)).toEqual({ from: undefined, to: undefined, project: undefined, thread: undefined, q: undefined, range: undefined, since: undefined, until: undefined });
    expect(filterFromParams({ range: "year" })).toEqual(DEFAULT_FILTER);
  });
});

describe("mail log: live tail", () => {
  it("pulls at once while live, and counts the events while it is off", () => {
    expect(tailEvent(TAIL_START, "T1")).toEqual({ state: TAIL_START, pull: true });
    let s = setTail(TAIL_START, false).state;
    expect(s).toEqual({ live: false, missed: 0, threads: [] });
    s = tailEvent(s, "T1").state;
    s = tailEvent(s, "T1").state;
    const r = tailEvent(s, undefined);
    expect(r.pull).toBe(false);
    expect(r.state).toEqual({ live: false, missed: 3, threads: ["T1"] });
    // On again: pull the touched threads, and start again from zero.
    expect(setTail(r.state, true)).toEqual({ state: TAIL_START, pull: ["T1"] });
    expect(setTail(TAIL_START, true)).toEqual({ state: TAIL_START, pull: null });
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

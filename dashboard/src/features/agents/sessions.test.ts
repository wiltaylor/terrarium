import { describe, expect, it } from "vitest";
import type { Session } from "../../api/types";
import { AGENT_TABS, agentViewQuery, currentSession, findGroup, groupChip, groupSessions, parseAgentView, sessionLabel, waitsTabLabel } from "./sessions";

const s = (id: string, over: Partial<Session> = {}): Session => ({
  id,
  agent: "A1",
  started_at: `2026-10-28T0${id.length}:00:00.000Z`,
  turns: 1,
  current: false,
  ...over,
});

describe("the agent page view in the URL", () => {
  it("defaults to the Conversation tab; a session opens only on the Sessions tab", () => {
    expect(parseAgentView({})).toEqual({ tab: "conversation", session: null });
    expect(parseAgentView({ tab: "bogus" })).toEqual({ tab: "conversation", session: null });
    expect(parseAgentView({ tab: "sessions", session: "01S" })).toEqual({ tab: "sessions", session: "01S" });
    expect(parseAgentView({ tab: "events", session: "01S" })).toEqual({ tab: "events", session: null });
    expect(parseAgentView({ tab: "sessions", session: "" })).toEqual({ tab: "sessions", session: null });
    // Issue #22: the Screen tab went; an old link opens the Terminal tab. The new tabs parse.
    expect(parseAgentView({ tab: "screen" })).toEqual({ tab: "terminal", session: null });
    expect(parseAgentView({ tab: "waits" })).toEqual({ tab: "waits", session: null });
    expect(parseAgentView({ tab: "details" })).toEqual({ tab: "details", session: null });
  });

  it("writes the query with the defaults left out, so a reload keeps the open session", () => {
    expect(agentViewQuery({ tab: "conversation", session: "x" })).toEqual({ tab: undefined, session: undefined });
    expect(agentViewQuery({ tab: "sessions", session: null })).toEqual({ tab: "sessions", session: undefined });
    expect(agentViewQuery({ tab: "sessions", session: "01S" })).toEqual({ tab: "sessions", session: "01S" });
    expect(agentViewQuery({ tab: "terminal", session: "01S" })).toEqual({ tab: "terminal", session: undefined });
    const v = { tab: "sessions" as const, session: "01S" };
    expect(parseAgentView(agentViewQuery(v))).toEqual(v);
  });
});

describe("the current session of the Conversation tab", () => {
  it("is the current row, else the newest, else none", () => {
    expect(currentSession([])).toBeUndefined();
    expect(currentSession([s("new"), s("cur", { current: true })])!.id).toBe("cur");
    expect(currentSession([s("new"), s("old")])!.id).toBe("new");
  });
});

describe("the rows of the Sessions tab", () => {
  it("makes the rows that resume one harness session one conversation", () => {
    const rows = [
      s("r4", { harness_session_id: "h2", current: true, active: true, turns: 0, title: "Test" }),
      s("r3", { harness_session_id: "h2", outcome: "daemon_restart", turns: 2, title: "Test" }),
      s("r2", { harness_session_id: "h2", outcome: "restarted", turns: 1, started_at: "2026-10-27T09:00:00.000Z" }),
      s("r1", { outcome: "error" }),
      s("r0", { harness_session_id: "h1", outcome: "stopped", title: "  " }),
    ];
    const g = groupSessions(rows);
    expect(g.map((x) => x.session.id)).toEqual(["r4", "r1", "r0"]);
    expect(g[0]).toMatchObject({ turns: 3, resumes: 2, started_at: "2026-10-27T09:00:00.000Z", current: true, active: true, title: "Test" });
    expect(g[0]!.rows.map((x) => x.id)).toEqual(["r4", "r3", "r2"]);
    expect(g[2]!.title).toBeNull();
    // Rows with no harness session never merge.
    expect(groupSessions([s("a", { outcome: "error" }), s("b", { outcome: "error" })])).toHaveLength(2);
  });

  it("shows the resumes that the daemon counts on one row", () => {
    const g = groupSessions([s("r1", { harness_session_id: "h", resumes: 3, turns: 7, current: true }), s("r0", { harness_session_id: "h0" })]);
    expect(g).toHaveLength(2);
    expect(g[0]).toMatchObject({ resumes: 3, turns: 7, current: true });
    expect(g[0]!.rows).toHaveLength(1);
    expect(g[1]!.resumes).toBe(0);
    // An old split row and a counted row of one harness session add up.
    expect(groupSessions([s("b", { harness_session_id: "h", resumes: 2 }), s("a", { harness_session_id: "h", resumes: 1 })])[0]!.resumes).toBe(4);
  });

  it("does not merge a harness session that comes back after another one", () => {
    const g = groupSessions([s("c", { harness_session_id: "h1" }), s("b", { harness_session_id: "h2" }), s("a", { harness_session_id: "h1" })]);
    expect(g).toHaveLength(3);
  });

  it("finds the group of any of its rows, or of its harness session", () => {
    const g = groupSessions([s("r2", { harness_session_id: "h" }), s("r1", { harness_session_id: "h" }), s("r0")]);
    expect(findGroup(g, "r1")!.session.id).toBe("r2");
    expect(findGroup(g, "h")!.session.id).toBe("r2");
    expect(findGroup(g, "r0")!.session.id).toBe("r0");
    expect(findGroup(g, "nope")).toBeUndefined();
  });

  it("labels and chips a group", () => {
    expect(sessionLabel({ id: "r", title: " ", harness_session_id: "h" })).toBe("h");
    expect(sessionLabel({ id: "r" })).toBe("r");
    const [active, current, ended, bare] = groupSessions([
      s("a", { active: true, current: true }),
      s("b", { current: true }),
      s("c", { outcome: "stopped" }),
      s("d"),
    ]);
    expect(groupChip(active!)).toEqual({ label: "active", tone: "ok" });
    expect(groupChip(current!)).toEqual({ label: "current", tone: "info" });
    expect(groupChip(ended!)).toEqual({ label: "stopped", tone: "" });
    expect(groupChip(bare!)).toBeNull();
  });
});

describe("the agent tabs (issue #22)", () => {
  it("has no Screen tab, and has Waits & leases and Details", () => {
    const ids = AGENT_TABS.map((t) => t.id as string);
    expect(ids).not.toContain("screen");
    expect(ids).toContain("waits");
    expect(ids).toContain("details");
    expect(ids[0]).toBe("conversation");
  });
  it("counts the open waits and the active leases in the Waits & leases label", () => {
    expect(waitsTabLabel(undefined, 0)).toBe("Waits & leases");
    expect(waitsTabLabel(0, 0)).toBe("Waits & leases");
    expect(waitsTabLabel(1, 0)).toBe("Waits & leases (1)");
    expect(waitsTabLabel(1, 1)).toBe("Waits & leases (2)");
  });
});

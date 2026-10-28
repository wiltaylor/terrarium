import { describe, expect, it } from "vitest";
import type { CardComment, SessionRef } from "../../api/types";
import { asSession, cardSessionRows, commentSessions, sessionChips, sessionHref, sessionRange } from "./sessions";

const ref = (id: string, extra: Partial<SessionRef> = {}): SessionRef => ({
  id,
  agent_id: `A-${id}`,
  agent: "worker",
  project: "p",
  harness: "claude",
  started_at: "2026-10-28T10:00:00Z",
  ended_at: "2026-10-28T11:00:00Z",
  removed: false,
  ...extra,
});
const comment = (session: SessionRef | null, author = "A"): CardComment => ({ author, body: "b", created_at: "t", session });

describe("card sessions", () => {
  it("links to the session page, the ID escaped", () => {
    expect(sessionHref("01S")).toBe("/sessions/01S");
    expect(sessionHref("a/b")).toBe("/sessions/a%2Fb");
  });

  it("shows removed and open chips", () => {
    expect(sessionChips(ref("1"))).toEqual([]);
    expect(sessionChips(ref("1", { removed: true }))).toEqual([{ label: "removed", tone: "warn" }]);
    expect(sessionChips(ref("1", { ended_at: undefined }))).toEqual([{ label: "open", tone: "ok" }]);
  });

  it("gives the range, open to now", () => {
    const fmt = (iso: string) => iso.slice(11, 16);
    expect(sessionRange(ref("1"), fmt)).toBe("10:00 – 11:00");
    expect(sessionRange(ref("1", { ended_at: undefined }), fmt)).toBe("10:00 – now");
  });

  it("counts the comments of each session, operator comments not", () => {
    const a = ref("a");
    const b = ref("b", { removed: true, agent: "gone" });
    const rows = cardSessionRows([a, b], { comments: [comment(a), comment(null, "operator"), comment(a), comment(b)] });
    expect(rows.map((r) => [r.session.id, r.comments, r.agent, r.href])).toEqual([
      ["a", 2, "p/worker", "/sessions/a"],
      ["b", 1, "p/gone", "/sessions/b"],
    ]);
    expect(rows[1]!.chips).toEqual([{ label: "removed", tone: "warn" }]);
  });

  it("falls back to the comment sessions, distinct and oldest first", () => {
    const late = ref("z", { started_at: "2026-10-28T12:00:00Z" });
    const early = ref("y", { started_at: "2026-10-28T09:00:00Z" });
    expect(commentSessions({ comments: [comment(late), comment(null), comment(early), comment(late)] }).map((s) => s.id)).toEqual(["y", "z"]);
  });

  it("makes a transcript session: ended when the link has an end", () => {
    expect(asSession(ref("1"))).toEqual({ id: "1", agent: "A-1", started_at: "2026-10-28T10:00:00Z", ended_at: "2026-10-28T11:00:00Z", turns: 0, current: false });
    expect(asSession({ ...ref("2", { ended_at: undefined }), active: true })).toMatchObject({ current: true, active: true });
  });
});

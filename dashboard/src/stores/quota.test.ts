import { createRoot } from "solid-js";
import { describe, expect, it } from "vitest";
import type { AgentEvent } from "../api/types";
import { applyQuotaEvent, createQuotaStore, describeCause, emptyQuota, holdsFor, withHolds } from "./quota";

let seq = 0;
const ev = (kind: string, data: Record<string, unknown>, extra: Partial<AgentEvent> = {}): AgentEvent =>
  ({ seq: ++seq, id: `E${seq}`, at: "2026-10-24T10:00:00Z", kind, summary: "", data, ...extra }) as AgentEvent;
const NOW = Date.parse("2026-10-24T10:30:00Z");
const LATER = "2026-10-24T12:00:00Z";
const alert = (used_pct: number, level = "alert", extra: Record<string, unknown> = {}) => ev("quota.alert", { subscription: "max", window: "5h", used_pct, resets_at: LATER, level, alert_at: 80, ...extra });
const blocked = (agent_id: string, until = LATER, cause = "quota:max") => ev("quota.blocked", { agent_id, cause, until, reason: "max window 5h is at 100%", enforced: false }, { agent: agent_id.toLowerCase(), agent_id, project: "p" });

describe("quota holds", () => {
  it("raises no banner for any quota.alert: each line is a mail in the Inbox", () => {
    const s = emptyQuota();
    expect(applyQuotaEvent(s, alert(85))).toBe(s);
    expect(applyQuotaEvent(s, alert(91, "alert", { threshold: 90 }))).toBe(s);
    expect(applyQuotaEvent(s, alert(100, "exhausted", { threshold: 100 }))).toBe(s);
    expect(applyQuotaEvent(s, ev("quota.alert", { project: "web", window: "tokens_per_day", used_pct: 100, level: "exhausted" }, { project: "web" }))).toBe(s);
    expect(applyQuotaEvent(s, ev("quota.sample", { subscription: "max", windows: [{ window: "5h", used_pct: 100 }] }))).toBe(s);
  });

  it("marks held agents from quota.blocked and frees them on quota.resumed", () => {
    let s = applyQuotaEvent(emptyQuota(), blocked("A1"));
    s = applyQuotaEvent(s, blocked("A1"));
    s = applyQuotaEvent(s, blocked("A1", "2026-10-25T00:00:00Z", "budget:project:p"));
    s = applyQuotaEvent(s, blocked("A2"));
    expect(holdsFor(s, "A1", NOW).map((h) => h.cause)).toEqual(["budget:project:p", "quota:max"]);
    expect(holdsFor(s, "A1", NOW)[1]).toMatchObject({ until: LATER, enforced: false, agent: "a1", project: "p" });
    s = applyQuotaEvent(s, ev("quota.resumed", { agent_id: "A1", cause: "quota:max", holds_left: 1 }, { agent_id: "A1" }));
    expect(holdsFor(s, "A1", NOW).map((h) => h.cause)).toEqual(["budget:project:p"]);
    s = applyQuotaEvent(s, ev("quota.resumed", { agent_id: "A1", cause: "budget:project:p", holds_left: 0 }, { agent_id: "A1" }));
    expect(holdsFor(s, "A1", NOW)).toEqual([]);
    expect(holdsFor(s, "A2", NOW)).toHaveLength(1);
    expect(holdsFor(s, "A2", Date.parse("2026-10-24T13:00:00Z"))).toEqual([]); // past its reset
  });

  it("seeds holds from /api/usage/me and describes causes", () => {
    const s = withHolds(emptyQuota(), "A1", [{ agent: "A1", cause: "quota:max", until: LATER, reason: "r", enforced: true }], "lead");
    expect(holdsFor(s, "A1", NOW)[0]).toMatchObject({ agent: "lead", enforced: true });
    expect(describeCause("limit:max")).toBe("a usage-limit error on max");
    expect(describeCause("quota:max")).toBe("subscription max");
    expect(describeCause("budget:project:web/max")).toBe("the daily budget of web/max");
  });

  it("feeds the store from the event stream, ignoring other kinds", () => {
    const listeners: ((e: AgentEvent) => void)[] = [];
    const events = { on: (fn: (e: AgentEvent) => void) => (listeners.push(fn), () => undefined), recent: () => [blocked("A9")] };
    createRoot((dispose) => {
      const q = createQuotaStore(events);
      expect(q.holdsFor("A9", NOW)).toHaveLength(1);
      const before = q.state();
      listeners.forEach((fn) => fn(ev("status", { status: "idle" })));
      expect(q.state()).toBe(before);
      listeners.forEach((fn) => fn(alert(100, "exhausted")));
      expect(q.state()).toBe(before);
      listeners.forEach((fn) => fn(blocked("A8")));
      expect(q.holdsFor("A8", NOW)).toHaveLength(1);
      dispose();
    });
  });
});

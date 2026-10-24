import { createRoot } from "solid-js";
import { describe, expect, it } from "vitest";
import type { AgentEvent } from "../api/types";
import { activeAlerts, alertText, applyQuotaEvent, createQuotaStore, describeCause, emptyQuota, holdsFor, withHolds } from "./quota";

let seq = 0;
const ev = (kind: string, data: Record<string, unknown>, extra: Partial<AgentEvent> = {}): AgentEvent =>
  ({ seq: ++seq, id: `E${seq}`, at: "2026-10-24T10:00:00Z", kind, summary: "", data, ...extra }) as AgentEvent;
const NOW = Date.parse("2026-10-24T10:30:00Z");
const LATER = "2026-10-24T12:00:00Z";
const alert = (used_pct: number, level = "alert", extra: Record<string, unknown> = {}) => ev("quota.alert", { subscription: "max", window: "5h", used_pct, resets_at: LATER, level, alert_at: 80, ...extra });
const blocked = (agent_id: string, until = LATER, cause = "quota:max") => ev("quota.blocked", { agent_id, cause, until, reason: "max window 5h is at 100%", enforced: false }, { agent: agent_id.toLowerCase(), agent_id, project: "p" });

describe("quota banner logic", () => {
  it("raises one alert per window and lets exhausted replace alert", () => {
    let s = applyQuotaEvent(emptyQuota(), alert(85));
    s = applyQuotaEvent(s, alert(90));
    expect(s.alerts).toHaveLength(1);
    expect(s.alerts[0]).toMatchObject({ key: "max/5h", used_pct: 90, level: "alert" });
    s = applyQuotaEvent(s, alert(100, "exhausted"));
    expect(activeAlerts(s, NOW).map((a) => a.level)).toEqual(["exhausted"]);
    expect(alertText(s.alerts[0]!)).toContain("used up");
  });

  it("orders used-up plans first, drops alerts past their reset, and hides dismissed ones until the next period", () => {
    let s = applyQuotaEvent(emptyQuota(), alert(85));
    s = applyQuotaEvent(s, ev("quota.alert", { subscription: "pro", window: "7d", used_pct: 100, resets_at: LATER, level: "exhausted", alert_at: 80 }));
    s = applyQuotaEvent(s, ev("quota.alert", { subscription: "old", window: "5h", used_pct: 90, resets_at: "2026-10-24T10:10:00Z", level: "alert", alert_at: 80 }));
    expect(activeAlerts(s, NOW).map((a) => a.key)).toEqual(["pro/7d", "max/5h"]);
    s = { ...s, dismissed: [`max/5h/alert/${LATER}`] };
    expect(activeAlerts(s, NOW).map((a) => a.key)).toEqual(["pro/7d"]);
    s = applyQuotaEvent(s, alert(100, "exhausted"));
    expect(activeAlerts(s, NOW).map((a) => a.key)).toEqual(["max/5h", "pro/7d"]);
  });

  it("clears an alert when a quota.sample reads the window under the line", () => {
    let s = applyQuotaEvent(emptyQuota(), alert(100, "exhausted"));
    s = applyQuotaEvent(s, ev("quota.sample", { subscription: "max", windows: [{ window: "5h", used_pct: 92, resets_at: LATER }] }));
    expect(s.alerts[0]).toMatchObject({ level: "alert", used_pct: 92 });
    s = applyQuotaEvent(s, ev("quota.sample", { subscription: "max", windows: [{ window: "5h", used_pct: 10 }] }));
    expect(s.alerts).toEqual([]);
    const other = applyQuotaEvent(emptyQuota(), alert(85));
    expect(applyQuotaEvent(other, ev("quota.sample", { subscription: "pro", windows: [{ window: "5h", used_pct: 1 }] }))).toBe(other);
  });

  it("keys a project budget alert apart from the plan", () => {
    const s = applyQuotaEvent(emptyQuota(), ev("quota.alert", { project: "web", subscription: "max", window: "day", used: 59, limit: 60, used_pct: 98, level: "alert" }, { project: "web" }));
    expect(s.alerts[0]).toMatchObject({ key: "budget:web/max/day", project: "web" });
    expect(alertText(s.alerts[0]!)).toContain("daily budget of web");
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
      listeners.forEach((fn) => fn(alert(81)));
      expect(q.alerts(NOW)).toHaveLength(1);
      q.dismiss(q.alerts(NOW)[0]!);
      expect(q.alerts(NOW)).toHaveLength(0);
      dispose();
    });
  });
});

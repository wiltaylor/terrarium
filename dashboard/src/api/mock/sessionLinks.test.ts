import { describe, expect, it } from "vitest";
import { createMockApi } from ".";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0 });

// The session links of the log rows (docs/api.md §4, §7, §9), as
// store/session_refs.rs `links` finds them.
describe("mock log session links", () => {
  it("links the events of an agent to a session that opens", async () => {
    const api = mock();
    const page = await api.events.history({ order: "desc", limit: 200 });
    const linked = page.items.filter((e) => e.session_id);
    expect(linked.length).toBeGreaterThan(0);
    for (const e of linked) {
      expect(e.agent_id).toBeTruthy();
      expect(e.session_label).toMatch(/ · \d{4}-\d\d-\d\d \d\d:\d\d UTC$/);
    }
    const s = await api.sessions.get(linked[0]!.session_id!);
    expect(s.agent_id).toBe(linked[0]!.agent_id);
    // An event with no agent has no session.
    expect(page.items.filter((e) => !e.agent_id).every((e) => !e.session_id)).toBe(true);
  });

  it("links mail from an agent to the sender's session, and nothing else", async () => {
    const api = mock();
    const page = await api.mail.list({ limit: 500 });
    const fromAgent = page.items.filter((m) => m.session_id);
    expect(fromAgent.length).toBeGreaterThan(0);
    for (const m of fromAgent) {
      expect(m.from_kind).toBe("agent");
      expect((await api.sessions.get(m.session_id!)).agent_id).toBe(m.from);
    }
    expect(page.items.filter((m) => m.from_kind !== "agent").every((m) => !m.session_id)).toBe(true);
    const one = await api.mail.get(fromAgent[0]!.id);
    expect(one.session_id).toBe(fromAgent[0]!.session_id);
  });

  it("links an audit row of an agent actor to its session at that time", async () => {
    const api = mock();
    const page = await api.audit({ order: "desc", limit: 500 });
    const linked = page.items.filter((a) => a.session_id);
    expect(linked.length).toBeGreaterThan(0);
    for (const a of linked) {
      expect(a.actor.startsWith("agent:")).toBe(true);
      const s = await api.sessions.get(a.session_id!);
      expect(`agent:${s.agent_id}`).toBe(a.actor);
      expect(s.started_at <= a.at && (!s.ended_at || s.ended_at >= a.at)).toBe(true);
    }
    expect(page.items.filter((a) => !a.actor.startsWith("agent:")).every((a) => !a.session_id)).toBe(true);
  });
});

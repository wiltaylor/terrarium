import { describe, expect, it } from "vitest";
import type { AgentEvent, Lease } from "../api/types";
import { createMockApi } from "../api/mock";
import { applyHoldEvent, createLeasesStore, holdsOf, seedAgentHolds, seedTargetHolds, type Holds } from "./leases";

const NOW = Date.parse("2026-10-25T09:00:00.000Z");
const at = (min: number) => new Date(NOW + min * 60_000).toISOString();
let seq = 0;
const ev = (agent_id: string, data: Record<string, unknown>): AgentEvent =>
  ({ seq: ++seq, id: `E${seq}`, at: at(0), kind: "lease", agent_id, summary: "", data }) as AgentEvent;
const granted = (agent: string, target: string, lease_id: string, expires_at: string | null = at(30), exclusive = false) =>
  ev(agent, { change: "granted", target, lease_id, expires_at, exclusive, state: "active" });
const ended = (agent: string, target: string, lease_id: string) => ev(agent, { change: "ended", target, lease_id, expires_at: null, exclusive: false, state: "revoked" });
const row = (id: string, agent: string, target: string, extra: Partial<Lease> = {}): Lease => ({
  id, target, agent, agent_name: null, project: "p", granted_by: "operator", granted_at: at(-5), expires_at: null, exclusive: false, revoked_at: null, revoked_by: null, reason: null, state: "active", ...extra,
});

describe("lease holds (the 'holds <target>' chips)", () => {
  it("adds a hold on a grant, moves its end on an extend, and removes it on the end", () => {
    let h: Holds = applyHoldEvent({}, granted("A1", "vm", "L1", at(30), true));
    expect(holdsOf(h, "A1", NOW)).toEqual([{ lease_id: "L1", target: "vm", expires_at: at(30), exclusive: true }]);
    h = applyHoldEvent(h, ev("A1", { change: "extended", target: "vm", lease_id: "L1", expires_at: at(90), exclusive: true, state: "active" }));
    expect(holdsOf(h, "A1", NOW)[0]!.expires_at).toBe(at(90));
    h = applyHoldEvent(h, ended("A1", "vm", "L1"));
    expect(holdsOf(h, "A1", NOW)).toEqual([]);
  });

  it("keeps a newer lease when an older one on the same target ends, and ignores other kinds", () => {
    let h = applyHoldEvent({}, granted("A1", "vm", "L2"));
    const same = applyHoldEvent(h, ended("A1", "vm", "L1"));
    expect(same).toBe(h);
    h = applyHoldEvent(h, { ...granted("A1", "box", "L3"), kind: "status" } as AgentEvent);
    expect(Object.keys(h.A1!)).toEqual(["vm"]);
  });

  it("hides a hold past its time, and sorts by target", () => {
    let h = applyHoldEvent({}, granted("A1", "vm", "L1", at(1)));
    h = applyHoldEvent(h, granted("A1", "box", "L2", null));
    expect(holdsOf(h, "A1", NOW).map((x) => x.target)).toEqual(["box", "vm"]);
    expect(holdsOf(h, "A1", NOW + 2 * 60_000).map((x) => x.target)).toEqual(["box"]);
  });

  it("seeds from an agent's list and from a target's list, active leases only", () => {
    let h = applyHoldEvent({}, granted("A1", "old", "L0"));
    h = seedAgentHolds(h, "A1", [row("L1", "A1", "vm"), row("L2", "A1", "box", { state: "revoked" }), row("L3", "A1", "gone", { expires_at: at(-1) })], NOW);
    expect(holdsOf(h, "A1", NOW).map((x) => x.target)).toEqual(["vm"]);
    h = seedTargetHolds(h, "vm", [row("L4", "A2", "vm")], NOW);
    expect(holdsOf(h, "A1", NOW)).toEqual([]);
    expect(holdsOf(h, "A2", NOW).map((x) => x.lease_id)).toEqual(["L4"]);
  });

  it("follows lease events from the events store, and loads every target's leases once", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    let fn: ((e: AgentEvent) => void) | null = null;
    const store = createLeasesStore(api, { on: (f) => ((fn = f), () => (fn = null)) });
    await store.loadAll();
    const runner = api.world.agents.find((a) => a.name === "runner-dev")!;
    const codex = api.world.agents.find((a) => a.name === "adapter-codex")!;
    expect(store.holdsFor(runner.id).map((h) => h.target)).toEqual(["lab-win11"]);
    expect(store.holdsFor(codex.id).map((h) => h.target)).toEqual(["build-box"]);
    fn!(granted(runner.id, "build-box", "LX", null));
    expect(store.holdsFor(runner.id).map((h) => h.target)).toEqual(["build-box", "lab-win11"]);
    fn!(ended(runner.id, "build-box", "LX"));
    expect(store.holdsFor(runner.id).map((h) => h.target)).toEqual(["lab-win11"]);
    store.dispose();
    expect(fn).toBeNull();
  });
});

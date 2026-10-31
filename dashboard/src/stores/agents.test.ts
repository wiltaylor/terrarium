import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { createMockApi } from "../api/mock";
import type { Agent, AgentEvent } from "../api/types";
import { applyAgentEvent, createAgentsStore, mergeFresh, removeAgent } from "./agents";
import { createEventsStore } from "./events";
import { fakeClock, waitFor } from "../test/wait";

const agent = (over: Partial<Agent> = {}): Agent => ({
  id: "A1",
  name: "lead",
  project: "p",
  harness: "claude",
  subscription: "s",
  status: "idle",
  roles: [],
  workspace: "/w",
  created_at: "",
  ...over,
});
const ev = (over: Partial<AgentEvent>): AgentEvent => ({ seq: 1, id: "E", at: "2026-10-24T10:00:00Z", kind: "status", agent: "lead", project: "p", summary: "", data: {}, ...over });

describe("applyAgentEvent", () => {
  it("keeps the reason of a status change, and the pause flag of the pause events", () => {
    const [a] = applyAgentEvent([agent()], ev({ data: { status: "stopped", from: "idle", reason: "idle" } }));
    expect(a).toMatchObject({ status: "stopped", status_reason: "idle" });
    const [b] = applyAgentEvent([a!], ev({ kind: "agent.paused" }));
    expect(b!.paused).toBe(true);
    const [c] = applyAgentEvent([b!], ev({ kind: "agent.unpaused" }));
    expect(c!.paused).toBe(false);
    const [d] = applyAgentEvent([c!], ev({ data: { status: "starting", from: "stopped", reason: null } }));
    expect(d!.status_reason).toBeUndefined();
  });

  it("keeps the compacting reason, and the end of a compaction", () => {
    const [a] = applyAgentEvent([agent()], ev({ data: { status: "working", from: "idle", reason: "compacting" } }));
    expect(a).toMatchObject({ status: "working", status_reason: "compacting" });
    const [b] = applyAgentEvent([a!], ev({ kind: "agent.compacted", at: "2026-10-24T10:01:00Z", data: { cause: "manual", ok: false } }));
    expect(b).toMatchObject({ last_compact_at: "2026-10-24T10:01:00Z", last_compact_ok: false });
  });

  it("applies status events and tracks turn start", () => {
    const [a] = applyAgentEvent([agent()], ev({ data: { status: "working" } }));
    expect(a!.status).toBe("working");
    expect(a!.turn_started_at).toBe("2026-10-24T10:00:00Z");
    const [b] = applyAgentEvent([a!], ev({ kind: "turn_end" }));
    expect(b!.status).toBe("idle");
    expect(b!.turn_started_at).toBeUndefined();
  });

  it("takes waits and waiting_on from a status event, and drops them when absent", () => {
    const [a] = applyAgentEvent([agent()], ev({ data: { status: "waiting", from: "idle", reason: null, waits: 2, waiting_on: "agent p/w1 (done); time 10m" } }));
    expect(a).toMatchObject({ status: "waiting", waits: 2, waiting_on: "agent p/w1 (done); time 10m" });
    const [b] = applyAgentEvent([a!], ev({ data: { status: "working", from: "waiting", reason: null, waits: 2, waiting_on: "agent p/w1 (done); time 10m" } }));
    // A turn_end of an agent with open waits rests in waiting, not idle.
    const [c] = applyAgentEvent([b!], ev({ kind: "turn_end" }));
    expect(c!.status).toBe("waiting");
    const [d] = applyAgentEvent([c!], ev({ data: { status: "idle", from: "waiting", reason: null } }));
    expect(d!.status).toBe("idle");
    expect(d!.waits).toBeUndefined();
    expect(d!.waiting_on).toBeUndefined();
  });

  it("marks exit as error and records the last event", () => {
    const e = ev({ kind: "exit", summary: "exit 1" });
    const [a] = applyAgentEvent([agent()], e);
    expect(a!.status).toBe("error");
    expect(a!.last_event).toEqual(e);
  });

  it("matches by agent_id first, as the daemon sends it", () => {
    const list = [agent(), agent({ id: "A2", project: "q" })];
    const next = applyAgentEvent(list, ev({ agent: "lead", project: "q", agent_id: "A2", data: { agent_id: "A2", status: "blocked", from: "idle", reason: null } }));
    expect(next[0]!.status).toBe("idle");
    expect(next[1]!.status).toBe("blocked");
    expect(applyAgentEvent(list, ev({ agent_id: "GONE", data: { status: "idle" } }))).toBe(list);
  });

  it("tracks the current card from card events", () => {
    const moved = applyAgentEvent([agent()], ev({ kind: "card", agent_id: "A1", card: "P-1", data: { card_id: "P-1", change: "moved", column: "in_progress" } }));
    expect(moved[0]!.current_card).toBe("P-1");
    const done = applyAgentEvent(moved, ev({ kind: "card", agent_id: "A1", card: "P-1", data: { card_id: "P-1", change: "moved", column: "review" } }));
    expect(done[0]!.current_card).toBeUndefined();
  });

  it("ignores unknown agents, other projects, and bad statuses", () => {
    const list = [agent()];
    expect(applyAgentEvent(list, ev({ agent: "other" }))).toBe(list);
    expect(applyAgentEvent(list, ev({ project: "q" }))).toBe(list);
    expect(applyAgentEvent(list, ev({ data: { status: "dancing" } }))[0]!.status).toBe("idle");
  });
});

describe("createAgentsStore", () => {
  it("loads agents and follows the event stream", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0 });
    await createRoot(async (dispose) => {
      const events = createEventsStore(api);
      const store = createAgentsStore(api, events);
      await store.refresh();
      const lead = store.byName("terrarium", "lead")!;
      expect(lead.status).toBe("idle");
      events.start();
      await new Promise((r) => setTimeout(r, 5));
      await api.agents.send(lead.id, "hello");
      await waitFor(() => expect(events.recent().some((e) => e.kind === "turn_start" && e.agent_id === lead.id)).toBe(true));
      expect(store.byId("terrarium/lead")!.id).toBe(lead.id);
      events.stop();
      dispose();
    });
  });
});

describe("createAgentsStore backlog", () => {
  it("applies events that came before the list, but keeps the list's status", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0 });
    const lead = api.world.agents.find((a) => a.name === "lead")!;
    const early = ev({ seq: 1, id: "EARLY", agent_id: lead.id, kind: "tool_use", summary: "Read a file", data: { agent_id: lead.id } });
    const stale = ev({ seq: 2, id: "STALE", agent_id: lead.id, data: { agent_id: lead.id, status: "error" } });
    await createRoot(async (dispose) => {
      const store = createAgentsStore(api, { on: () => () => {}, recent: () => [stale, early] });
      await store.refresh();
      const a = store.byId(lead.id)!;
      expect(a.last_event?.id).toBe("STALE");
      expect(a.status).toBe("idle");
      dispose();
    });
  });
});

describe("agent removal", () => {
  const removedEv = (id: string) => ev({ seq: 9, id: "RM", kind: "agent_removed", agent: "lead", agent_id: id, data: { agent_id: id, project: "p", name: "lead" } });

  it("drops the agent on agent_removed, by ULID only", () => {
    const list = [agent(), agent({ id: "A2", name: "w1" })];
    expect(applyAgentEvent(list, removedEv("A1")).map((a) => a.id)).toEqual(["A2"]);
    // An unknown ULID changes nothing, even when the name matches.
    expect(applyAgentEvent(list, removedEv("ZZ"))).toBe(list);
    expect(removeAgent(list, undefined)).toBe(list);
  });

  it("does not bring a removed agent back from a replayed backlog", () => {
    const status = ev({ seq: 1, id: "S", agent_id: "A1", data: { agent_id: "A1", status: "stopped" } });
    // The fresh list has no A1: the status replay cannot add it.
    const out = mergeFresh([agent({ id: "A2", name: "w1" })], [agent()], [removedEv("A1"), status]);
    expect(out.map((a) => a.id)).toEqual(["A2"]);
  });

  it("keeps a removed agent out of a list fetched before the removal", () => {
    const out = mergeFresh([agent(), agent({ id: "A2", name: "w1" })], [], [], new Set(["A1"]));
    expect(out.map((a) => a.id)).toEqual(["A2"]);
  });

  it("keeps the list's status for each agent when the replay drops one", () => {
    const fresh = [agent({ id: "A1", status: "idle" }), agent({ id: "A2", name: "w1", status: "working" })];
    const out = mergeFresh(fresh, [], [removedEv("A1")]);
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ id: "A2", status: "working" });
  });

  it("drops a deleted agent from the store, and its later events ask for no refresh", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    await createRoot(async (dispose) => {
      let listen: (e: AgentEvent) => void = () => {};
      const store = createAgentsStore(api, { on: (fn) => ((listen = fn), () => {}) });
      await store.refresh();
      const w = store.state.agents.find((a) => a.source !== "file")!;
      await api.agents.remove(w.id, true);
      listen(removedEv(w.id));
      expect(store.byId(w.id)).toBeUndefined();
      let calls = 0;
      const list = api.agents.list;
      api.agents.list = (...args) => (calls++, list(...args));
      // A fake clock: 600 ms passes the 500 ms refresh debounce at once.
      const tick = fakeClock();
      listen(ev({ kind: "session_ended", agent_id: w.id, data: {} }));
      await tick(600);
      expect(calls).toBe(0);
      store.dispose();
      dispose();
    });
  });
});

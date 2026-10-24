import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { createMockApi } from "../api/mock";
import type { Agent, AgentEvent } from "../api/types";
import { applyAgentEvent, createAgentsStore } from "./agents";
import { createAuthStore, type KeyValueStorage } from "./auth";
import { createEventsStore } from "./events";

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
  it("applies status events and tracks turn start", () => {
    const [a] = applyAgentEvent([agent()], ev({ data: { status: "working" } }));
    expect(a!.status).toBe("working");
    expect(a!.turn_started_at).toBe("2026-10-24T10:00:00Z");
    const [b] = applyAgentEvent([a!], ev({ kind: "turn_end" }));
    expect(b!.status).toBe("idle");
    expect(b!.turn_started_at).toBeUndefined();
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
      await new Promise((r) => setTimeout(r, 1));
      expect(events.recent().some((e) => e.kind === "turn_start" && e.agent_id === lead.id)).toBe(true);
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

describe("createAuthStore", () => {
  it("persists the token and flags 401s", () => {
    const mem = new Map<string, string>();
    const storage: KeyValueStorage = { getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => void mem.set(k, v), removeItem: (k) => void mem.delete(k) };
    createRoot((dispose) => {
      const auth = createAuthStore({ session: storage });
      expect(auth.token()).toBeNull();
      auth.requireToken();
      expect(auth.needsToken()).toBe(true);
      auth.setToken("  abc  ");
      expect(auth.token()).toBe("abc");
      expect(auth.needsToken()).toBe(false);
      expect(createAuthStore({ session: storage }).token()).toBe("abc");
      auth.clear();
      expect(mem.size).toBe(0);
      dispose();
    });
  });
});

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
  roles: ["member"],
  workspace: "/w",
  created_at: "",
  ...over,
});
const ev = (over: Partial<AgentEvent>): AgentEvent => ({ id: "E", at: "2026-10-24T10:00:00Z", kind: "status", agent: "lead", project: "p", summary: "", ...over });

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
      await api.agents.send(lead.id, "hello");
      expect(store.byId(lead.id)!.status).toBe("working");
      expect(events.recent().length).toBeGreaterThan(0);
      dispose();
    });
  });
});

describe("createAuthStore", () => {
  it("persists the token and flags 401s", () => {
    const mem = new Map<string, string>();
    const storage: KeyValueStorage = { getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => void mem.set(k, v), removeItem: (k) => void mem.delete(k) };
    createRoot((dispose) => {
      const auth = createAuthStore(storage);
      expect(auth.token()).toBeNull();
      auth.requireToken();
      expect(auth.needsToken()).toBe(true);
      auth.setToken("  abc  ");
      expect(auth.token()).toBe("abc");
      expect(auth.needsToken()).toBe(false);
      expect(createAuthStore(storage).token()).toBe("abc");
      auth.clear();
      expect(mem.size).toBe(0);
      dispose();
    });
  });
});

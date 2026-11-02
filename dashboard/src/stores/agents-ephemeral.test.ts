// The agents store and ephemeral agents (docs/api.md §6.10): the list
// leaves them out, and their events ask for one refresh only.
import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { createMockApi } from "../api/mock";
import type { AgentEvent } from "../api/types";
import { createAgentsStore } from "./agents";
import { fakeClock } from "../test/wait";

const ev = (over: Partial<AgentEvent>): AgentEvent => ({ seq: 1, id: "E", at: "2026-10-24T10:00:00Z", kind: "status", agent: "x", project: "terrarium", summary: "", data: { status: "working" }, ...over });

describe("the agents store and ephemeral agents", () => {
  it("does not list them, does not upsert them, and refreshes once for their events", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    await createRoot(async (dispose) => {
      let listen: (e: AgentEvent) => void = () => {};
      const store = createAgentsStore(api, { on: (fn) => ((listen = fn), () => {}) });
      await store.refresh();
      expect(store.state.agents.some((a) => a.ephemeral)).toBe(false);
      const eph = api.world.agents.find((a) => a.ephemeral)!;
      store.upsert(structuredClone(eph));
      expect(store.byId(eph.id)).toBeUndefined();
      let calls = 0;
      const list = api.agents.list;
      api.agents.list = (...args) => (calls++, list(...args));
      const tick = fakeClock();
      listen(ev({ agent_id: eph.id }));
      await tick(600);
      expect(calls).toBe(1);
      listen(ev({ agent_id: eph.id, seq: 2 }));
      await tick(600);
      expect(calls).toBe(1);
      store.dispose();
      dispose();
    });
  });
});

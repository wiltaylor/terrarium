import { describe, expect, it, vi } from "vitest";
import { createRoot } from "solid-js";
import type { AgentEvent, AuditEntry, Subscription } from "../api/types";
import { createSubscriptionsStore } from "./subscriptions";

describe("subscriptions store", () => {
  it("coalesces assignment events and preserves known counts after mutation replies", async () => {
    vi.useFakeTimers();
    let onEvent!: (e: AgentEvent) => void;
    let onAudit!: (a: AuditEntry) => void;
    const list = vi.fn(async () => [{ name: "s", assigned_agent_count: 0, assigned_agent_count_scope: "all_projects" } as Subscription]);
    const api = { subscriptions: { list } } as unknown as Parameters<typeof createSubscriptionsStore>[0];
    const store = createSubscriptionsStore(api, {
      on: (fn) => { onEvent = fn; return () => {}; },
      onAudit: (fn) => { onAudit = fn; return () => {}; },
    });
    try {
      await store.ensureLoaded();
      store.replace({ name: "s" } as Subscription);
      expect(store.byName("s")?.assigned_agent_count).toBe(0);
      for (const action of ["agent.create", "agent.update", "agent.delete"]) onAudit({ action } as AuditEntry);
      for (const kind of ["subscription", "agent.swapped", "agent_removed"]) onEvent({ kind } as AgentEvent);
      await vi.advanceTimersByTimeAsync(400);
      expect(list).toHaveBeenCalledTimes(2);
      expect(store.byName("s")?.assigned_agent_count).toBe(0);
    } finally {
      store.stop();
      vi.useRealTimers();
    }
  });
  it("ignores an older reply after a newer refresh", async () => {
    const replies: ((s: Subscription[]) => void)[] = [];
    const api = { subscriptions: { list: () => new Promise<Subscription[]>((resolve) => replies.push(resolve)) } } as unknown as Parameters<typeof createSubscriptionsStore>[0];
    const store = createSubscriptionsStore(api);
    const old = store.refresh();
    const fresh = store.refresh();
    replies[1]!([{ name: "s", assigned_agent_count: 2 } as Subscription]);
    await fresh;
    replies[0]!([{ name: "s", assigned_agent_count: 1 } as Subscription]);
    await old;
    expect(store.byName("s")?.assigned_agent_count).toBe(2);
    store.stop();
  });
  it("ensureLoaded loads once, and joins a load in flight", async () => {
    let calls = 0;
    const api = { subscriptions: { list: async () => (calls++, [{ name: "zai-key" } as Subscription]) } } as unknown as Parameters<typeof createSubscriptionsStore>[0];
    await createRoot(async (dispose) => {
      const s = createSubscriptionsStore(api);
      expect(s.list()).toEqual([]);
      await Promise.all([s.ensureLoaded(), s.ensureLoaded()]);
      expect(calls).toBe(1);
      expect(s.byName("zai-key")?.name).toBe("zai-key");
      await s.ensureLoaded();
      expect(calls).toBe(1);
      // A refresh is always a new request.
      await s.refresh();
      expect(calls).toBe(2);
      dispose();
    });
  });
});

import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import type { Subscription } from "../api/types";
import { createSubscriptionsStore } from "./subscriptions";

describe("subscriptions store", () => {
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

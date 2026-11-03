import { createRoot } from "solid-js";
import { describe, expect, it } from "vitest";
import { createMockApi } from "../api/mock";
import type { AgentEvent } from "../api/types";
import { createPinsStore } from "./pins";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });

/** A fake event bus: `emit` sends one event to every listener. */
function bus() {
  const fns = new Set<(e: AgentEvent) => void>();
  return {
    on: (fn: (e: AgentEvent) => void) => {
      fns.add(fn);
      return () => fns.delete(fn);
    },
    emit: (e: Partial<AgentEvent>) => fns.forEach((f) => f(e as AgentEvent)),
  };
}

function memoryStorage() {
  const m = new Map<string, string>();
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), removeItem: (k: string) => void m.delete(k), m };
}

describe("the pins store", () => {
  it("pins, opens the new tab, unpins, and keeps the order of the daemon", async () => {
    const api = mock();
    const [lead, dev] = api.world.agents;
    const store = createRoot(() => createPinsStore(api));
    await store.refresh();
    expect(store.pins()).toEqual([]);
    expect(store.active()).toBeNull();

    await store.pin(lead!.id);
    await store.pin(`${dev!.project}/${dev!.name}`);
    expect(store.pins().map((p) => p.agent)).toEqual([lead!.id, dev!.id]);
    expect(store.active()).toBe(dev!.id);
    expect(store.isPinned(lead!.id)).toBe(true);

    // The list is on the daemon: a second store (another browser) reads it.
    const other = createRoot(() => createPinsStore(api));
    await other.refresh();
    expect(other.pins().map((p) => p.agent)).toEqual([lead!.id, dev!.id]);

    await store.toggle(dev!.id);
    expect(store.pins().map((p) => p.agent)).toEqual([lead!.id]);
    expect(store.active()).toBe(lead!.id);
  });

  it("drops the pin of a removed agent at once, and the daemon drops it too", async () => {
    const api = mock();
    const ev = bus();
    const a = api.world.agents.find((x) => x.status === "stopped") ?? api.world.agents[0]!;
    const store = createRoot(() => createPinsStore(api, ev));
    await store.pin(a.id);
    ev.emit({ kind: "agent_removed", agent_id: a.id, data: { agent_id: a.id } });
    expect(store.pins()).toEqual([]);
    await api.agents.remove(a.id, true);
    expect(await api.pins.list()).toEqual([]);
  });

  it("keeps the open tab and the collapsed side bar in the browser's storage", async () => {
    const api = mock();
    const storage = memoryStorage();
    const [a, b] = api.world.agents;
    const store = createRoot(() => createPinsStore(api, undefined, storage));
    await store.pin(a!.id);
    await store.pin(b!.id);
    store.select(a!.id);
    store.setCollapsed(true);
    const again = createRoot(() => createPinsStore(api, undefined, storage));
    await again.refresh();
    expect(again.active()).toBe(a!.id);
    expect(again.collapsed()).toBe(true);
    // A new pin opens the side bar.
    await again.unpin(b!.id);
    await again.pin(b!.id);
    expect(again.collapsed()).toBe(false);
  });

  it("keeps the error of a failed pin, and an unknown agent is a 404", async () => {
    const api = mock();
    const store = createRoot(() => createPinsStore(api));
    await expect(store.pin("nope/nobody")).rejects.toMatchObject({ status: 404 });
    expect(store.error()).toBeTruthy();
  });

  it("is for the operator only", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "terrarium" } });
    await expect(api.pins.list()).rejects.toMatchObject({ status: 403, code: "forbidden" });
    await expect(api.pins.pin(api.world.agents[0]!.id)).rejects.toMatchObject({ status: 403 });
  });
});

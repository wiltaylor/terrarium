import { describe, expect, it } from "vitest";
import { createMockApi } from "../api/mock";
import { fakeUlid } from "../api/mock/data";
import type { AgentEvent } from "../api/types";
import { createAttentionStore } from "./attention";

const wait = (ms = 20) => new Promise((r) => setTimeout(r, ms));

/** The mock and an `on` fed from its event stream, as the events store does. */
function setup() {
  const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
  const subs = new Set<(e: AgentEvent) => void>();
  api.events.stream({ since: api.world.seq }).subscribe((e) => subs.forEach((f) => f(e)));
  const events = { on: (f: (e: AgentEvent) => void) => (subs.add(f), () => subs.delete(f)) };
  const store = createAttentionStore(api, events, { prefixes: () => ({ TER: "terrarium", WEB: "webshop" }), debounceMs: 0 });
  return { api, store };
}

describe("attention store", () => {
  it("counts the seed's mail that needs the operator", async () => {
    const { store } = setup();
    expect(store.count()).toBe(0);
    await store.refresh();
    expect(store.loaded()).toBe(true);
    expect(store.items().map((e) => e.message.id)).toEqual([16, 13, 12, 11, 3].map((n) => fakeUlid("msg", n)));
    expect(store.count()).toBe(5);
  });

  it("drops a message once the operator reads it, and sets read_by", async () => {
    const { api, store } = setup();
    await store.refresh();
    const read = await api.mail.markRead(fakeUlid("msg", 3));
    expect(read.read_by).toBe("operator");
    store.upsert([read]);
    expect(store.count()).toBe(4);
  });

  it("ends a pause when the operator replies on the thread", async () => {
    const { api, store } = setup();
    await store.refresh();
    await api.mail.reply(fakeUlid("msg", 12), "Stop. Rerun with RUST_LOG=debug and send me the log.");
    await wait();
    const ids = store.items().map((e) => e.message.id);
    expect(ids).not.toContain(fakeUlid("msg", 11));
    expect(ids).not.toContain(fakeUlid("msg", 12));
    expect(store.count()).toBe(3);
  });

  it("refreshes on a mail event and keeps the IDs that the event flags held", async () => {
    const { api, store } = setup();
    await store.refresh();
    // An eighth agent reply on the paused thread (the mock lets the test send as an agent): held, no second notice.
    const m = await api.mail.send({ from: "terrarium/runner-dev", to: "terrarium/lead", thread_id: fakeUlid("th", 7), body: "Still cannot reproduce." });
    expect(m.state).toBe("delivered");
    expect(m.thread_paused).toBe(true);
    await wait();
    expect(store.live().has(m.id)).toBe(true);
    const entry = store.items().find((e) => e.message.id === m.id)!;
    expect(entry.reasons).toEqual(["held", "paused"]);
    expect(api.world.mail.filter((x) => x.thread_id === fakeUlid("th", 7) && x.from_kind === "system")).toHaveLength(1);
    expect(store.count()).toBe(6);
  });
});

import { createRoot } from "solid-js";
import { describe, expect, it } from "vitest";
import type { InboxCounts, InboxItem } from "../api/types";
import { EMPTY_COUNTS } from "../features/inbox/model";
import { createInboxStore } from "./inbox";

type Deferred<T> = { promise: Promise<T>; resolve: (v: T) => void };
const deferred = <T>(): Deferred<T> => {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
};

const counts = (thread_critical: number): InboxCounts => ({ ...EMPTY_COUNTS, thread: thread_critical, thread_critical });
const criticalThread: InboxItem = { id: "inbox-thread:T1", kind: "thread", open: true, urgent: true, critical: true, project: "p", at: "2026-11-09T10:00:00Z" };
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("inbox store", () => {
  it("drops an older refresh that resolves after a newer one (no stale critical item)", async () => {
    // Refresh 1 sees a critical thread; its thread read is still in flight
    // when refresh 2 sees none. The late thread read must not restore it.
    const [older, newer] = [deferred<InboxCounts>(), deferred<InboxCounts>()];
    const threadRead = deferred<InboxItem[]>();
    let c = 0;
    const api = {
      inbox: {
        counts: () => (c++ === 0 ? older : newer).promise,
        list: (q: { kind?: string }) => (q.kind === "thread" ? threadRead.promise : Promise.resolve([])),
      },
    } as unknown as Parameters<typeof createInboxStore>[0];
    await createRoot(async (dispose) => {
      const store = createInboxStore(api);
      const first = store.refresh();
      older.resolve(counts(1));
      await flush(); // refresh 1 now waits for its thread read
      const second = store.refresh();
      newer.resolve(counts(0));
      await second;
      expect(store.open()).toEqual([]);
      threadRead.resolve([criticalThread]);
      await first;
      expect(store.open()).toEqual([]);
      expect(store.counts().thread_critical).toBe(0);
      dispose();
    });
  });

  it("drops older counts that resolve after newer ones", async () => {
    const [older, newer] = [deferred<InboxCounts>(), deferred<InboxCounts>()];
    let c = 0;
    const api = {
      inbox: {
        counts: () => (c++ === 0 ? older : newer).promise,
        list: (q: { kind?: string }) => Promise.resolve(q.kind === "thread" ? [criticalThread] : []),
      },
    } as unknown as Parameters<typeof createInboxStore>[0];
    await createRoot(async (dispose) => {
      const store = createInboxStore(api);
      const first = store.refresh();
      const second = store.refresh();
      newer.resolve(counts(0));
      await second;
      older.resolve(counts(1));
      await first;
      expect(store.counts().thread_critical).toBe(0);
      expect(store.open()).toEqual([]);
      dispose();
    });
  });

  it("keeps the critical thread of a single refresh", async () => {
    const api = {
      inbox: {
        counts: () => Promise.resolve(counts(1)),
        list: (q: { kind?: string }) => Promise.resolve(q.kind === "thread" ? [criticalThread] : []),
      },
    } as unknown as Parameters<typeof createInboxStore>[0];
    await createRoot(async (dispose) => {
      const store = createInboxStore(api);
      await store.refresh();
      expect(store.open().map((i) => i.id)).toEqual(["inbox-thread:T1"]);
      dispose();
    });
  });
});

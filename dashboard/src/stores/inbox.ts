// What waits for the operator: the counts for the Inbox badge in the sidebar
// (`GET /api/inbox/counts`), and the open items for the Home queue and the
// critical banner (`GET /api/inbox`). An `inbox` or `question` event
// refreshes them.
//
// The threads left `GET /api/inbox` (TER-466). A critical question inside a
// thread still belongs in the queue and the banner (TER-488): while the counts
// give `thread_critical > 0`, the refresh also reads the open thread items
// (`kind=thread`) and keeps the critical ones. With no critical thread it
// sends no extra request. Overlapping refreshes apply only the newest one.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { InboxCounts, InboxItem } from "../api/types";
import { EMPTY_COUNTS, isCritical, isInboxEvent, sortItems } from "../features/inbox/model";
import type { EventsStore } from "./events";

export function createInboxStore(api: Pick<Api, "inbox">, events?: Pick<EventsStore, "on">, opts: { debounceMs?: number } = {}) {
  const [counts, setCounts] = createSignal<InboxCounts>(EMPTY_COUNTS);
  const [items, setItems] = createSignal<InboxItem[]>([]);
  const [threads, setThreads] = createSignal<InboxItem[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const open = () => (threads().length ? sortItems([...items(), ...threads()]) : items());
  let timer: ReturnType<typeof setTimeout> | undefined;
  // The generation of the newest refresh: an older read that resolves later
  // is dropped, so it cannot restore a stale (critical) item.
  let generation = 0;

  const refresh = async () => {
    const gen = ++generation;
    const [c, list] = await Promise.allSettled([api.inbox.counts(), api.inbox.list({ status: "open" })]);
    if (gen !== generation) return;
    // A failed read keeps the last value.
    if (c.status === "fulfilled") setCounts(c.value);
    if (list.status === "fulfilled") setItems(list.value);
    if (c.status === "fulfilled" || list.status === "fulfilled") setLoaded(true);
    if (c.status !== "fulfilled") return;
    if ((c.value.thread_critical ?? 0) <= 0) {
      setThreads([]);
      return;
    }
    try {
      const critical = (await api.inbox.list({ status: "open", kind: "thread" })).filter(isCritical);
      if (gen !== generation) return;
      setThreads(critical);
    } catch {
      // A failed read keeps the last value.
    }
  };

  const off = events?.on((e) => {
    if (!isInboxEvent(e)) return;
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), opts.debounceMs ?? 300);
  });

  return {
    /** The open items by kind, for the badge. */
    counts,
    /** The open items, urgent first, then the newest; with the threads that have an open critical question (TER-488). */
    open,
    loaded,
    refresh,
    dispose: () => { clearTimeout(timer); off?.(); },
  };
}

export type InboxStore = ReturnType<typeof createInboxStore>;

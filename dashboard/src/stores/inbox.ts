// What waits for the operator: the counts for the Inbox badge in the sidebar
// (`GET /api/inbox/counts`), and the open items for the Home queue
// (`GET /api/inbox`). An `inbox` or `question` event refreshes them.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { InboxCounts, InboxItem } from "../api/types";
import { EMPTY_COUNTS, isInboxEvent } from "../features/inbox/model";
import type { EventsStore } from "./events";

export function createInboxStore(api: Pick<Api, "inbox">, events?: Pick<EventsStore, "on">, opts: { debounceMs?: number } = {}) {
  const [counts, setCounts] = createSignal<InboxCounts>(EMPTY_COUNTS);
  const [open, setOpen] = createSignal<InboxItem[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refresh = async () => {
    const [c, items] = await Promise.allSettled([api.inbox.counts(), api.inbox.list({ status: "open" })]);
    // A failed read keeps the last value.
    if (c.status === "fulfilled") setCounts(c.value);
    if (items.status === "fulfilled") setOpen(items.value);
    if (c.status === "fulfilled" || items.status === "fulfilled") setLoaded(true);
  };

  events?.on((e) => {
    if (!isInboxEvent(e)) return;
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), opts.debounceMs ?? 300);
  });

  return {
    /** The open items by kind, for the badge. */
    counts,
    /** The open items, urgent first, then the newest. */
    open,
    loaded,
    refresh,
  };
}

export type InboxStore = ReturnType<typeof createInboxStore>;

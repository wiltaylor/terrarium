// Unread counts for the sidebar: each project inbox. They refresh when a
// `mail` event arrives. The Mail count is the attention count
// (stores/attention.ts), not an unread count.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { EventsStore } from "./events";

export interface UnreadCount {
  n: number;
  /** True when the count hit the page size; show "n+". */
  more: boolean;
}

const LIMIT = 200;

export function createUnreadStore(api: Pick<Api, "inbox">, events?: Pick<EventsStore, "on">) {
  const [inboxes, setInboxes] = createSignal<Record<string, UnreadCount>>({});
  const watched = new Set<string>();
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refreshInbox = async (project: string) => {
    try {
      const p = await api.inbox.list(project, { unread: true, limit: LIMIT });
      setInboxes((prev) => ({ ...prev, [project]: { n: p.items.length, more: p.next !== null } }));
    } catch {
      // keep the last count
    }
  };
  const refresh = () => Promise.all([...watched].map(refreshInbox)).then(() => undefined);

  events?.on((e) => {
    if (e.kind !== "mail") return;
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), 300);
  });

  return {
    inbox: (project: string | null | undefined) => (project ? inboxes()[project] : undefined),
    refresh,
    /** Track the inbox of a project. */
    watch(project: string) {
      if (watched.has(project)) return;
      watched.add(project);
      void refreshInbox(project);
    },
  };
}

export type UnreadStore = ReturnType<typeof createUnreadStore>;

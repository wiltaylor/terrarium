// Unread counts for the sidebar: the operator's mail and each project inbox.
// They refresh when a `mail` event arrives.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { EventsStore } from "./events";

export interface UnreadCount {
  n: number;
  /** True when the count hit the page size; show "n+". */
  more: boolean;
}

const LIMIT = 200;

export function createUnreadStore(api: Pick<Api, "mail" | "inbox">, events?: Pick<EventsStore, "on">) {
  const [operator, setOperator] = createSignal<UnreadCount>({ n: 0, more: false });
  const [inboxes, setInboxes] = createSignal<Record<string, UnreadCount>>({});
  const watched = new Set<string>();
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refreshOperator = async () => {
    try {
      const p = await api.mail.list({ to: "operator", state: "unread", limit: LIMIT });
      setOperator({ n: p.items.length, more: p.next !== null });
    } catch {
      // keep the last count
    }
  };
  const refreshInbox = async (project: string) => {
    try {
      const p = await api.inbox.list(project, { unread: true, limit: LIMIT });
      setInboxes((prev) => ({ ...prev, [project]: { n: p.items.length, more: p.next !== null } }));
    } catch {
      // keep the last count
    }
  };
  const refresh = () => Promise.all([refreshOperator(), ...[...watched].map(refreshInbox)]).then(() => undefined);

  events?.on((e) => {
    if (e.kind !== "mail") return;
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), 300);
  });

  return {
    operator,
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

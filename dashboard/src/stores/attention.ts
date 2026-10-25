// The mail that needs the operator (features/mail/log.ts, the attention rule):
// the sidebar count on Mail and the mail items of the Home queue. It holds
// every message, because the daemon cannot filter on the rule (held and
// paused need the whole thread). A `mail` event refreshes it.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { Message } from "../api/types";
import { annotate, mergeMessages, needsAttention, type LogEntry } from "../features/mail/log";
import { collectAll } from "../lib/paging";
import type { EventsStore } from "./events";

/** Pages of 1000; the cap keeps a runaway log from hanging the tab. */
const MAX_PAGES = 20;

export interface AttentionOptions {
  /** Card prefixes to projects, for blocked notices. */
  prefixes?: () => Record<string, string>;
  debounceMs?: number;
}

export function createAttentionStore(api: Pick<Api, "mail">, events?: Pick<EventsStore, "on">, opts: AttentionOptions = {}) {
  const [messages, setMessages] = createSignal<Message[]>([]);
  const [live, setLive] = createSignal<ReadonlySet<string>>(new Set());
  const [entries, setEntries] = createSignal<LogEntry[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  const recompute = () => setEntries(annotate(messages(), { prefixes: opts.prefixes?.(), live: live() }));

  const refresh = async () => {
    try {
      setMessages(await collectAll((cursor) => api.mail.list({ cursor, limit: 1000 }), MAX_PAGES));
      setLoaded(true);
      recompute();
    } catch {
      // keep the last list
    }
  };

  events?.on((e) => {
    if (e.kind !== "mail") return;
    if (e.data?.held === true && typeof e.data.message_id === "string") setLive((s) => new Set([...s, e.data.message_id as string]));
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), opts.debounceMs ?? 300);
  });

  return {
    /** Every message, annotated, newest first. */
    entries,
    /** The messages that need the operator, newest first. */
    items: () => entries().filter(needsAttention),
    count: () => entries().filter(needsAttention).length,
    /** IDs that a `mail` event flagged `held`. */
    live,
    loaded,
    refresh,
    /** Put changed messages in without a request: a read, a reply. */
    upsert(changed: Message[]) {
      if (!changed.length) return;
      setMessages((cur) => mergeMessages(cur, changed));
      recompute();
    },
  };
}

export type AttentionStore = ReturnType<typeof createAttentionStore>;

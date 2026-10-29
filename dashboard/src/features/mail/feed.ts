// The messages of the mail log view. `GET /api/mail` answers oldest first,
// so a newest-first log needs the full list: the feed follows every cursor
// once, then asks only for messages after the newest ID it has (the list is
// `id > cursor`). A `mail` event pulls the new messages and the event's
// thread again (a reply marks the original `replied`; a pause changes
// `thread_paused` on the whole thread). `onNew` gets the IDs of the
// messages that a pull found for the first time, for the live mode.

import { batch, createEffect, createMemo, createSignal, on } from "solid-js";
import type { Api } from "../../api/client";
import type { MailQuery, Message } from "../../api/types";
import { newKeys } from "../../components/live";
import { collectAll } from "../../lib/paging";
import { mergeMessages, newestId } from "./log";

const PAGE = 1000;
const MAX_PAGES = 20;

export function createMailFeed(api: Pick<Api, "mail">, query: () => MailQuery, opts: { onNew?: (ids: string[]) => void } = {}) {
  const [messages, setMessages] = createSignal<Message[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  /** The newest ID that the server query returned: the cursor of the next pull. */
  let cursor: string | undefined;
  let generation = 0;

  const all = (q: MailQuery, from?: string) => collectAll((c) => api.mail.list({ ...q, cursor: c ?? from, limit: PAGE }), MAX_PAGES);

  const load = async () => {
    const gen = ++generation;
    const q = query();
    setLoading(true);
    try {
      const rows = await all(q);
      if (gen !== generation) return;
      cursor = newestId(rows);
      setMessages(mergeMessages([], rows));
      setLoaded(true);
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  /** The messages after the cursor, and the given threads again. */
  const pull = async (threads: string[] = []) => {
    const gen = generation;
    const q = query();
    try {
      const [fresh, ...again] = await Promise.all([all(q, cursor), ...threads.map((thread) => all({ thread }))]);
      if (gen !== generation) return;
      const next = newestId(fresh!);
      if (next && (!cursor || next > cursor)) cursor = next;
      const cur = messages();
      const merged = mergeMessages(cur, [...fresh!, ...again.flat()]);
      const ids = newKeys(cur, merged, (m) => m.id);
      // One update: a held message never shows for a moment.
      batch(() => {
        setMessages(merged);
        if (ids.length) opts.onNew?.(ids);
      });
    } catch (e) {
      if (gen === generation) setError(e);
    }
  };

  // A memo compares the key, so a filter change that keeps the server query does not reload.
  const key = createMemo(() => JSON.stringify(query()));
  createEffect(on(key, () => void load()));

  return {
    /** Newest first. */
    messages,
    loading,
    loaded,
    error,
    reload: load,
    pull,
    /** Put changed messages in without a request: a read, a reply, a new message. */
    upsert: (changed: Message[]) => setMessages((cur) => mergeMessages(cur, changed)),
  };
}

export type MailFeed = ReturnType<typeof createMailFeed>;

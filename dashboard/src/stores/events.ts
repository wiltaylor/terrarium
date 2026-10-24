// One connection to the event firehose, shared by every view.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { ConnectionState, EventStream } from "../api/sockets";
import type { AgentEvent } from "../api/types";

export const RECENT_LIMIT = 300;

export function createEventsStore(api: Pick<Api, "events">) {
  const [recent, setRecent] = createSignal<AgentEvent[]>([]);
  const [state, setState] = createSignal<ConnectionState>("closed");
  const listeners = new Set<(e: AgentEvent) => void>();
  let stream: EventStream | null = null;

  const start = () => {
    if (stream) return;
    stream = api.events();
    stream.onState(setState);
    stream.subscribe((e) => {
      setRecent((prev) => [e, ...prev].slice(0, RECENT_LIMIT));
      listeners.forEach((fn) => fn(e));
    });
  };

  return {
    recent,
    state,
    start,
    /** Listen to every event. Returns an unsubscribe function. */
    on(fn: (e: AgentEvent) => void): () => void {
      start();
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    stop() {
      stream?.close();
      stream = null;
    },
  };
}

export type EventsStore = ReturnType<typeof createEventsStore>;

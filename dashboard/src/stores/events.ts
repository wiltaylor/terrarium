// One connection to the event firehose, shared by every view. It reconnects
// with backoff and resumes after the last `seq` it saw.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { ConnectionState, EventStream, StreamStatus } from "../api/sockets";
import type { AgentEvent } from "../api/types";

export const RECENT_LIMIT = 300;

/** Add an event to the newest-first buffer. Pure. Drops a `seq` that is there already. */
export function pushRecent(prev: AgentEvent[], e: AgentEvent, limit = RECENT_LIMIT): AgentEvent[] {
  if (prev.some((x) => x.id === e.id)) return prev;
  return [e, ...prev].slice(0, limit);
}

export function createEventsStore(api: Pick<Api, "events">) {
  const [recent, setRecent] = createSignal<AgentEvent[]>([]);
  const [state, setState] = createSignal<ConnectionState>("closed");
  const [status, setStatus] = createSignal<StreamStatus>({ state: "closed", attempt: 0, retryAt: null, lastSeq: 0 });
  /** True once the socket has opened at least once. */
  const [everOpen, setEverOpen] = createSignal(false);
  const listeners = new Set<(e: AgentEvent) => void>();
  let stream: EventStream | null = null;

  const start = () => {
    if (stream) return;
    stream = api.events.stream();
    stream.onState((s, st) => {
      setState(s);
      setStatus(st);
      if (s === "open") setEverOpen(true);
    });
    stream.subscribe((e) => {
      setRecent((prev) => pushRecent(prev, e));
      setStatus((st) => ({ ...st, lastSeq: Math.max(st.lastSeq, e.seq ?? 0) }));
      listeners.forEach((fn) => fn(e));
    });
  };

  return {
    recent,
    state,
    status,
    /** The socket is down and was up before, or never came up after a retry. */
    disconnected: () => state() !== "open" && (everOpen() || status().attempt > 0),
    start,
    reconnect: () => stream?.reconnect(),
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

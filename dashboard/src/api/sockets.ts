// Socket contracts shared by the real daemon and the mock.
//
// PTY protocol (agent terminal and login terminal):
//   server -> client: raw terminal output, binary frames (text frames are accepted too).
//   client -> server: JSON text frames, one of
//     {"type":"input","data":"<keystrokes>"}
//     {"type":"resize","cols":<n>,"rows":<n>}
//
// Events protocol (`/api/events`, `/api/agents/{id}/events`):
//   server -> client: one JSON `AgentEvent` per text frame.

import type { AgentEvent } from "./types";

/** The subset of `WebSocket` the terminal needs. A real `WebSocket` fits it; the mock implements it. */
export interface PtySocket {
  readonly readyState: number;
  binaryType: BinaryType;
  send(data: string): void;
  close(): void;
  addEventListener(type: "open" | "message" | "close" | "error", fn: (ev: { data?: unknown }) => void): void;
  removeEventListener(type: "open" | "message" | "close" | "error", fn: (ev: { data?: unknown }) => void): void;
}

export const ptyInput = (data: string): string => JSON.stringify({ type: "input", data });
export const ptyResize = (cols: number, rows: number): string =>
  JSON.stringify({ type: "resize", cols, rows });

export type PtyControl = { type: "input"; data: string } | { type: "resize"; cols: number; rows: number };

export function parsePtyControl(raw: string): PtyControl | null {
  try {
    const msg = JSON.parse(raw) as Partial<PtyControl> & Record<string, unknown>;
    if (msg.type === "input" && typeof msg.data === "string") return { type: "input", data: msg.data };
    if (msg.type === "resize" && typeof msg.cols === "number" && typeof msg.rows === "number")
      return { type: "resize", cols: msg.cols, rows: msg.rows };
  } catch {
    // not JSON
  }
  return null;
}

export type ConnectionState = "connecting" | "open" | "closed";

export interface StreamStatus {
  state: ConnectionState;
  /** Failed connects since the last open. */
  attempt: number;
  /** When the next connect starts, while closed. */
  retryAt: number | null;
  /** The highest `seq` seen. The next connect asks for `?since=` this. */
  lastSeq: number;
}

export interface EventStream {
  subscribe(fn: (e: AgentEvent) => void): () => void;
  onState(fn: (s: ConnectionState, status: StreamStatus) => void): () => void;
  /** Connect now, without waiting for the backoff. */
  reconnect(): void;
  close(): void;
}

/** Backoff for the next connect: doubles from `min` to `max`, with up to 20% jitter. */
export function backoffDelay(attempt: number, min = 500, max = 15_000, random: () => number = Math.random): number {
  const base = Math.min(max, min * 2 ** Math.max(0, attempt));
  return Math.round(base * (0.8 + 0.2 * random()));
}

/** The parts of `WebSocket` the event stream uses. */
export interface EventSocket {
  addEventListener(type: "open" | "close" | "error", fn: () => void): void;
  addEventListener(type: "message", fn: (ev: { data: unknown }) => void): void;
  close(): void;
}

export interface EventStreamOptions {
  makeSocket?: (url: string) => EventSocket;
  /** The `seq` to start after. Absent: the daemon's default, the whole log. */
  since?: number;
  minDelay?: number;
  maxDelay?: number;
  random?: () => number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (t: unknown) => void;
  now?: () => number;
}

/**
 * A WebSocket event stream that reconnects with backoff. It remembers the
 * last `seq` and resumes with `?since=<seq>`, so a drop loses no event and a
 * frame that comes twice is dropped (docs/api.md §9.1).
 */
export function createWsEventStream(url: (since: number | undefined) => string, opts: EventStreamOptions = {}): EventStream {
  const makeSocket = opts.makeSocket ?? ((u: string) => new WebSocket(u) as unknown as EventSocket);
  const setTimer = opts.setTimer ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));
  const clearTimer = opts.clearTimer ?? ((t: unknown) => clearTimeout(t as ReturnType<typeof setTimeout>));
  const now = opts.now ?? Date.now;
  const listeners = new Set<(e: AgentEvent) => void>();
  const stateListeners = new Set<(s: ConnectionState, status: StreamStatus) => void>();
  let socket: EventSocket | null = null;
  let closed = false;
  let timer: unknown;
  const status: StreamStatus = { state: "connecting", attempt: 0, retryAt: null, lastSeq: opts.since ?? 0 };
  let started = opts.since !== undefined;

  const setState = (s: ConnectionState) => {
    status.state = s;
    const snap = { ...status };
    stateListeners.forEach((fn) => fn(s, snap));
  };

  const schedule = () => {
    const delay = backoffDelay(status.attempt, opts.minDelay, opts.maxDelay, opts.random);
    status.attempt++;
    status.retryAt = now() + delay;
    setState("closed");
    timer = setTimer(connect, delay);
  };

  function connect() {
    if (closed) return;
    timer = undefined;
    status.retryAt = null;
    setState("connecting");
    const ws = makeSocket(url(started ? status.lastSeq : undefined));
    socket = ws;
    let opened = false;
    let done = false;
    ws.addEventListener("open", () => {
      if (socket !== ws) return;
      opened = true;
      status.attempt = 0;
      setState("open");
    });
    ws.addEventListener("message", (ev) => {
      if (socket !== ws || typeof ev.data !== "string") return;
      let e: AgentEvent;
      try {
        e = JSON.parse(ev.data) as AgentEvent;
      } catch {
        return; // ignore malformed frames
      }
      if (typeof e.seq === "number") {
        if (started && e.seq <= status.lastSeq) return; // seen already
        status.lastSeq = e.seq;
        started = true;
      }
      listeners.forEach((fn) => fn(e));
    });
    const lost = () => {
      if (done || socket !== ws) return;
      done = true;
      socket = null;
      if (closed) return setState("closed");
      if (opened) status.attempt = 0;
      schedule();
    };
    ws.addEventListener("close", lost);
    ws.addEventListener("error", lost);
  }
  connect();

  return {
    subscribe(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onState(fn) {
      stateListeners.add(fn);
      fn(status.state, { ...status });
      return () => stateListeners.delete(fn);
    },
    reconnect() {
      if (closed || socket) return;
      if (timer !== undefined) clearTimer(timer);
      connect();
    },
    close() {
      closed = true;
      if (timer !== undefined) clearTimer(timer);
      const ws = socket;
      socket = null;
      ws?.close();
      setState("closed");
    },
  };
}

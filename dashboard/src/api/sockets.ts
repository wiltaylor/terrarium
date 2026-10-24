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

export interface EventStream {
  subscribe(fn: (e: AgentEvent) => void): () => void;
  onState(fn: (s: ConnectionState) => void): () => void;
  close(): void;
}

/** A WebSocket event stream that reconnects with backoff. */
export function createWsEventStream(
  url: () => string,
  makeSocket: (url: string) => WebSocket = (u) => new WebSocket(u),
): EventStream {
  const listeners = new Set<(e: AgentEvent) => void>();
  const stateListeners = new Set<(s: ConnectionState) => void>();
  let socket: WebSocket | null = null;
  let closed = false;
  let delay = 500;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const setState = (s: ConnectionState) => stateListeners.forEach((fn) => fn(s));

  const connect = () => {
    if (closed) return;
    setState("connecting");
    socket = makeSocket(url());
    socket.addEventListener("open", () => {
      delay = 500;
      setState("open");
    });
    socket.addEventListener("message", (ev: MessageEvent) => {
      if (typeof ev.data !== "string") return;
      try {
        const e = JSON.parse(ev.data) as AgentEvent;
        listeners.forEach((fn) => fn(e));
      } catch {
        // ignore malformed frames
      }
    });
    socket.addEventListener("close", () => {
      setState("closed");
      if (closed) return;
      timer = setTimeout(connect, delay);
      delay = Math.min(delay * 2, 15_000);
    });
  };
  connect();

  return {
    subscribe(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onState(fn) {
      stateListeners.add(fn);
      return () => stateListeners.delete(fn);
    },
    close() {
      closed = true;
      clearTimeout(timer);
      socket?.close();
    },
  };
}

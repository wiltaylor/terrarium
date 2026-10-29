// Socket contracts shared by the real daemon and the mock.
//
// PTY protocol (agent terminal and login terminal):
//   server -> client: raw terminal output, binary frames (text frames are accepted too).
//   client -> server: JSON text frames, one of
//     {"type":"input","data":"<keystrokes>"}
//     {"type":"resize","cols":<n>,"rows":<n>}
//
// Events protocol (`/api/events`, `/api/agents/{id}/events`):
//   server -> client: one JSON `AgentEvent` per text frame. With `?audit=1`
//   (firehose only), also `{"kind":"audit","audit":<row>}` frames with no
//   `seq`: new audit rows that the caller may read (docs/api.md §9.1).
//
// Authentication (docs/security-review.md F4): a browser cannot set an
// `Authorization` header on a WebSocket. Two modes, picked by `AuthSocket`
// for every socket (agent PTY, login PTY, target exec console, events):
// - Cookie session: no subprotocol. The browser sends the session cookie
//   and the page's `Origin` on the upgrade, and the daemon checks both.
// - Bearer: the client offers the token as the subprotocol `bearer.<token>`
//   (`Sec-WebSocket-Protocol`), and the daemon echoes it back.
// The URL never carries a token.

import type { AgentEvent, AuditEntry } from "./types";

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
  /** The `audit` frames: new or changed audit rows. They do not move `lastSeq`. */
  onAudit(fn: (a: AuditEntry) => void): () => void;
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

/** The audit row of an `audit` frame (no `seq`), or undefined for an event frame. */
export function auditOf(frame: unknown): AuditEntry | undefined {
  const f = frame as { kind?: unknown; seq?: unknown; audit?: unknown } | null;
  if (!f || f.kind !== "audit" || typeof f.seq === "number") return undefined;
  const a = f.audit as AuditEntry | undefined;
  return a && typeof a === "object" && typeof a.id === "string" ? a : undefined;
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
  const auditListeners = new Set<(a: AuditEntry) => void>();
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
      const audit = auditOf(e);
      if (audit) return auditListeners.forEach((fn) => fn(audit));
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
    onAudit(fn) {
      auditListeners.add(fn);
      return () => auditListeners.delete(fn);
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

// ----- authentication -----

export const BEARER_PREFIX = "bearer.";
export const bearerProtocol = (token: string): string => `${BEARER_PREFIX}${token}`;

/** A subprotocol must be an RFC 7230 token, or `new WebSocket` throws. */
const SUBPROTOCOL = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
export const isSubprotocolSafe = (token: string): boolean => SUBPROTOCOL.test(bearerProtocol(token));

/** What `AuthSocket` needs from a `WebSocket`. A real one fits; the mock implements it. */
export interface RawSocket {
  readonly readyState: number;
  readonly protocol: string;
  binaryType: BinaryType;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: string, fn: (ev: SocketEvent) => void): void;
  removeEventListener(type: string, fn: (ev: SocketEvent) => void): void;
}

export interface SocketEvent {
  data?: unknown;
  /** On `close`: the close code. 1006 is an abnormal close, for example a refused handshake. */
  code?: number;
}

export type MakeRawSocket = (url: string, protocols?: string[]) => RawSocket;

export const browserSocket: MakeRawSocket = (url, protocols) => new WebSocket(url, protocols) as unknown as RawSocket;

export interface AuthSocketOptions {
  /** Bearer mode: offer `bearer.<token>`. Null: offer no subprotocol. */
  token: string | null;
  /** A cookie session is active. Only names the mode (`cookie`); the browser sends the cookie. */
  cookie?: boolean;
  make?: MakeRawSocket;
  /**
   * Dev only (`VITE_ALLOW_QUERY_TOKEN=1`). When the subprotocol handshake is
   * refused (closed with 1006 before open), try once more with `?token=`.
   * Off by default: the token must not reach URLs and access logs.
   */
  allowQueryToken?: boolean;
  /** Told when a socket falls back to `?token=`. */
  onQueryFallback?: (url: string) => void;
}

/** How a socket authenticates: the subprotocol, the dev `?token=` fallback, the session cookie, or nothing. */
export type SocketAuthMode = "subprotocol" | "query" | "cookie" | "none";

/** Why an `AuthSocket` failed before it opened, for tests and logs. */
export type AuthFailure = "refused" | "protocol_not_echoed" | "token_not_subprotocol" | null;

const CONNECTING = 0;
const CLOSED = 3;
const ABNORMAL = 1006;

/** Add `token=` to a ws URL. Only for the dev fallback. */
export function withQueryToken(url: string, token: string): string {
  const u = new URL(url);
  u.searchParams.set("token", token);
  return u.toString();
}

/**
 * A WebSocket that authenticates with the session cookie (no subprotocol),
 * or with the `bearer.<token>` subprotocol and a check that the server
 * echoed it. It fits `PtySocket` and `EventSocket`,
 * so the terminal and the event stream use it unchanged.
 */
export class AuthSocket implements PtySocket, EventSocket {
  /** How the open socket authenticated. */
  mode: SocketAuthMode = "none";
  failure: AuthFailure = null;

  private inner: RawSocket | null = null;
  private listeners = new Map<string, Set<(ev: SocketEvent) => void>>();
  private type: BinaryType = "blob";
  private opened = false;
  private closedByUser = false;
  private pendingError: SocketEvent | null = null;
  private triedQuery = false;
  private readonly make: MakeRawSocket;

  constructor(
    readonly url: string,
    private readonly opts: AuthSocketOptions,
  ) {
    this.make = opts.make ?? browserSocket;
    const token = opts.token;
    if (!token) {
      this.attach(this.make(url), opts.cookie ? "cookie" : "none");
    } else if (!isSubprotocolSafe(token)) {
      if (opts.allowQueryToken) this.fallback();
      else this.failSoon("token_not_subprotocol");
    } else {
      this.attach(this.make(url, [bearerProtocol(token)]), "subprotocol");
    }
  }

  get readyState(): number {
    if (this.failure && !this.inner) return CLOSED;
    return this.inner?.readyState ?? CONNECTING;
  }

  get protocol(): string {
    return this.inner?.protocol ?? "";
  }

  get binaryType(): BinaryType {
    return this.type;
  }

  set binaryType(t: BinaryType) {
    this.type = t;
    if (this.inner) this.inner.binaryType = t;
  }

  send(data: string): void {
    this.inner?.send(data);
  }

  close(): void {
    this.closedByUser = true;
    this.inner?.close();
  }

  addEventListener(type: "message", fn: (ev: { data: unknown }) => void): void;
  addEventListener(type: string, fn: (ev: SocketEvent) => void): void;
  addEventListener(type: string, fn: (ev: { data: unknown }) => void): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    // A message event always carries `data`; the other events do not read it.
    this.listeners.get(type)!.add(fn as (ev: SocketEvent) => void);
  }

  removeEventListener(type: string, fn: (ev: SocketEvent) => void): void {
    this.listeners.get(type)?.delete(fn);
  }

  private emit(type: string, ev: SocketEvent): void {
    for (const fn of [...(this.listeners.get(type) ?? [])]) fn(ev);
  }

  private failSoon(why: Exclude<AuthFailure, null>): void {
    this.failure = why;
    queueMicrotask(() => {
      this.emit("error", {});
      this.emit("close", { code: ABNORMAL });
    });
  }

  private fallback(): void {
    const token = this.opts.token!;
    this.triedQuery = true;
    this.opts.onQueryFallback?.(this.url);
    this.attach(this.make(withQueryToken(this.url, token)), "query");
  }

  private attach(ws: RawSocket, mode: SocketAuthMode): void {
    this.inner = ws;
    ws.binaryType = this.type;
    const live = () => this.inner === ws;
    ws.addEventListener("open", (ev) => {
      if (!live()) return;
      if (mode === "subprotocol" && ws.protocol !== bearerProtocol(this.opts.token!)) {
        // The server did not take the subprotocol. Browsers fail such a
        // handshake on their own; this covers proxies and other clients.
        this.failure = "protocol_not_echoed";
        this.inner = null;
        ws.close();
        this.emit("error", {});
        this.emit("close", { code: ABNORMAL });
        return;
      }
      this.opened = true;
      this.mode = mode;
      this.emit("open", ev);
    });
    ws.addEventListener("message", (ev) => {
      if (live()) this.emit("message", ev);
    });
    ws.addEventListener("error", (ev) => {
      if (!live()) return;
      // Hold an error before open: the close code decides between a retry with
      // `?token=` and a failure.
      if (!this.opened) this.pendingError = ev;
      else this.emit("error", ev);
    });
    ws.addEventListener("close", (ev) => {
      if (!live()) return;
      if (!this.opened) {
        const refused = ev.code === undefined || ev.code === ABNORMAL;
        if (refused && mode === "subprotocol" && this.opts.allowQueryToken && !this.triedQuery && !this.closedByUser) {
          this.pendingError = null;
          this.fallback();
          return;
        }
        this.failure ??= "refused";
        if (this.pendingError) this.emit("error", this.pendingError);
        this.pendingError = null;
      }
      this.emit("close", ev);
    });
  }
}

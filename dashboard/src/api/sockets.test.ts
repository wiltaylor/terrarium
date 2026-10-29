import { describe, expect, it } from "vitest";
import {
  AuthSocket,
  backoffDelay,
  bearerProtocol,
  createWsEventStream,
  isSubprotocolSafe,
  withQueryToken,
  type AuthSocketOptions,
  type ConnectionState,
  type EventSocket,
  type RawSocket,
  type SocketEvent,
} from "./sockets";
import type { AgentEvent } from "./types";

/** A socket the test drives by hand. */
class FakeSocket implements EventSocket {
  handlers: Record<string, ((ev: { data: unknown }) => void)[]> = {};
  closed = false;
  constructor(readonly url: string) {}
  addEventListener(type: string, fn: (ev: { data: unknown }) => void) {
    (this.handlers[type] ??= []).push(fn);
  }
  close() {
    this.closed = true;
  }
  fire(type: "open" | "close" | "error" | "message", data?: unknown) {
    for (const fn of this.handlers[type] ?? []) fn({ data });
  }
  frame(e: Partial<AgentEvent>) {
    this.fire("message", JSON.stringify({ id: `E${e.seq}`, at: "", kind: "status", summary: "", data: {}, ...e }));
  }
}

function harness(since?: number) {
  const sockets: FakeSocket[] = [];
  const timers: { fn: () => void; ms: number; cancelled: boolean }[] = [];
  const states: ConnectionState[] = [];
  const seen: number[] = [];
  const stream = createWsEventStream((s) => `ws://d/api/events${s === undefined ? "" : `?since=${s}`}`, {
    since,
    makeSocket: (url) => {
      const s = new FakeSocket(url);
      sockets.push(s);
      return s;
    },
    random: () => 1,
    setTimer: (fn, ms) => {
      const t = { fn, ms, cancelled: false };
      timers.push(t);
      return t;
    },
    clearTimer: (t) => ((t as { cancelled: boolean }).cancelled = true),
    now: () => 0,
  });
  stream.onState((s) => states.push(s));
  stream.subscribe((e) => seen.push(e.seq));
  const fireTimer = () => {
    const t = timers.filter((x) => !x.cancelled).at(-1)!;
    t.cancelled = true;
    t.fn();
  };
  return { stream, sockets, timers, states, seen, fireTimer };
}

describe("backoffDelay", () => {
  it("doubles from min to max with up to 20% jitter", () => {
    expect([0, 1, 2, 3].map((a) => backoffDelay(a, 500, 15_000, () => 1))).toEqual([500, 1000, 2000, 4000]);
    expect(backoffDelay(20, 500, 15_000, () => 1)).toBe(15_000);
    expect(backoffDelay(0, 500, 15_000, () => 0)).toBe(400);
  });
});

describe("createWsEventStream", () => {
  it("resumes after the last seq it saw, and drops repeated frames", () => {
    const h = harness();
    expect(h.sockets[0]!.url).toBe("ws://d/api/events");
    h.sockets[0]!.fire("open");
    h.sockets[0]!.frame({ seq: 1 });
    h.sockets[0]!.frame({ seq: 2 });
    h.sockets[0]!.fire("close");
    expect(h.states.at(-1)).toBe("closed");
    h.fireTimer();
    expect(h.sockets[1]!.url).toBe("ws://d/api/events?since=2");
    h.sockets[1]!.fire("open");
    h.sockets[1]!.frame({ seq: 2 }); // seen already
    h.sockets[1]!.frame({ seq: 3 });
    expect(h.seen).toEqual([1, 2, 3]);
  });

  it("gives audit frames to onAudit, and they do not move the resume point", () => {
    const h = harness();
    const rows: string[] = [];
    h.stream.onAudit((a) => rows.push(a.action));
    h.sockets[0]!.fire("open");
    h.sockets[0]!.frame({ seq: 4 });
    h.sockets[0]!.fire("message", JSON.stringify({ kind: "audit", audit: { id: "A1", seq: 90, at: "", actor: "operator", action: "agent.start", target: "x" } }));
    expect(rows).toEqual(["agent.start"]);
    expect(h.seen).toEqual([4]);
    h.sockets[0]!.fire("close");
    h.fireTimer();
    expect(h.sockets[1]!.url).toBe("ws://d/api/events?since=4");
  });

  it("backs off on failed connects and resets after an open", () => {
    const h = harness(10);
    expect(h.sockets[0]!.url).toBe("ws://d/api/events?since=10");
    h.sockets[0]!.fire("error");
    h.sockets[0]!.fire("close"); // one loss, not two
    expect(h.timers.map((t) => t.ms)).toEqual([500]);
    h.fireTimer();
    h.sockets[1]!.fire("close");
    h.fireTimer();
    h.sockets[2]!.fire("close");
    expect(h.timers.map((t) => t.ms)).toEqual([500, 1000, 2000]);
    h.fireTimer();
    h.sockets[3]!.fire("open");
    h.sockets[3]!.fire("close");
    expect(h.timers.at(-1)!.ms).toBe(500);
    expect(h.states).toContain("open");
  });

  it("reconnects at once on reconnect(), and close() stops for good", () => {
    const h = harness();
    h.sockets[0]!.fire("close");
    expect(h.sockets).toHaveLength(1);
    h.stream.reconnect();
    expect(h.sockets).toHaveLength(2);
    expect(h.timers[0]!.cancelled).toBe(true);
    h.stream.close();
    expect(h.sockets[1]!.closed).toBe(true);
    h.sockets[1]!.fire("close");
    expect(h.timers.filter((t) => !t.cancelled)).toHaveLength(0);
    expect(h.states.at(-1)).toBe("closed");
  });

  it("ignores frames from an old socket and malformed frames", () => {
    const h = harness();
    const old = h.sockets[0]!;
    old.fire("close");
    h.fireTimer();
    old.frame({ seq: 9 });
    h.sockets[1]!.fire("message", "not json");
    h.sockets[1]!.fire("message", new ArrayBuffer(1));
    expect(h.seen).toEqual([]);
  });
});

/** A raw WebSocket the AuthSocket tests drive by hand. */
class FakeRaw implements RawSocket {
  readyState = 0;
  protocol = "";
  binaryType: BinaryType = "blob";
  closed = false;
  sent: string[] = [];
  private h: Record<string, ((ev: SocketEvent) => void)[]> = {};
  constructor(
    readonly url: string,
    readonly protocols?: string[],
  ) {}
  addEventListener(type: string, fn: (ev: SocketEvent) => void) {
    (this.h[type] ??= []).push(fn);
  }
  removeEventListener(type: string, fn: (ev: SocketEvent) => void) {
    this.h[type] = (this.h[type] ?? []).filter((f) => f !== fn);
  }
  send(d: string) {
    this.sent.push(d);
  }
  close() {
    this.closed = true;
  }
  fire(type: string, ev: SocketEvent = {}) {
    for (const fn of this.h[type] ?? []) fn(ev);
  }
  /** The server accepts and echoes `protocol`. */
  accept(protocol = this.protocols?.[0] ?? "") {
    this.protocol = protocol;
    this.readyState = 1;
    this.fire("open");
  }
  /** The upgrade fails before the handshake, as a browser reports it. */
  refuse(code = 1006) {
    this.readyState = 3;
    this.fire("error");
    this.fire("close", { code });
  }
}

function authHarness(opts: Partial<AuthSocketOptions> = {}, url = "wss://d/api/agents/A1/pty") {
  const raws: FakeRaw[] = [];
  const seen: string[] = [];
  const fallbacks: string[] = [];
  const s = new AuthSocket(url, {
    token: "abc123",
    make: (u, p) => {
      const r = new FakeRaw(u, p);
      raws.push(r);
      return r;
    },
    onQueryFallback: (u) => fallbacks.push(u),
    ...opts,
  });
  for (const t of ["open", "message", "error", "close"]) s.addEventListener(t, (ev) => seen.push(t === "close" ? `close:${ev.code}` : t));
  return { s, raws, seen, fallbacks };
}

describe("AuthSocket", () => {
  it("offers bearer.<token> as the subprotocol, with no token in the URL", () => {
    const h = authHarness();
    expect(h.raws).toHaveLength(1);
    expect(h.raws[0]!.url).toBe("wss://d/api/agents/A1/pty");
    expect(h.raws[0]!.protocols).toEqual([bearerProtocol("abc123")]);
    h.s.binaryType = "arraybuffer";
    expect(h.raws[0]!.binaryType).toBe("arraybuffer");
    h.raws[0]!.accept();
    expect(h.s.readyState).toBe(1);
    expect(h.s.mode).toBe("subprotocol");
    h.raws[0]!.fire("message", { data: "x" });
    h.s.send("hi");
    expect(h.raws[0]!.sent).toEqual(["hi"]);
    expect(h.seen).toEqual(["open", "message"]);
  });

  it("fails when the server does not echo the subprotocol", () => {
    const h = authHarness();
    h.raws[0]!.accept("");
    expect(h.s.failure).toBe("protocol_not_echoed");
    expect(h.raws[0]!.closed).toBe(true);
    expect(h.seen).toEqual(["error", "close:1006"]);
    expect(h.s.readyState).toBe(3);
  });

  it("does not fall back to ?token= by default", () => {
    const h = authHarness();
    h.raws[0]!.refuse();
    expect(h.raws).toHaveLength(1);
    expect(h.s.failure).toBe("refused");
    expect(h.seen).toEqual(["error", "close:1006"]);
  });

  it("falls back to ?token= once, on a refused handshake, when the dev flag is on", () => {
    const h = authHarness({ allowQueryToken: true });
    h.s.binaryType = "arraybuffer";
    h.raws[0]!.refuse();
    expect(h.seen).toEqual([]); // the first failure is not reported
    expect(h.raws).toHaveLength(2);
    expect(h.raws[1]!.url).toBe("wss://d/api/agents/A1/pty?token=abc123");
    expect(h.raws[1]!.protocols).toBeUndefined();
    expect(h.raws[1]!.binaryType).toBe("arraybuffer");
    expect(h.fallbacks).toEqual(["wss://d/api/agents/A1/pty"]);
    h.raws[1]!.accept("");
    expect(h.s.mode).toBe("query");
    expect(h.seen).toEqual(["open"]);
  });

  it("stops after the fallback fails too, and does not fall back on other close codes", () => {
    const h = authHarness({ allowQueryToken: true });
    h.raws[0]!.refuse();
    h.raws[1]!.refuse();
    expect(h.raws).toHaveLength(2);
    expect(h.seen).toEqual(["error", "close:1006"]);
    const policy = authHarness({ allowQueryToken: true });
    policy.raws[0]!.refuse(1008);
    expect(policy.raws).toHaveLength(1);
    expect(policy.seen).toEqual(["error", "close:1008"]);
  });

  it("does not fall back after an open, or after the client closed it", () => {
    const h = authHarness({ allowQueryToken: true });
    h.raws[0]!.accept();
    h.raws[0]!.fire("close", { code: 1006 });
    expect(h.raws).toHaveLength(1);
    expect(h.seen).toEqual(["open", "close:1006"]);
    const c = authHarness({ allowQueryToken: true });
    c.s.close();
    c.raws[0]!.refuse();
    expect(c.raws).toHaveLength(1);
  });

  it("refuses a token that cannot be a subprotocol, unless the dev flag is on", async () => {
    expect(isSubprotocolSafe("a b")).toBe(false);
    expect(isSubprotocolSafe("deadbeef")).toBe(true);
    const h = authHarness({ token: "has space" });
    expect(h.raws).toHaveLength(0);
    await Promise.resolve();
    expect(h.s.failure).toBe("token_not_subprotocol");
    expect(h.seen).toEqual(["error", "close:1006"]);
    const q = authHarness({ token: "has space", allowQueryToken: true });
    expect(q.raws[0]!.url).toBe("wss://d/api/agents/A1/pty?token=has+space");
  });

  it("offers no subprotocol without a token", () => {
    const h = authHarness({ token: null });
    expect(h.raws[0]!.protocols).toBeUndefined();
  });

  it("keeps other query parameters when it adds the token", () => {
    expect(withQueryToken("ws://d/api/events?since=4", "t")).toBe("ws://d/api/events?since=4&token=t");
  });
});

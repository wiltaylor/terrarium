import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { createWsEventStream, type EventSocket } from "../api/sockets";
import type { AgentEvent } from "../api/types";
import { createEventsStore, firstSince, pushRecent, RECENT_LIMIT } from "./events";

const ev = (seq: number): AgentEvent => ({ seq, id: `E${seq}`, at: "", kind: "status", summary: "", data: {} });

class Sock implements EventSocket {
  h: Record<string, ((ev: { data: unknown }) => void)[]> = {};
  constructor(readonly url: string) {}
  addEventListener(t: string, fn: (ev: { data: unknown }) => void) {
    (this.h[t] ??= []).push(fn);
  }
  close() {}
  fire(t: string, data?: unknown) {
    for (const fn of this.h[t] ?? []) fn({ data });
  }
}

describe("pushRecent", () => {
  it("keeps newest first, drops a repeated id, and caps the buffer", () => {
    let r: AgentEvent[] = [];
    for (let i = 1; i <= 5; i++) r = pushRecent(r, ev(i), 3);
    expect(r.map((e) => e.seq)).toEqual([5, 4, 3]);
    expect(pushRecent(r, ev(4), 3)).toBe(r);
  });
});

describe("createEventsStore", () => {
  const fakeApi = (latest: number | null, sockets: Sock[], timers: (() => void)[]) => ({
    events: {
      stream: (o: { since?: number } = {}) =>
        createWsEventStream((s) => `ws://x${s === undefined ? "" : `?since=${s}`}`, {
          since: o.since,
          makeSocket: (u) => {
            const s = new Sock(u);
            sockets.push(s);
            return s;
          },
          setTimer: (fn) => (timers.push(fn), timers.length),
          clearTimer: () => {},
        }),
      history: async () => ({ items: [], next: null }),
      latest: async () => latest,
    },
  });
  const flush = () => new Promise((r) => setTimeout(r, 0));

  it("starts after the newest events when the daemon has GET /api/events/latest", async () => {
    expect(firstSince(null)).toBeUndefined();
    expect(firstSince(50)).toBe(0);
    expect(firstSince(1000, 300)).toBe(700);
    const sockets: Sock[] = [];
    const store = createRoot(() => createEventsStore(fakeApi(1000, sockets, [])));
    store.start();
    await flush();
    expect(sockets[0]!.url).toBe(`ws://x?since=${1000 - RECENT_LIMIT}`);
    store.stop();
  });

  it("shows disconnected after a drop, resumes with since, and fans out events", async () => {
    const sockets: Sock[] = [];
    const timers: (() => void)[] = [];
    const api = fakeApi(null, sockets, timers);
    await createRoot(async (dispose) => {
      const store = createEventsStore(api);
      const got: number[] = [];
      store.on((e) => got.push(e.seq));
      await flush();
      expect(sockets[0]!.url).toBe("ws://x");
      expect(store.disconnected()).toBe(false);
      sockets[0]!.fire("open");
      sockets[0]!.fire("message", JSON.stringify(ev(7)));
      expect(store.state()).toBe("open");
      expect(store.status().lastSeq).toBe(7);
      sockets[0]!.fire("close");
      expect(store.disconnected()).toBe(true);
      expect(store.status().retryAt).not.toBeNull();
      timers.pop()!();
      expect(sockets[1]!.url).toBe("ws://x?since=7");
      sockets[1]!.fire("open");
      expect(store.disconnected()).toBe(false);
      sockets[1]!.fire("message", JSON.stringify(ev(8)));
      expect(got).toEqual([7, 8]);
      expect(store.recent().map((e) => e.seq)).toEqual([8, 7]);
      store.stop();
      dispose();
    });
  });
});

import { createRoot, createSignal } from "solid-js";
import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import { fakeUlid } from "../../api/mock/data";
import type { MailQuery } from "../../api/types";
import { createMailFeed } from "./feed";
import { annotate, applyFilter, DEFAULT_FILTER, threadOf } from "./log";

const wait = (ms = 10) => new Promise((r) => setTimeout(r, ms));

/** The mock, with pages of 3 so the feed must follow the cursor; `calls` records each query. */
function setup() {
  const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
  const calls: MailQuery[] = [];
  const small = { mail: { ...api.mail, list: (q: MailQuery = {}) => (calls.push(q), api.mail.list({ ...q, limit: 3 })) } };
  return { api, small, calls };
}

describe("mail feed", () => {
  it("follows every cursor and lists newest first", async () => {
    const { api, small, calls } = setup();
    await createRoot(async (dispose) => {
      const feed = createMailFeed(small, () => ({}));
      await wait();
      expect(feed.loaded()).toBe(true);
      expect(feed.messages()).toHaveLength(api.world.mail.length);
      expect(calls.length).toBe(Math.ceil(api.world.mail.length / 3));
      const times = feed.messages().map((m) => m.created_at);
      expect(times).toEqual(times.slice().sort().reverse());
      dispose();
    });
  });

  it("sends the server part of the filter, and reloads when it changes", async () => {
    const { small, calls } = setup();
    await createRoot(async (dispose) => {
      const [q, setQ] = createSignal<MailQuery>({ to: "operator" });
      const feed = createMailFeed(small, q);
      await wait();
      expect(feed.messages().every((m) => m.to === "operator")).toBe(true);
      expect(calls[0]).toMatchObject({ to: "operator" });
      setQ({ to: "terrarium/lead" });
      await wait();
      expect(feed.messages().length).toBeGreaterThan(0);
      expect(feed.messages().every((m) => m.to_name === "terrarium/lead")).toBe(true);
      // The same query again (another filter changed in the browser) does not reload.
      const n = calls.length;
      setQ({ to: "terrarium/lead" });
      await wait();
      expect(calls.length).toBe(n);
      dispose();
    });
  });

  it("pulls only the messages after its newest ID, and puts them on top", async () => {
    const { api, small, calls } = setup();
    await createRoot(async (dispose) => {
      const feed = createMailFeed(small, () => ({}));
      await wait();
      const before = feed.messages().length;
      const newest = feed.messages()[0]!.id;
      const sent = await api.mail.send({ to: "terrarium/lead", subject: "Ping", body: "Status?" });
      calls.length = 0;
      await feed.pull();
      expect(calls[0]!.cursor).toBe(newest);
      expect(calls).toHaveLength(1);
      expect(feed.messages()).toHaveLength(before + 1);
      expect(feed.messages()[0]!.id).toBe(sent.id);
      // A second pull finds nothing new and changes nothing.
      await feed.pull();
      expect(feed.messages()).toHaveLength(before + 1);
      dispose();
    });
  });

  it("pulls a thread again, so a reply marks the original replied", async () => {
    const { api, small } = setup();
    await createRoot(async (dispose) => {
      const feed = createMailFeed(small, () => ({ to: "operator" }));
      await wait();
      const th = fakeUlid("th", 2);
      await api.mail.reply(fakeUlid("msg", 3), "Approved.");
      await feed.pull([th]);
      const thread = threadOf(feed.messages(), th);
      expect(thread.map((m) => [m.from_name, m.state])).toEqual([
        ["terrarium/lead", "replied"],
        ["operator", "queued"],
      ]);
      // The reply is not to the operator; the filter in the browser keeps it out of a "to operator" view.
      const shown = applyFilter(annotate(feed.messages()), { ...DEFAULT_FILTER, attention: false, kind: "to_operator" }, Date.now());
      expect(shown.every((e) => e.message.to === "operator")).toBe(true);
      dispose();
    });
  });
});

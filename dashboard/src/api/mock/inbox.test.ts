// The operator's inbox in the mock: the items, the counts, the mail
// acknowledge, and the release and resume of held mail.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import { fakeUlid } from "./data";
import type { AgentEvent } from "../types";
import { createInboxStore } from "../../stores/inbox";
import { criticalBanner } from "../../features/inbox/critical";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));
const listen = async (api: ReturnType<typeof mock>) => {
  const frames: AgentEvent[] = [];
  const s = api.events.stream({ since: api.world.seq });
  s.subscribe((e) => frames.push(e));
  await wait();
  return { frames, close: () => s.close() };
};
const loop = fakeUlid("th", 7);

describe("the inbox in the mock", () => {
  it("lists the open items, urgent first, then the newest, with the daemon's fields", async () => {
    const api = mock();
    const open = await api.inbox.list();
    expect(open.length).toBe(7);
    expect(open[0]).toMatchObject({ kind: "question", urgent: true, open: true });
    expect(open.slice(1).map((i) => i.at)).toEqual(open.slice(1).map((i) => i.at).sort().reverse());
    for (const i of open) {
      expect(i.id.startsWith(`${i.kind === "system" ? "thread" : i.kind}:`)).toBe(true);
      expect(i.open).toBe(true);
      expect(!!i.question).toBe(i.kind === "question" || i.kind === "notice");
      expect(!!i.message).toBe(i.kind === "mail");
      expect(!!i.system).toBe(i.kind === "system");
    }
    const sys = open.find((i) => i.kind === "system")!;
    expect(sys).toMatchObject({ id: `thread:${loop}`, project: "terrarium" });
    expect(sys.system).toMatchObject({ kind: "paused_thread", thread_id: loop, subject: "Re: Flaky runner test", paused: true, agent_run: 7, limit: 6 });
    expect(sys.system!.participants.sort()).toEqual(["terrarium/lead", "terrarium/runner-dev"]);
    expect(sys.system!.held.map((m) => m.id)).toEqual([fakeUlid("msg", 11)]);
    // Mail to the operator, but not the "needs a human" notice: the thread item stands for it.
    expect(open.filter((i) => i.kind === "mail").map((i) => i.message!.id).sort()).toEqual([3, 13, 16].map((n) => fakeUlid("msg", n)));
  });

  it("filters by kind, project, and status; system items show only while open", async () => {
    const api = mock();
    expect((await api.inbox.list({ kind: "notice" })).map((i) => i.question!.kind)).toEqual(["notice"]);
    expect((await api.inbox.list({ project: "webshop" })).map((i) => i.id)).toEqual(["question:01JBQUES000000000000000002"]);
    const done = await api.inbox.list({ status: "done" });
    expect(done.every((i) => !i.open && i.kind !== "system")).toBe(true);
    expect(done.map((i) => i.kind).sort()).toEqual(["mail", "notice", "question", "question"]);
    expect((await api.inbox.list({ status: "all" })).length).toBe(11);
    await expect(api.inbox.list({ status: "nope" as "open" })).rejects.toMatchObject({ status: 422 });
  });

  it("counts the open items by kind, for a project too", async () => {
    const api = mock();
    expect(await api.inbox.counts()).toEqual({ open: 7, urgent: 1, critical: 0, question: 2, notice: 1, mail: 3, system: 1 });
    expect(await api.inbox.counts("webshop")).toEqual({ open: 1, urgent: 0, critical: 0, question: 1, notice: 0, mail: 0, system: 0 });
  });

  it("acknowledges mail to the operator, with an optional reply on the thread", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const id = fakeUlid("msg", 3);
    const r = await api.inbox.ackMail(id, "Approved.");
    expect(r.message).toMatchObject({ id, state: "replied", read_by: "operator" });
    expect(r.replies).toHaveLength(1);
    expect(r.replies[0]).toMatchObject({ from: "operator", to_name: "terrarium/lead", thread_id: fakeUlid("th", 2), in_reply_to: id });
    await expect(api.inbox.ackMail(id)).rejects.toMatchObject({ status: 409 });
    expect((await api.inbox.list()).some((i) => i.id === `mail:${id}`)).toBe(false);
    expect((await api.inbox.list({ status: "done", kind: "mail" })).some((i) => i.id === `mail:${id}`)).toBe(true);
    await wait();
    expect(frames.find((f) => f.kind === "inbox" && f.data.item === `mail:${id}`)!.data).toMatchObject({ kind: "mail", change: "closed", project: "terrarium" });
    // No reply: only the acknowledge.
    const quota = fakeUlid("msg", 16);
    expect(await api.inbox.ackMail(quota)).toMatchObject({ message: { state: "read" }, replies: [] });
    close();
  });

  it("refuses a reply to a system mail that has nobody to reply to", async () => {
    const api = mock();
    await expect(api.inbox.ackMail(fakeUlid("msg", 16), "ok")).rejects.toMatchObject({ status: 422 });
    expect((await api.inbox.list()).some((i) => i.id === `mail:${fakeUlid("msg", 16)}`)).toBe(true);
    await expect(api.inbox.ackMail(fakeUlid("msg", 4))).rejects.toMatchObject({ status: 404 });
  });

  it("releases one held message: the thread stays paused, the item stays open with no held mail", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const id = fakeUlid("msg", 11);
    const m = await api.mail.release(id);
    expect(m).toMatchObject({ id, released_by: "operator", thread_paused: true });
    expect(m.released_at).toBeTruthy();
    await expect(api.mail.release(id)).rejects.toMatchObject({ status: 409 });
    await expect(api.mail.release(fakeUlid("msg", 3))).rejects.toMatchObject({ status: 409 });
    const sys = (await api.inbox.list({ kind: "system" }))[0]!;
    expect(sys.system).toMatchObject({ paused: true, held: [] });
    await wait();
    expect(frames.find((f) => f.kind === "mail" && f.data.message_id === id)!.data).toMatchObject({ released: true });
    expect(frames.find((f) => f.kind === "inbox")!.data).toMatchObject({ item: `thread:${loop}`, change: "updated" });
    close();
  });

  it("resumes a thread: the pause ends, the held mail goes out, and a note goes to the participants", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const r = await api.mail.resume(loop, "Stop the reruns. I look at the runner.");
    expect(r.thread_id).toBe(loop);
    expect(r.released).toEqual([fakeUlid("msg", 11)]);
    expect(r.note.map((m) => m.to_name).sort()).toEqual(["terrarium/lead", "terrarium/runner-dev"]);
    expect(r.note.every((m) => m.from === "operator" && m.thread_id === loop && m.subject === "Re: Flaky runner test")).toBe(true);
    expect((await api.inbox.list({ kind: "system" }))).toEqual([]);
    expect((await api.mail.list({ thread: loop })).items.some((m) => m.thread_paused)).toBe(false);
    await wait();
    expect(frames.filter((f) => f.kind === "inbox" && f.data.item === `thread:${loop}`).at(-1)!.data).toMatchObject({ change: "closed" });
    close();
  });

  it("resumes with no note; the agents can then write again until the limit", async () => {
    const api = mock();
    expect(await api.mail.resume(loop)).toMatchObject({ released: [fakeUlid("msg", 11)], note: [] });
    expect((await api.inbox.list({ kind: "system" }))).toEqual([]);
    const next = await api.mail.send({ from: "terrarium/runner-dev", to: "terrarium/lead", thread_id: loop, body: "Found it." });
    expect(next.state).toBe("queued");
    expect(next.thread_paused).toBeUndefined();
    await expect(api.mail.resume("01JBTH0000000000000000NONE")).rejects.toMatchObject({ status: 404 });
    // Resumed: nothing to resume any more, as the daemon answers.
    await expect(api.mail.resume(loop)).rejects.toMatchObject({ status: 409 });
  });

  it("opens a system item when the loop guard holds a message", async () => {
    const api = mock();
    const a = await api.mail.send({ from: "terrarium/lead", to: "terrarium/runner-dev", subject: "loop", body: "1" });
    for (let i = 2; i <= 7; i++) await api.mail.send({ from: i % 2 ? "terrarium/lead" : "terrarium/runner-dev", to: i % 2 ? "terrarium/runner-dev" : "terrarium/lead", thread_id: a.thread_id, body: String(i) });
    const item = (await api.inbox.list({ kind: "system" })).find((i) => i.id === `thread:${a.thread_id}`)!;
    expect(item.system).toMatchObject({ paused: true, agent_run: 7, subject: "loop" });
    expect(item.system!.held).toHaveLength(1);
  });

  it("keeps the inbox store live from inbox events", async () => {
    const api = mock();
    const store = createInboxStore(api, { on: (fn) => api.events.stream({ since: api.world.seq }).subscribe(fn) }, { debounceMs: 0 });
    await store.refresh();
    expect(store.loaded()).toBe(true);
    expect(store.counts()).toMatchObject({ open: 7, urgent: 1 });
    expect(store.open()).toHaveLength(7);
    await wait();
    await api.questions.answer(store.open()[0]!.question!.id, { answer: "yes" });
    await wait(30);
    expect(store.counts()).toMatchObject({ open: 6, urgent: 0, question: 1 });
    await api.inbox.ackMail(fakeUlid("msg", 13));
    await wait(30);
    expect(store.counts()).toMatchObject({ open: 5, mail: 2 });
    expect(store.open()).toHaveLength(5);
  });

  it("shows a critical notice first, counts it, and drops the banner after the acknowledgement", async () => {
    const api = mock();
    const store = createInboxStore(api, { on: (fn) => api.events.stream({ since: api.world.seq }).subscribe(fn) }, { debounceMs: 0 });
    await store.refresh();
    expect(criticalBanner(store.open())).toBeNull();
    // A lead sends a critical notice (the dashboard does not ask; the world gets it).
    const notice = api.world.questions!.find((q) => q.kind === "notice" && q.status === "open")!;
    notice.priority = "critical";
    await store.refresh();
    expect(store.counts()).toMatchObject({ critical: 1, urgent: 2 });
    expect(store.open()[0]!.id).toBe(`notice:${notice.id}`);
    const b = criticalBanner(store.open())!;
    expect(b).toMatchObject({ more: 0, from: notice.agent_name, href: `/inbox?item=notice%3A${notice.id}` });
    await wait();
    await api.questions.ack(notice.id);
    await wait(30);
    expect(store.counts()).toMatchObject({ critical: 0 });
    expect(criticalBanner(store.open())).toBeNull();
  });
});

// The operator's inbox in the mock: the items, the counts, the mail
// acknowledge, and the release and resume of held mail.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import { fakeUlid } from "./data";
import type { AgentEvent } from "../types";
import { createInboxStore } from "../../stores/inbox";
import { criticalBanner } from "../../features/inbox/critical";
import { waitFor } from "../../test/wait";

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
    // Without the Inbox thread that waits for the operator (./threads.ts): only `kind=thread` lists it (TER-466).
    // Every open notice is in it, a normal one too (TER-1324).
    expect(open.length).toBe(4);
    expect(open.every((i) => i.kind === "question" || i.kind === "notice")).toBe(true);
    expect(open.some((i) => i.kind === "notice" && !i.urgent)).toBe(true);
    expect(open.some((i) => i.kind === "thread")).toBe(false);
    expect((await api.inbox.list({ kind: "thread" })).map((i) => i.kind)).toEqual(["thread"]);
    expect(open[0]).toMatchObject({ kind: "question", urgent: true, open: true });
    expect(open.slice(1).map((i) => i.at)).toEqual(open.slice(1).map((i) => i.at).sort().reverse());
    for (const i of open) {
      // A bundle is a `question` item with the ID `bundle:<id>`.
      expect(i.id.startsWith(`${i.kind === "system" ? "thread" : i.kind === "thread" ? "inbox-thread" : i.question?.kind === "bundle" ? "bundle" : i.kind}:`)).toBe(true);
      expect(i.open).toBe(true);
      expect(!!i.question).toBe(i.kind === "question" || i.kind === "notice");
      expect(!!i.message).toBe(i.kind === "mail");
      expect(!!i.system).toBe(i.kind === "system");
    }
    const sys = (await api.inbox.list({ kind: "system" }))[0]!;
    expect(sys).toMatchObject({ id: `thread:${loop}`, project: "terrarium" });
    expect(sys.system).toMatchObject({ kind: "paused_thread", thread_id: loop, subject: "Re: Flaky runner test", paused: true, agent_run: 7, limit: 6 });
    expect(sys.system!.participants.sort()).toEqual(["terrarium/lead", "terrarium/runner-dev"]);
    expect(sys.system!.held.map((m) => m.id)).toEqual([fakeUlid("msg", 11)]);
    // Mail to the operator, but not the "needs a human" notice: the thread item stands for it.
    expect((await api.inbox.list({ kind: "mail" })).map((i) => i.message!.id).sort()).toEqual([3, 13, 16].map((n) => fakeUlid("msg", n)));
    const b = open.find((i) => i.question?.kind === "bundle")!;
    expect(b).toMatchObject({ id: `bundle:${b.question!.id}`, kind: "question", project: "terrarium" });
    expect(b.question!.items).toHaveLength(3);
  });

  it("filters by kind, project, and status; system items show only while open", async () => {
    const api = mock();
    expect((await api.inbox.list({ kind: "notice" })).map((i) => i.question!.kind)).toEqual(["notice"]);
    expect((await api.inbox.list({ project: "webshop" })).map((i) => i.id)).toEqual(["question:01JBQUES000000000000000002"]);
    const done = await api.inbox.list({ status: "done" });
    expect(done.every((i) => !i.open && i.kind !== "system")).toBe(true);
    // No Inbox thread: only `kind=thread` lists them (TER-466); there, one waits for its agent and one is closed.
    expect(done.map((i) => i.kind).sort()).toEqual(["mail", "notice", "question", "question"]);
    expect((await api.inbox.list({ status: "done", kind: "thread" })).map((i) => i.kind)).toEqual(["thread", "thread"]);
    expect((await api.inbox.list({ status: "all" })).length).toBe(12);
    // The Questions tab holds the bundle too.
    expect((await api.inbox.list({ kind: "question" })).map((i) => i.question!.kind).sort()).toEqual(["bundle", "question", "question"]);
    await expect(api.inbox.list({ status: "nope" as "open" })).rejects.toMatchObject({ status: 422 });
  });

  it("counts the open items by kind, for a project too", async () => {
    const api = mock();
    // The bundle counts once, as a question.
    // The questions of a thread count in its thread item, not as questions; the threads count for the Threads badge, not in `open` (TER-466).
    expect(await api.inbox.counts()).toEqual({ open: 4, urgent: 1, critical: 0, question: 3, notice: 1, mail: 3, system: 1, watch: 0, thread: 1, thread_urgent: 0, thread_critical: 0 });
    expect(await api.inbox.counts("webshop")).toEqual({ open: 1, urgent: 0, critical: 0, question: 1, notice: 0, mail: 0, system: 0, watch: 0, thread: 0, thread_urgent: 0, thread_critical: 0 });
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
    expect((await api.inbox.list({ kind: "mail" })).some((i) => i.id === `mail:${fakeUlid("msg", 16)}`)).toBe(true);
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
    expect(store.counts()).toMatchObject({ open: 3, urgent: 1, thread: 1 });
    expect(store.open()).toHaveLength(3);
    await wait();
    await api.questions.answer(store.open()[0]!.question!.id, { answer: "yes" });
    await waitFor(() => expect(store.counts()).toMatchObject({ open: 2, urgent: 0, question: 2 }));
    await api.inbox.ackMail(fakeUlid("msg", 13));
    await waitFor(() => expect(store.counts()).toMatchObject({ open: 2, mail: 2 }));
    expect(store.open()).toHaveLength(2);
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
    await waitFor(() => expect(store.counts()).toMatchObject({ critical: 0 }));
    expect(criticalBanner(store.open())).toBeNull();
  });

  it("shows a critical question inside a thread in the queue and the banner, and drops it after the answer (TER-488)", async () => {
    const api = mock();
    const calls: unknown[] = [];
    const spy = { ...api, inbox: { ...api.inbox, list: (q?: Parameters<typeof api.inbox.list>[0]) => (calls.push(q), api.inbox.list(q)) } };
    const store = createInboxStore(spy, { on: (fn) => api.events.stream({ since: api.world.seq }).subscribe(fn) }, { debounceMs: 0 });
    await store.refresh();
    expect(criticalBanner(store.open())).toBeNull();
    // No critical thread: no read of the thread items.
    expect(calls).toEqual([{ status: "open" }]);
    // The agent of a thread asks a critical question.
    const q = api.world.questions!.find((x) => x.thread && x.status === "open")!;
    q.priority = "critical";
    await store.refresh();
    expect(store.counts()).toMatchObject({ critical: 0, thread_critical: 1 });
    expect(calls.at(-1)).toEqual({ status: "open", kind: "thread" });
    const item = store.open()[0]!;
    expect(item).toMatchObject({ kind: "thread", critical: true });
    expect(store.open().filter((i) => i.kind === "thread")).toHaveLength(1);
    const b = criticalBanner(store.open())!;
    expect(b).toMatchObject({ more: 0, text: item.thread!.topic, href: `/threads/${encodeURIComponent(item.thread!.id)}` });
    await wait();
    await api.questions.answer(q.id, { answer: "yes" });
    await waitFor(() => expect(store.counts()).toMatchObject({ thread_critical: 0 }));
    expect(criticalBanner(store.open())).toBeNull();
    expect(store.open().some((i) => i.kind === "thread")).toBe(false);
  });

  it("asks an agent: one mail on a new thread, an open watch that the badge does not count, and the answer closes it", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const before = await api.inbox.counts();
    const r = await api.inbox.ask({ to: "terrarium/lead", text: "Why did the runner fail?\nMore lines." });
    expect(r.message).toMatchObject({ from: "operator", to_name: "terrarium/lead", subject: "Question from the operator: Why did the runner fail?" });
    expect(r.message.body).toContain("Answer the operator with `terrarium mail reply <this message>`");
    expect(r.watch).toMatchObject({ thread_id: r.message.thread_id, agent_name: "terrarium/lead", message_id: r.message.id, waiting: true, replies: 0 });
    expect(r.watch.question_id).toBeUndefined();
    const c = await api.inbox.counts();
    expect(c).toMatchObject({ open: before.open, watch: 1 });
    const watches = await api.inbox.list({ kind: "watch" });
    expect(watches.map((i) => i.id)).toEqual([`watch:${r.message.thread_id}`]);
    expect(watches[0]).toMatchObject({ open: true, project: "terrarium", urgent: false });
    // The agent answers: a mail item with the watch, and the watch closes.
    const answer = await api.mail.send({ from: "terrarium/lead", to: "operator", thread_id: r.message.thread_id, body: "A flaky test." });
    expect(await api.inbox.list({ kind: "watch" })).toEqual([]);
    const done = (await api.inbox.list({ kind: "watch", status: "done" }))[0]!;
    expect(done.watch).toMatchObject({ waiting: false, replies: 1 });
    const mailItem = (await api.inbox.list({ kind: "mail" })).find((i) => i.id === `mail:${answer.id}`)!;
    expect(mailItem.watch).toMatchObject({ thread_id: r.message.thread_id, waiting: false });
    expect((await api.inbox.counts()).watch).toBe(0);
    // The operator's reply opens it again.
    await api.inbox.ackMail(answer.id, "And the fix?");
    expect((await api.inbox.list({ kind: "watch" }))[0]!.watch).toMatchObject({ waiting: true, replies: 1 });
    await wait();
    const changes = frames.filter((f) => f.kind === "inbox" && f.data.item === `watch:${r.message.thread_id}`).map((f) => f.data.change);
    expect(changes).toEqual(["opened", "closed", "opened"]);
    expect(api.world.audit.some((x) => x.action === "inbox.ask" && x.target === r.message.thread_id)).toBe(true);
    close();
  });

  it("forwards a question: the mail quotes it, the question stays open, and a cancel ends the watch", async () => {
    const api = mock();
    const q = api.world.questions!.find((x) => x.kind === "question" && x.status === "open" && x.options.length)!;
    const r = await api.inbox.ask({ to: "terrarium/runner-dev", question: q.id, text: "Can you take this?" });
    expect(r.message.subject).toBe(`Forwarded question from ${q.agent_name}: ${q.text.split("\n")[0]!.slice(0, 80)}`);
    expect(r.message.body.startsWith("Can you take this?\n\nThe operator forwards this question of ")).toBe(true);
    expect(r.message.body).toContain(`> 1. ${q.options[0]}`);
    expect(r.watch.question_id).toBe(q.id);
    expect((await api.questions.get(q.id)).status).toBe("open");
    const w = await api.inbox.cancelWatch(r.message.thread_id);
    expect(w.cancelled_at).toBeTruthy();
    expect(await api.inbox.list({ kind: "watch" })).toEqual([]);
    await expect(api.inbox.cancelWatch(r.message.thread_id)).rejects.toMatchObject({ status: 409 });
    await expect(api.inbox.cancelWatch("01JBTH0000000000000000NONE")).rejects.toMatchObject({ status: 404 });
  });

  it("refuses a bad ask as the daemon", async () => {
    const api = mock();
    await expect(api.inbox.ask({ to: "terrarium/lead", text: "  " })).rejects.toMatchObject({ status: 422 });
    await expect(api.inbox.ask({ to: "terrarium/nobody", text: "hi" })).rejects.toMatchObject({ status: 404 });
    await expect(api.inbox.ask({ to: "terrarium/lead", question: "01JBQUES0000000000000NONE0" })).rejects.toMatchObject({ status: 404 });
    await expect(api.inbox.ask({ to: "terrarium/lead", text: "x".repeat(4001) })).rejects.toMatchObject({ status: 422 });
    await expect(api.inbox.list({ kind: "bogus" as never })).rejects.toMatchObject({ status: 422 });
  });
});

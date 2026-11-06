// The question routes of the mock: the daemon's shapes, the mail on the
// thread, the `question` waits, and the events (docs/api.md §6.8).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent, Wait } from "../types";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));
const listen = async (api: ReturnType<typeof mock>) => {
  const frames: AgentEvent[] = [];
  const s = api.events.stream({ since: api.world.seq });
  s.subscribe((e) => frames.push(e));
  await wait();
  return { frames, close: () => s.close() };
};

describe("questions in the mock", () => {
  it("lists the open questions, urgent first, with the daemon's fields", async () => {
    const api = mock();
    const open = await api.questions.list({ kind: "question" });
    // Two of the leaders, two of the seeded Inbox thread (./threads.ts).
    expect(open.length).toBe(4);
    expect(open[0]!.priority).toBe("urgent");
    expect(open.every((q) => q.status === "open" && q.kind === "question")).toBe(true);
    // Without `kind`: every kind.
    expect((await api.questions.list()).map((q) => q.kind)).toEqual(["question", "question", "notice", "question", "question", "bundle"]);
    for (const k of ["id", "kind", "agent", "agent_name", "project", "text", "options", "priority", "status", "thread_id", "created_at", "updated_at", "revision"]) {
      expect(open[0]).toHaveProperty(k);
    }
    expect((await api.questions.list({ status: "all", kind: "question" })).length).toBe(6);
    expect((await api.questions.list({ status: "all" })).length).toBe(9);
    expect((await api.questions.list({ project: "webshop" })).map((q) => q.agent_name)).toEqual(["webshop/shop-lead"]);
    // The agent list: open, and answered or acknowledged in the last 24 h; `all` adds the retracted one.
    const mine = await api.questions.ofAgent("terrarium/lead");
    expect(mine.map((q) => `${q.kind} ${q.status}`)).toEqual(["question open", "notice open", "bundle open", "question answered"]);
    expect((await api.questions.ofAgent("terrarium/lead", { all: true })).length).toBe(5);
    expect((await api.questions.ofAgent("webshop/shop-lead")).map((q) => q.status)).toEqual(["open", "acknowledged"]);
  });

  it("answers with an option: one mail from the operator on the thread, an event, and a 409 after", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const q = (await api.questions.list({ kind: "question" }))[0]!;
    const done = await api.questions.answer(q.id, { option: 1, answer: "Wait, it is a small fix." });
    expect(done).toMatchObject({ status: "answered", answer_option: 1, answer: "Wait, it is a small fix.", answered_by: "operator" });
    const mail = api.world.mail.filter((m) => m.thread_id === q.thread_id);
    expect(mail).toHaveLength(1);
    expect(mail[0]).toMatchObject({ from: "operator", to: q.agent, subject: expect.stringMatching(/^Answer: /) });
    expect(mail[0]!.body).toContain("Chosen option: 2. Wait for TER-12");
    await wait();
    const e = frames.find((f) => f.kind === "question")!;
    expect(e.data).toMatchObject({ question_id: q.id, change: "answered", kind: "question", status: "answered" });
    expect(frames.find((f) => f.kind === "inbox")!.data).toMatchObject({ item: `question:${q.id}`, kind: "question", change: "closed", project: "terrarium" });
    await expect(api.questions.answer(q.id, { answer: "again" })).rejects.toMatchObject({ status: 409, code: "conflict" });
    await expect(api.questions.answer((await api.questions.list())[0]!.id, { option: "nope" })).rejects.toMatchObject({ status: 422 });
    await expect(api.questions.answer((await api.questions.list())[0]!.id, {})).rejects.toMatchObject({ status: 422 });
    close();
  });

  it("resolves a question wait with the answer and no second mail", async () => {
    const api = mock();
    const q = (await api.questions.list())[0]!;
    const lead = await api.agents.get(q.agent);
    const w: Wait = {
      id: "01JBWAITQ00000000000000001", agent: lead.id, agent_name: q.agent_name, project: q.project, kind: "question",
      spec: { question: q.id }, summary: "question Ship v0.2…", state: "open", created_by: `agent:${lead.id}`, created_at: new Date().toISOString(),
    };
    api.world.waits.push(w);
    const systemMail = () => api.world.mail.filter((m) => m.to === lead.id && m.from === "system").length;
    const before = systemMail();
    await api.questions.answer(q.id, { option: "Ship on Friday" });
    expect(w.state).toBe("resolved");
    expect(w.result).toMatchObject({ outcome: "answered", answer: "Ship on Friday", question: q.id });
    expect(systemMail()).toBe(before);
  });

  it("retracts: one mail with the reason, then a 409", async () => {
    const api = mock();
    const q = (await api.questions.list({ project: "webshop" }))[0]!;
    const r = await api.questions.retract(q.id, "Stripe, as agreed");
    expect(r).toMatchObject({ status: "retracted", retract_reason: "Stripe, as agreed", retracted_by: "operator" });
    const mail = api.world.mail.filter((m) => m.thread_id === q.thread_id);
    expect(mail).toHaveLength(1);
    expect(mail[0]!.body).toContain("> Stripe, as agreed");
    await expect(api.questions.retract(q.id)).rejects.toMatchObject({ status: 409 });
  });

  it("acknowledges a notice: the reply, one mail on the thread, events, and a 409 after", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const n = (await api.questions.list({ kind: "notice" }))[0]!;
    expect(n).toMatchObject({ kind: "notice", status: "open", options: [] });
    await expect(api.questions.answer(n.id, { answer: "x" })).rejects.toMatchObject({ status: 409 });
    const done = await api.questions.ack(n.id, "  Good, go on.  ");
    expect(done).toMatchObject({ status: "acknowledged", answer: "Good, go on.", answered_by: "operator", answered_by_name: "operator" });
    expect(done.answered_at).toBeTruthy();
    const mail = api.world.mail.filter((m) => m.thread_id === n.thread_id);
    expect(mail).toHaveLength(1);
    expect(mail[0]).toMatchObject({ from: "operator", to: n.agent, subject: expect.stringMatching(/^Seen: /) });
    expect(mail[0]!.body).toContain("> Good, go on.");
    await wait();
    expect(frames.find((f) => f.kind === "question")!.data).toMatchObject({ question_id: n.id, change: "acknowledged", kind: "notice" });
    expect(frames.find((f) => f.kind === "inbox" && f.data.item === `notice:${n.id}`)!.data).toMatchObject({ change: "closed", kind: "notice" });
    await expect(api.questions.ack(n.id)).rejects.toMatchObject({ status: 409 });
    // A question takes no acknowledge.
    const q = (await api.questions.list({ kind: "question" }))[0]!;
    await expect(api.questions.ack(q.id)).rejects.toMatchObject({ status: 409 });
    close();
  });

  it("acknowledges a notice with no reply, and retracts a notice", async () => {
    const api = mock();
    const n = (await api.questions.list({ kind: "notice" }))[0]!;
    expect(await api.questions.ack(n.id)).toMatchObject({ status: "acknowledged", answer: null });
    const api2 = mock();
    const r = await api2.questions.retract(n.id, "not needed");
    expect(r).toMatchObject({ kind: "notice", status: "retracted", retracted_by: "operator" });
  });

  it("answers a bundle in one step: every item, the revision, one mail, one event, and the wait result", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const b = (await api.questions.list({ kind: "bundle" }))[0]!;
    expect(b).toMatchObject({ kind: "bundle", status: "open", options: [] });
    expect(b.items!.map((i) => i.options.length)).toEqual([2, 2, 0]);
    const lead = await api.agents.get(b.agent);
    const w: Wait = {
      id: "01JBWAITQ00000000000000002", agent: lead.id, agent_name: b.agent_name, project: b.project, kind: "question",
      spec: { question: b.id }, summary: "bundle Release plan…", state: "open", created_by: `agent:${lead.id}`, created_at: new Date().toISOString(),
    };
    api.world.waits.push(w);
    // A partial answer: 422 with the missing items, and nothing changes.
    await expect(api.questions.answer(b.id, { items: [{ option: 0 }] })).rejects.toMatchObject({ status: 422, details: { missing: [2, 3] } });
    await expect(api.questions.answer(b.id, { answer: "all of it" })).rejects.toMatchObject({ status: 422 });
    await expect(api.questions.answer(b.id, { items: [{ option: "maybe" }, { answer: "x" }, { answer: "x" }] })).rejects.toMatchObject({ status: 422 });
    // The agent changed it: an answer for an old revision is a 409.
    await expect(api.questions.answer(b.id, { revision: b.revision + 1, items: [{ option: 0 }, { option: 1 }, { answer: "x" }] })).rejects.toMatchObject({ status: 409, details: { revision: b.revision } });
    expect((await api.questions.get(b.id)).status).toBe("open");
    expect(api.world.mail.filter((m) => m.thread_id === b.thread_id)).toHaveLength(0);
    // Also a single question takes no items, and the bundle takes no acknowledge.
    const single = (await api.questions.list({ kind: "question" }))[0]!;
    await expect(api.questions.answer(single.id, { items: [{ answer: "x" }] })).rejects.toMatchObject({ status: 422 });
    await expect(api.questions.ack(b.id)).rejects.toMatchObject({ status: 409 });

    const done = await api.questions.answer(b.id, { revision: b.revision, items: [{ option: 1 }, { option: "staging-us", answer: "the newer kernel" }, { answer: "Mention the PTY fix." }] });
    expect(done).toMatchObject({ status: "answered", answered_by: "operator" });
    expect(done.items!.map((i) => [i.answer, i.answer_option ?? null])).toEqual([
      ["Next week", 1],
      ["the newer kernel", 1],
      ["Mention the PTY fix.", null],
    ]);
    const mail = api.world.mail.filter((m) => m.thread_id === b.thread_id);
    expect(mail).toHaveLength(1);
    expect(mail[0]).toMatchObject({ from: "operator", to: b.agent, subject: expect.stringMatching(/^Answers: /) });
    expect(mail[0]!.body).toContain("all 3 items are in this mail");
    expect(mail[0]!.body).toContain("Item 2 of 3:\n> Which staging host gets the release first?\nChosen option: 2. staging-us\nThe answer of operator:\n> the newer kernel\n");
    expect(w.state).toBe("resolved");
    expect(w.result).toMatchObject({ outcome: "answered", kind: "bundle", question: b.id, answers: [{ item: 1, option: "Next week" }, { item: 2 }, { item: 3, answer: "Mention the PTY fix." }] });
    await wait();
    expect(frames.filter((f) => f.kind === "question").map((f) => f.data.change)).toEqual(["answered"]);
    expect(frames.find((f) => f.kind === "inbox")!.data).toMatchObject({ item: `bundle:${b.id}`, kind: "question", change: "closed" });
    await expect(api.questions.answer(b.id, { items: [] })).rejects.toMatchObject({ status: 409 });
    close();
  });

  it("retracts a whole bundle with one mail", async () => {
    const api = mock();
    const b = (await api.questions.list({ kind: "bundle" }))[0]!;
    expect(await api.questions.retract(b.id)).toMatchObject({ kind: "bundle", status: "retracted" });
    const mail = api.world.mail.filter((m) => m.thread_id === b.thread_id);
    expect(mail).toHaveLength(1);
    expect(mail[0]!.body).toContain(`retracted your bundle ${b.id}: you need no answers`);
  });

  it("links a question to a card and unlinks it, at any status, with the revision kept", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const onCard = await api.questions.list({ card: "TER-12", status: "all" });
    expect(onCard.map((q) => q.kind)).toEqual(["question", "bundle"]);
    expect((await api.questions.list({ card: "ter-12" })).length).toBe(2);
    await expect(api.questions.list({ card: "NOPE-1" })).rejects.toMatchObject({ status: 404 });
    const q = onCard[0]!;
    const moved = await api.questions.setCard(q.id, "TER-16");
    expect(moved).toMatchObject({ card: "TER-16", revision: q.revision });
    expect((await api.questions.list({ card: "TER-12" })).map((x) => x.kind)).toEqual(["bundle"]);
    await expect(api.questions.setCard(q.id, "WEB-4")).rejects.toMatchObject({ status: 422 });
    await expect(api.questions.setCard(q.id, "NOPE-1")).rejects.toMatchObject({ status: 422 });
    // A closed one changes too.
    const done = (await api.questions.list({ status: "answered" }))[0]!;
    expect((await api.questions.setCard(done.id, "TER-14")).card).toBe("TER-14");
    expect((await api.questions.setCard(done.id, null)).card).toBeNull();
    await wait();
    const ev = frames.filter((f) => f.kind === "question");
    expect(ev.map((f) => [f.data.change, f.data.card_id ?? null, f.data.previous_card_id ?? null])).toEqual([
      ["updated", "TER-16", "TER-12"],
      ["updated", "TER-14", null],
      ["updated", null, "TER-14"],
    ]);
    expect(frames.filter((f) => f.kind === "inbox").every((f) => f.data.change === "updated")).toBe(true);
    expect(api.world.audit.filter((a) => a.action === "question.update").length).toBeGreaterThanOrEqual(3);
    close();
  });
});

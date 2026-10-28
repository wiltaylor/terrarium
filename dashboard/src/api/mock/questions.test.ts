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
    expect(open.length).toBe(2);
    expect(open[0]!.priority).toBe("urgent");
    expect(open.every((q) => q.status === "open" && q.kind === "question")).toBe(true);
    // Without `kind`: both kinds.
    expect((await api.questions.list()).map((q) => q.kind)).toEqual(["question", "question", "notice"]);
    for (const k of ["id", "kind", "agent", "agent_name", "project", "text", "options", "priority", "status", "thread_id", "created_at", "updated_at", "revision"]) {
      expect(open[0]).toHaveProperty(k);
    }
    expect((await api.questions.list({ status: "all", kind: "question" })).length).toBe(4);
    expect((await api.questions.list({ status: "all" })).length).toBe(6);
    expect((await api.questions.list({ project: "webshop" })).map((q) => q.agent_name)).toEqual(["webshop/shop-lead"]);
    // The agent list: open, and answered or acknowledged in the last 24 h; `all` adds the retracted one.
    const mine = await api.questions.ofAgent("terrarium/lead");
    expect(mine.map((q) => `${q.kind} ${q.status}`)).toEqual(["question open", "notice open", "question answered"]);
    expect((await api.questions.ofAgent("terrarium/lead", { all: true })).length).toBe(4);
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
});

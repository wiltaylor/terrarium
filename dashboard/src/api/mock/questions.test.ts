// The question routes of the mock: the daemon's shapes, the mail on the
// thread, the `question` waits, and the events (docs/api.md §6.8).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent, Wait } from "../types";
import { createQuestionsStore } from "../../stores/questions";

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
    const open = await api.questions.list();
    expect(open.length).toBe(2);
    expect(open[0]!.priority).toBe("urgent");
    expect(open.every((q) => q.status === "open")).toBe(true);
    for (const k of ["id", "agent", "agent_name", "project", "text", "options", "priority", "status", "thread_id", "created_at", "updated_at", "revision"]) {
      expect(open[0]).toHaveProperty(k);
    }
    expect((await api.questions.list({ status: "all" })).length).toBe(4);
    expect((await api.questions.list({ project: "webshop" })).map((q) => q.agent_name)).toEqual(["webshop/shop-lead"]);
    // The agent list: open and answered in the last 24 h; `all` adds the retracted one.
    const mine = await api.questions.ofAgent("terrarium/lead");
    expect(mine.map((q) => q.status)).toEqual(["open", "answered"]);
    expect((await api.questions.ofAgent("terrarium/lead", { all: true })).length).toBe(3);
  });

  it("answers with an option: one mail from the operator on the thread, an event, and a 409 after", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const q = (await api.questions.list())[0]!;
    const done = await api.questions.answer(q.id, { option: 1, answer: "Wait, it is a small fix." });
    expect(done).toMatchObject({ status: "answered", answer_option: 1, answer: "Wait, it is a small fix.", answered_by: "operator" });
    const mail = api.world.mail.filter((m) => m.thread_id === q.thread_id);
    expect(mail).toHaveLength(1);
    expect(mail[0]).toMatchObject({ from: "operator", to: q.agent, subject: expect.stringMatching(/^Answer: /) });
    expect(mail[0]!.body).toContain("Chosen option: 2. Wait for TER-12");
    await wait();
    const e = frames.find((f) => f.kind === "question")!;
    expect(e.data).toMatchObject({ question_id: q.id, change: "answered", status: "answered" });
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

  it("keeps the badge store live from question events", async () => {
    const api = mock();
    const store = createQuestionsStore(api, { on: (fn) => api.events.stream({ since: api.world.seq }).subscribe(fn) }, { debounceMs: 0 });
    await store.refresh();
    expect(store.counts()).toEqual({ open: 2, urgent: 1 });
    await wait();
    await api.questions.answer(store.open()[0]!.id, { answer: "yes" });
    await wait(30);
    expect(store.counts()).toEqual({ open: 1, urgent: 0 });
  });
});

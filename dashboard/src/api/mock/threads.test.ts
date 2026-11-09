// Inbox threads in the mock (docs/api.md §6.13): the daemon's shapes and
// status rules, the Inbox items of kind `thread`, and the events.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent } from "../types";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));
const listen = async (api: ReturnType<typeof mock>) => {
  const frames: AgentEvent[] = [];
  const s = api.events.stream({ since: api.world.seq });
  s.subscribe((e) => frames.push(e));
  await wait();
  return { frames, close: () => s.close() };
};

describe("Inbox threads in the mock", () => {
  it("lists the open threads, the ones that wait for the operator first, without messages", async () => {
    const api = mock();
    const open = await api.threads.list();
    expect(open.map((t) => t.status)).toEqual(["waiting_operator", "waiting_agent"]);
    expect(open[0]).toMatchObject({ scope: "terrarium", report_to_name: "terrarium/lead", message_count: 2, open_questions: 2 });
    expect(open[0]).not.toHaveProperty("messages");
    expect(open[0]).not.toHaveProperty("questions");
    expect((await api.threads.list({ status: "closed" })).map((t) => t.topic)).toEqual(["Rename the runner crate"]);
    expect(await api.threads.list({ status: "all", scope: "webshop" })).toHaveLength(1);
    await expect(api.threads.list({ status: "nope" as "open" })).rejects.toMatchObject({ status: 422 });
    const full = await api.threads.get(open[0]!.id);
    expect(full.messages).toHaveLength(2);
    expect(full.questions!.map((q) => q.thread)).toEqual([full.id, full.id]);
    await expect(api.threads.get("01JBNOPE")).rejects.toMatchObject({ status: 404 });
  });

  it("opens a thread: the first message makes it wait for the agent; the lead of the scope is report_to", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const t = await api.threads.open({ topic: " Shard? ", scope: "terrarium", template: "lead-consult", agent: "terrarium/lead", text: "How?" });
    expect(t).toMatchObject({ topic: "Shard?", scope: "terrarium", template: "lead-consult", opened_by: "operator", agent_name: "terrarium/lead", report_to_name: "terrarium/lead", status: "waiting_agent", response: null, handoff: null });
    expect(t.messages!.map((m) => [m.author, m.kind, m.text])).toEqual([["operator", "message", "How?"]]);
    expect((await api.threads.open({ topic: "No message", scope: "webshop" })).status).toBe("waiting_operator");
    // A global thread reports to the lead of project `meta`; the mock has none.
    await expect(api.threads.open({ topic: "Global" })).rejects.toMatchObject({ status: 409 });
    await expect(api.threads.open({ topic: "x", scope: "nope" })).rejects.toMatchObject({ status: 404 });
    await expect(api.threads.open({ topic: "x", scope: "webshop", agent: "terrarium/lead" })).rejects.toMatchObject({ status: 422 });
    await expect(api.threads.open({ topic: "a\nb", scope: "webshop" })).rejects.toMatchObject({ status: 422 });
    await wait();
    expect(frames.find((f) => f.kind === "thread")!.data).toMatchObject({ change: "opened", id: t.id, scope: "terrarium", status: "waiting_agent" });
    expect(frames.find((f) => f.kind === "inbox" && f.data.item === `inbox-thread:${t.id}`)!.data).toMatchObject({ kind: "thread", change: "opened", project: "terrarium" });
    close();
  });

  it("moves the status: agent post and question wait on the operator; operator post, response, and the last answer wait on the agent", async () => {
    const api = mock();
    const t = await api.threads.open({ topic: "Rules", scope: "terrarium", agent: "terrarium/lead", text: "Start." });
    expect((await api.threads.post(t.id, "More.")).status).toBe("waiting_agent");
    expect(api.sim.threadPost(t.id, "An answer.").status).toBe("waiting_operator");
    const r = await api.threads.respond(t.id, "Approved: do it.");
    expect(r.status).toBe("waiting_agent");
    expect(r.response).toMatchObject({ text: "Approved: do it.", by: "operator" });
    expect(r.messages!.at(-1)).toMatchObject({ kind: "response", text: "Approved: do it." });
    const q = api.sim.threadAsk(t.id, { text: "A or B?", options: ["A", "B"] });
    expect(q.thread).toBe(t.id);
    expect((await api.threads.get(t.id)).status).toBe("waiting_operator");
    // The Inbox item is open while a question is open, and holds the question; the question is no item of its own.
    const items = await api.inbox.list({ kind: "thread" });
    expect(items.find((i) => i.id === `inbox-thread:${t.id}`)).toMatchObject({ kind: "thread", open: true, thread: { open_questions: 1 } });
    expect((await api.inbox.list()).some((i) => i.question?.id === q.id || i.kind === "thread")).toBe(false);
    await api.questions.answer(q.id, { option: 0 });
    expect((await api.threads.get(t.id)).status).toBe("waiting_agent");
    expect((await api.inbox.list({ kind: "thread" })).some((i) => i.id === `inbox-thread:${t.id}`)).toBe(false);
    await expect(api.threads.post(t.id, " ")).rejects.toMatchObject({ status: 422 });
  });

  it("closes with a handoff, retracts the open questions, and refuses every write after", async () => {
    const api = mock();
    const [waiting] = await api.threads.list();
    const t = await api.threads.close(waiting!.id, { summary: "Pruning it is.", links: [{ kind: "card", ref: "TER-99" }] });
    expect(t).toMatchObject({ status: "closed", closed_by: "operator", handoff: { summary: "Pruning it is.", links: [{ kind: "card", ref: "TER-99" }], by: "operator" } });
    expect(t.questions!.map((q) => [q.status, q.retract_reason])).toEqual([
      ["retracted", `thread ${t.id} was closed`],
      ["retracted", `thread ${t.id} was closed`],
    ]);
    await expect(api.threads.post(t.id, "x")).rejects.toMatchObject({ status: 409 });
    await expect(api.threads.respond(t.id, "x")).rejects.toMatchObject({ status: 409 });
    await expect(api.threads.close(t.id)).rejects.toMatchObject({ status: 409 });
    expect((await api.inbox.list({ status: "done", kind: "thread" })).find((i) => i.id === `inbox-thread:${t.id}`)).toMatchObject({ open: false });
    const other = (await api.threads.list())[0]!;
    await expect(api.threads.close(other.id, { links: [{ kind: "nope" as "card", ref: "x" }] })).rejects.toMatchObject({ status: 422 });
    expect((await api.threads.close(other.id)).handoff).toMatchObject({ summary: "", links: [] });
  });

  it("keeps the agent's close proposal until the operator confirms it or writes on (TER-95)", async () => {
    const api = mock();
    const { frames, close } = await listen(api);
    const t = await api.threads.open({ topic: "Propose", scope: "terrarium", agent: "terrarium/lead", text: "Go." });
    expect(t.close_proposal).toBeNull();
    // Confirm with no proposal is a 409; an agent proposal waits on the operator, and a list row has it.
    await expect(api.threads.close(t.id, { confirm: true })).rejects.toMatchObject({ status: 409 });
    const lead = api.world.agents.find((a) => a.project === "terrarium" && a.name === "lead")!;
    const p = api.sim.threadPropose(t.id, { summary: "TER-99 has it.", links: [{ kind: "card", ref: "TER-99" }] });
    expect(p).toMatchObject({ status: "waiting_operator", close_proposal: { summary: "TER-99 has it.", links: [{ kind: "card", ref: "TER-99" }], by: lead.id } });
    expect((await api.threads.list()).find((x) => x.id === t.id)!.close_proposal).toMatchObject({ summary: "TER-99 has it." });
    await wait();
    expect(frames.find((f) => f.kind === "thread" && f.data.change === "close_proposed")!.data).toMatchObject({ id: t.id, status: "waiting_operator" });
    // A message, a response, or the answer to the last open question declines it.
    expect((await api.threads.post(t.id, "One more thing.")).close_proposal).toBeNull();
    api.sim.threadPropose(t.id, { summary: "Again." });
    expect((await api.threads.respond(t.id, "Approved.")).close_proposal).toBeNull();
    const q = api.sim.threadAsk(t.id, { text: "Close?", options: ["Yes", "No"] });
    api.sim.threadPropose(t.id, { summary: "Asked." });
    await api.questions.answer(q.id, { option: 1 });
    expect((await api.threads.get(t.id)).close_proposal).toBeNull();
    // Confirm closes with the agent's handoff; `confirm` with a summary is a 422.
    api.sim.threadPropose(t.id, { summary: "Final.", links: [{ kind: "wiki", ref: "plan" }] });
    await expect(api.threads.close(t.id, { confirm: true, summary: "x" })).rejects.toMatchObject({ status: 422 });
    const done = await api.threads.close(t.id, { confirm: true });
    expect(done).toMatchObject({ status: "closed", closed_by: "operator", close_reason: "confirmed", close_proposal: null, handoff: { summary: "Final.", links: [{ kind: "wiki", ref: "plan" }], by: lead.id } });
    expect(() => api.sim.threadPropose(t.id)).toThrow();
    const other = await api.threads.open({ topic: "Own close", scope: "webshop" });
    expect((await api.threads.close(other.id)).close_reason).toBe("operator");
    close();
  });
});

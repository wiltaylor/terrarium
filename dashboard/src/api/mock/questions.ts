// Questions of the leaders to the operator in the mock (docs/api.md §6.8):
// the seed, and the endpoints. The answers have the daemon's shapes.
//
// An answer sends one mail from the operator to the agent on the thread of
// the question, and resolves the `question` waits on it with no wake mail of
// their own (store/questions.rs). A retract by the operator sends one mail.

import type { Api } from "../client";
import type { Agent, AgentEvent, Message, Question, QuestionEventData } from "../types";
import type { MockWorld } from "./data";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface QuestionMockContext {
  w: MockWorld;
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> }) => AgentEvent;
  agent: (key: string) => Agent;
  audit: (action: string, target: string, project?: string, detail?: Record<string, unknown>) => void;
  sendMail: (to: string, from: string, subject: string, body: string, thread?: string) => Message;
  /** The `question` waits on the question resolve (./waits.ts). */
  questionClosed: (questionId: string, result: Record<string, unknown>) => void;
}

/** The longest answer, as in the daemon. */
const MAX_ANSWER = 8000;

/** The first line, at most `max` characters. */
export const shortText = (text: string, max = 60): string => {
  const first = text.split("\n").find((l) => l.trim()) ?? "";
  const t = first.trim();
  return t.length > max || text.trim().split("\n").length > 1 ? `${t.slice(0, max)}…` : t;
};

const quoted = (text: string) =>
  text
    .split("\n")
    .map((l) => `> ${l}\n`)
    .join("");

/** The seed: two open questions (one urgent, with options and a hostile context), one answered, one retracted. */
export function seedQuestions(now: number, agents: Agent[]): Question[] {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const named = (n: string) => agents.find((a) => a.name === n);
  const lead = named("lead");
  const shop = named("shop-lead");
  if (!lead || !shop) return [];
  const id = (n: number) => `01JBQUES0000000000000000${String(n).padStart(2, "0")}`;
  const base = (a: Agent, n: number, min: number) => ({
    id: id(n),
    agent: a.id,
    agent_name: `${a.project}/${a.name}`,
    project: a.project,
    thread_id: `01JBQTHR0000000000000000${String(n).padStart(2, "0")}`,
    created_at: ago(min),
    updated_at: ago(min),
    revision: 1,
  });
  return [
    {
      ...base(lead, 1, 25),
      text: "Ship v0.2 to the staging host on Friday, or wait for the PTY fix (TER-12)?",
      options: ["Ship on Friday", "Wait for TER-12", "Ship without the PTY change"],
      context:
        "**CI** is green on `main`. The PTY fix is in review.\n\n- Risk: a resize bug on wide screens.\n- [the review](/p/terrarium/board)\n\n<img src=x onerror=\"window.__xss=1\"> <script>window.__xss=1</script>",
      card: "TER-12",
      priority: "urgent",
      status: "open",
      revision: 2,
    },
    {
      ...base(shop, 2, 70),
      text: "Which payment provider do we use for the storefront: Stripe or Mollie?",
      options: [],
      context: null,
      card: "WEB-4",
      priority: "normal",
      status: "open",
    },
    {
      ...base(lead, 3, 60 * 5),
      text: "May I make a second worker for the adapter cards?",
      options: ["yes", "no"],
      priority: "normal",
      status: "answered",
      answer: "yes",
      answer_option: 0,
      answered_by: "operator",
      answered_by_name: "operator",
      answered_at: ago(60 * 4),
      updated_at: ago(60 * 4),
    },
    {
      ...base(lead, 4, 60 * 8),
      text: "Should the board get a WIP limit on review?",
      options: [],
      priority: "normal",
      status: "retracted",
      retract_reason: "decided with the team",
      retracted_by: `agent:${lead.id}`,
      updated_at: ago(60 * 7),
    },
  ];
}

const ORDER = (x: Question, y: Question): number => {
  const open = (q: Question) => (q.status === "open" ? 0 : 1);
  if (open(x) !== open(y)) return open(x) - open(y);
  if (x.status === "open") {
    const u = (q: Question) => (q.priority === "urgent" ? 0 : 1);
    return u(x) - u(y) || x.created_at.localeCompare(y.created_at);
  }
  return (y.answered_at ?? y.updated_at).localeCompare(x.answered_at ?? x.updated_at);
};

/** The question endpoints. */
export function createQuestionMock(c: QuestionMockContext) {
  const { w, fail, reply } = c;
  w.questions ??= seedQuestions(Date.parse(c.iso()), w.agents);
  const all = () => (w.questions ??= []);

  const question = (id: string): Question => all().find((q) => q.id === id) ?? fail(404, "not_found", `no question \`${id}\``);

  const event = (q: Question, change: QuestionEventData["change"], actor = "operator") => {
    const data: QuestionEventData = {
      question_id: q.id,
      change,
      status: q.status,
      priority: q.priority,
      text: shortText(q.text, 120),
      revision: q.revision,
      agent_id: q.agent,
      ...(q.card ? { card_id: q.card } : {}),
    };
    c.emit({
      kind: "question",
      actor,
      agent: q.agent_name.split("/")[1] ?? q.agent_name,
      agent_id: q.agent,
      project: q.project,
      ...(q.card ? { card: q.card } : {}),
      summary: `${q.agent_name.split("/")[1] ?? q.agent_name}: question ${change}: ${shortText(q.text, 80)}`,
      data: data as unknown as Record<string, unknown>,
    });
  };

  const notOpen = (q: Question, what: string) => fail(409, "conflict", `question ${q.id} is ${q.status}; ${what} needs an open question`, { status: q.status });

  const questions: Api["questions"] = {
    list: (q = {}) =>
      reply(() => {
        const status = q.status ?? "open";
        return all()
          .filter((x) => (status === "all" || x.status === status) && (!q.project || x.project === q.project) && (!q.agent || x.agent === c.agent(q.agent).id))
          .sort(ORDER);
      }),
    ofAgent: (agent, o = {}) =>
      reply(() => {
        const a = c.agent(agent);
        const day = Date.parse(c.iso()) - 24 * 3600_000;
        return all()
          .filter((x) => x.agent === a.id)
          .filter((x) => {
            if (o.status) return x.status === o.status;
            if (o.all) return true;
            return x.status === "open" || (x.status === "answered" && Date.parse(x.answered_at ?? "") >= day);
          })
          .sort(ORDER);
      }),
    get: (id) => reply(() => question(id)),
    answer: (id, body) =>
      reply(() => {
        const q = question(id);
        if (q.status !== "open") notOpen(q, "an answer");
        let index: number | undefined;
        if (typeof body.option === "number") {
          if (body.option < 0 || body.option >= q.options.length) fail(422, "invalid", `option ${body.option} is not there: the question has ${q.options.length} options`);
          index = body.option;
        } else if (typeof body.option === "string") {
          index = q.options.indexOf(body.option.trim());
          if (index < 0) fail(422, "invalid", `the question has no option \`${body.option.trim()}\``);
        }
        const text = body.answer?.trim();
        if (text && text.length > MAX_ANSWER) fail(422, "invalid", `the answer is longer than ${MAX_ANSWER} characters`);
        const answer = text || (index !== undefined ? q.options[index] : undefined);
        if (!answer) fail(422, "invalid", "an answer needs `answer` (text), `option`, or both");
        const at = c.iso();
        Object.assign(q, { status: "answered", answer, answered_by: "operator", answered_by_name: "operator", answered_at: at, updated_at: at });
        if (index !== undefined) q.answer_option = index;
        c.questionClosed(q.id, { outcome: "answered", answer, ...(index !== undefined ? { option: q.options[index] } : {}) });
        const chosen = index !== undefined ? `\nChosen option: ${index + 1}. ${q.options[index]}\n` : "";
        const own = index === undefined || answer !== q.options[index] ? `\nThe answer of operator:\n${quoted(answer!)}` : "";
        c.sendMail(q.agent, "operator", `Answer: ${shortText(q.text)}`, `operator answered your question ${q.id} (revision ${q.revision}).\n\nYour question:\n${quoted(q.text)}${chosen}${own}\nRun \`terrarium question show ${q.id}\` for the details.\n`, q.thread_id);
        c.audit("question.answer", q.id, q.project, { agent: q.agent, by: "operator", option: q.answer_option ?? null });
        event(q, "answered");
        return q;
      }),
    retract: (id, reason) =>
      reply(() => {
        const q = question(id);
        if (q.status !== "open") notOpen(q, "a retract");
        const r = reason?.trim() || null;
        Object.assign(q, { status: "retracted", retract_reason: r, retracted_by: "operator", updated_at: c.iso() });
        c.questionClosed(q.id, { outcome: "retracted" });
        c.sendMail(q.agent, "operator", `Retracted: ${shortText(q.text)}`, `The operator retracted your question ${q.id}: you need no answer to it.\n\nYour question:\n${quoted(q.text)}${r ? `\nThe reason:\n${quoted(r)}` : ""}`, q.thread_id);
        c.audit("question.retract", q.id, q.project, { agent: q.agent, reason: r, by_owner: false });
        event(q, "retracted");
        return q;
      }),
  };

  return { questions };
}

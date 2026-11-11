// Questions and notices of the leaders to the operator in the mock
// (docs/api.md §6.8): the seed, and the endpoints. The answers have the
// daemon's shapes.
//
// An answer sends one mail from the operator to the agent on the thread of
// the question, and resolves the `question` waits on it with no wake mail of
// their own (store/questions.rs). A retract by the operator sends one mail.
// An acknowledge of a notice sends one mail, with the reply when there is one.
// A bundle takes an answer for every item in one step (a partial answer is a
// 422), and sends one mail with all answers, item by item.

import type { Api } from "../client";
import type { Agent, AgentEvent, BundleItem, InboxEventData, Message, Question, QuestionAnswer, QuestionEventData } from "../types";
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
  /** An `inbox` event (./inbox.ts). */
  inboxEvent: (d: InboxEventData) => void;
  /** A question of an Inbox thread changed (./threads.ts): the thread item stands for it in the Inbox. */
  threadQuestion?: (q: Question, change: QuestionEventData["change"]) => void;
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

/**
 * The seed: two open questions (one urgent, with options and a hostile
 * context), one answered, one retracted; one open notice (with a hostile
 * context) and one acknowledged notice with a reply; one open bundle of
 * three questions (one item with a hostile context).
 */
export function seedQuestions(now: number, agents: Agent[]): Question[] {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const named = (n: string) => agents.find((a) => a.name === n);
  const lead = named("lead");
  const shop = named("shop-lead");
  if (!lead || !shop) return [];
  const id = (n: number) => `01JBQUES0000000000000000${String(n).padStart(2, "0")}`;
  const base = (a: Agent, n: number, min: number) => ({
    id: id(n),
    kind: "question" as const,
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
    {
      ...base(lead, 5, 15),
      kind: "notice",
      text: "I moved TER-16 back to ready: the push path needs the new mailbox schema first.",
      options: [],
      context: "The schema change is in **TER-18**.\n\n<img src=x onerror=\"window.__xss=1\"> <a href=\"javascript:window.__xss=1\">details</a>",
      card: "TER-16",
      priority: "normal",
      status: "open",
    },
    {
      ...base(shop, 6, 60 * 3),
      kind: "notice",
      text: "The staging database is back. I restarted the checkout tests.",
      options: [],
      priority: "normal",
      status: "acknowledged",
      answer: "Thanks. Tell me when they pass.",
      answered_by: "operator",
      answered_by_name: "operator",
      answered_at: ago(60 * 2),
      updated_at: ago(60 * 2),
    },
    {
      ...base(lead, 7, 10),
      kind: "bundle",
      text: "Release plan for v0.3: three decisions before I tag it",
      options: [],
      context: "The release branch is `release/0.3`. **CI** is green.",
      items: [
        { text: "Tag v0.3 on Friday, or wait one week for the PTY fix?", options: ["Friday", "Next week"] },
        {
          text: "Which staging host gets the release first?",
          options: ["staging-eu", "staging-us"],
          context: "`staging-us` has the newer kernel.\n\n<img src=x onerror=\"window.__xss=1\">",
        },
        { text: "Anything that must go in the release notes?", options: [] },
      ],
      card: "TER-12",
      priority: "normal",
      status: "open",
    },
  ];
}

const ORDER = (x: Question, y: Question): number => {
  const open = (q: Question) => (q.status === "open" ? 0 : 1);
  if (open(x) !== open(y)) return open(x) - open(y);
  if (x.status === "open") {
    const u = (q: Question) => (q.priority === "critical" ? 0 : q.priority === "urgent" ? 1 : 2);
    return u(x) - u(y) || x.created_at.localeCompare(y.created_at);
  }
  return (y.answered_at ?? y.updated_at).localeCompare(x.answered_at ?? x.updated_at);
};

/** The inbox item ID of a question, a bundle, or a notice. */
export const questionItemId = (q: Pick<Question, "id" | "kind">) => `${q.kind}:${q.id}`;

/** The question endpoints. */
export function createQuestionMock(c: QuestionMockContext) {
  const { w, fail, reply } = c;
  w.questions ??= seedQuestions(Date.parse(c.iso()), w.agents);
  const all = () => (w.questions ??= []);

  const question = (id: string): Question => all().find((q) => q.id === id) ?? fail(404, "not_found", `no question \`${id}\``);

  const event = (q: Question, change: QuestionEventData["change"], actor = "operator", previousCard?: string | null) => {
    const data: QuestionEventData = {
      question_id: q.id,
      change,
      kind: q.kind,
      status: q.status,
      priority: q.priority,
      text: shortText(q.text, 120),
      revision: q.revision,
      agent_id: q.agent,
      ...(q.card ? { card_id: q.card } : {}),
      ...(previousCard ? { previous_card_id: previousCard } : {}),
    };
    c.emit({
      kind: "question",
      actor,
      agent: q.agent_name.split("/")[1] ?? q.agent_name,
      agent_id: q.agent,
      project: q.project,
      ...(q.card ? { card: q.card } : {}),
      summary: `${q.agent_name.split("/")[1] ?? q.agent_name}: ${q.kind} ${change}: ${shortText(q.text, 80)}`,
      data: data as unknown as Record<string, unknown>,
    });
    // A question of a thread is not an Inbox item of its own (docs/api.md §6.13).
    if (q.thread) return c.threadQuestion?.(q, change);
    c.inboxEvent({ item: questionItemId(q), kind: q.kind === "notice" ? "notice" : "question", change: q.status === "open" || change === "updated" ? (change === "asked" ? "opened" : "updated") : "closed", project: q.project });
  };

  const findCard = (id: string) => w.cards.find((x) => x.id.toUpperCase() === id.trim().toUpperCase());

  const notOpen = (q: Question, what: string) => fail(409, "conflict", `${q.kind} ${q.id} is ${q.status}; ${what} needs an open ${q.kind}`, { status: q.status });
  const wrongKind = (q: Question, what: string) => fail(409, "conflict", `${q.id} is a ${q.kind}; ${what} needs a ${q.kind === "notice" ? "question" : "notice"}`, { kind: q.kind });

  /** The answer and the index of one item, or `null` when it has neither. */
  const resolveItem = (item: BundleItem, n: number, a: { answer?: string; option?: string | number } | undefined): { answer: string; index?: number } | null => {
    let index: number | undefined;
    if (typeof a?.option === "number") {
      if (a.option < 0 || a.option >= item.options.length) fail(422, "invalid", `option ${a.option} is not there: item ${n} has ${item.options.length} options`);
      index = a.option;
    } else if (typeof a?.option === "string") {
      index = item.options.indexOf(a.option.trim());
      if (index < 0) fail(422, "invalid", `item ${n} has no option \`${a.option.trim()}\``);
    }
    const text = a?.answer?.trim();
    if (text && text.length > MAX_ANSWER) fail(422, "invalid", `the answer of item ${n} is longer than ${MAX_ANSWER} characters`);
    const answer = text || (index !== undefined ? item.options[index] : undefined);
    return answer ? { answer, ...(index !== undefined ? { index } : {}) } : null;
  };

  const answerBundle = (q: Question, body: QuestionAnswer): Question => {
    if (body.answer !== undefined || body.option !== undefined) fail(422, "invalid", "a bundle takes `items`: one entry `{answer?, option?}` for each item, not `answer` or `option`");
    const items = q.items ?? [];
    const given = body.items ?? [];
    if (given.length > items.length) fail(422, "invalid", `the bundle has ${items.length} items; the answer has ${given.length} entries`);
    const resolved = items.map((item, i) => resolveItem(item, i + 1, given[i]));
    const missing = resolved.flatMap((r, i) => (r ? [] : [i + 1]));
    if (missing.length) {
      const list = missing.length === 1 ? `${missing[0]}` : missing.length === 2 ? `${missing[0]} and ${missing[1]}` : `${missing.slice(0, -1).join(", ")}, and ${missing[missing.length - 1]}`;
      fail(
        422,
        "invalid",
        `${missing.length === 1 ? "item" : "items"} ${list} of ${items.length} ${missing.length === 1 ? "has" : "have"} no answer: a bundle needs an answer (a text, an option, or both) for every item, in one step`,
        { missing, items: items.length },
      );
    }
    const at = c.iso();
    q.items = items.map((item, i) => {
      const r = resolved[i]!;
      return { ...item, answer: r.answer, ...(r.index !== undefined ? { answer_option: r.index } : {}) };
    });
    Object.assign(q, { status: "answered", answered_by: "operator", answered_by_name: "operator", answered_at: at, updated_at: at });
    c.questionClosed(q.id, {
      outcome: "answered",
      kind: "bundle",
      answers: q.items.map((item, i) => ({ item: i + 1, text: item.text, answer: item.answer, ...(item.answer_option != null ? { option: item.options[item.answer_option] } : {}) })),
    });
    const parts = q.items
      .map((item, i) => {
        const label = item.answer_option != null ? item.options[item.answer_option] : undefined;
        const chosen = label !== undefined ? `Chosen option: ${item.answer_option! + 1}. ${label}\n` : "";
        const own = item.answer !== label ? `The answer of operator:\n${quoted(item.answer!)}` : "";
        return `\nItem ${i + 1} of ${q.items!.length}:\n${quoted(shortText(item.text, 200))}${chosen}${own}`;
      })
      .join("");
    c.sendMail(
      q.agent,
      "operator",
      `Answers: ${shortText(q.text)}`,
      `operator answered your question bundle ${q.id} (revision ${q.revision}): all ${q.items.length} items are in this mail.\n\nYour bundle:\n${quoted(q.text)}${parts}\nRun \`terrarium question show ${q.id}\` for the details.\n`,
      q.thread_id,
    );
    c.audit("question.answer", q.id, q.project, { agent: q.agent, by: "operator", kind: "bundle", items: q.items.length });
    event(q, "answered");
    return q;
  };

  /** Retract an open question: by the operator (a mail tells the agent), or by its own agent (no mail). */
  const retractNow = (id: string, reason: string | undefined, byOwner: boolean): Question => {
    const q = question(id);
    if (q.status !== "open" && q.status !== "in_thread") notOpen(q, "a retract");
    const r = reason?.trim() || null;
    Object.assign(q, { status: "retracted", retract_reason: r, retracted_by: byOwner ? `agent:${q.agent}` : "operator", updated_at: c.iso() });
    c.questionClosed(q.id, { outcome: "retracted" });
    if (!byOwner)
      c.sendMail(q.agent, "operator", `Retracted: ${shortText(q.text)}`, `The operator retracted your ${q.kind} ${q.id}: you need no ${q.kind === "bundle" ? "answers" : "answer"} to it.\n\nYour question:\n${quoted(q.text)}${r ? `\nThe reason:\n${quoted(r)}` : ""}`, q.thread_id);
    c.audit("question.retract", q.id, q.project, { agent: q.agent, reason: r, by_owner: byOwner });
    event(q, "retracted", byOwner ? "agent" : "operator");
    return q;
  };

  // `toThread` needs the thread mock: ./index.ts adds it.
  const questions: Omit<Api["questions"], "toThread"> = {
    list: (q = {}) =>
      reply(() => {
        const status = q.status ?? "open";
        // An unknown card is a 404, as in the daemon.
        const card = q.card?.trim() ? (findCard(q.card) ?? fail(404, "not_found", `no card \`${q.card.trim()}\``)).id : undefined;
        return all()
          .filter((x) => (status === "all" || x.status === status) && (!q.kind || x.kind === q.kind) && (!q.project || x.project === q.project) && (!q.agent || x.agent === c.agent(q.agent).id) && (!card || x.card === card))
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
            return x.status === "open" || ((x.status === "answered" || x.status === "acknowledged") && Date.parse(x.answered_at ?? "") >= day);
          })
          .sort(ORDER);
      }),
    get: (id) => reply(() => question(id)),
    answer: (id, body) =>
      reply(() => {
        const q = question(id);
        if (q.kind === "notice") wrongKind(q, "an answer");
        if (q.status !== "open" && q.status !== "in_thread") notOpen(q, "an answer");
        if (body.revision !== undefined && body.revision !== q.revision)
          fail(409, "conflict", `${q.kind} ${q.id} changed: it is at revision ${q.revision}, and the answer is for revision ${body.revision}. Read it again, then answer`, { revision: q.revision, kind: q.kind });
        if (q.kind === "bundle") return answerBundle(q, body);
        if (body.items) fail(422, "invalid", "a question takes `answer`, `option`, or both; `items` is for a bundle");
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
    retract: (id, reason) => reply(() => retractNow(id, reason, false)),
    ack: (id, text) =>
      reply(() => {
        const q = question(id);
        if (q.kind !== "notice") wrongKind(q, "an acknowledge");
        if (q.status !== "open" && q.status !== "in_thread") notOpen(q, "an acknowledge");
        const t = text?.trim() || null;
        if (t && t.length > MAX_ANSWER) fail(422, "invalid", `the reply is longer than ${MAX_ANSWER} characters`);
        const at = c.iso();
        Object.assign(q, { status: "acknowledged", answer: t, answered_by: "operator", answered_by_name: "operator", answered_at: at, updated_at: at });
        c.questionClosed(q.id, { outcome: "acknowledged", ...(t ? { answer: t } : {}) });
        c.sendMail(q.agent, "operator", `Seen: ${shortText(q.text)}`, `operator acknowledged your notice ${q.id}.\n\nYour notice:\n${quoted(q.text)}${t ? `\nThe reply of operator:\n${quoted(t)}` : ""}`, q.thread_id);
        c.audit("question.ack", q.id, q.project, { agent: q.agent, by: "operator", reply: !!t });
        event(q, "acknowledged");
        return q;
      }),
    // The operator links at any status; the revision stays (store/questions.rs set_card).
    setCard: (id, card) =>
      reply(() => {
        const q = question(id);
        const key = card?.trim() || null;
        let next: string | null = null;
        if (key) {
          const found = findCard(key) ?? fail(422, "invalid", `no card \`${key}\` in project \`${q.project}\``);
          if (found.project !== q.project) fail(422, "invalid", `card \`${found.id}\` is in project \`${found.project}\`, not in \`${q.project}\``);
          next = found.id;
        }
        const previous = q.card ?? null;
        if (next === previous) return q;
        q.card = next;
        q.updated_at = c.iso();
        c.audit("question.update", q.id, q.project, { agent: q.agent, kind: q.kind, changed: ["card"], card: next, previous_card: previous, revision: q.revision });
        event(q, "updated", "operator", previous);
        return q;
      }),
  };

  return { questions, retractNow };
}

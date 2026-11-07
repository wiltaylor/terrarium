// Inbox threads in the mock (docs/api.md §6.13, store/threads.rs): the seed,
// the endpoints, the Inbox items of kind `thread`, and the agent's side
// (a post and a question of the attached agent) for the sim and the tests.
//
// The rules follow the daemon: an operator message or response makes the
// thread `waiting_agent`; an agent message or question makes it
// `waiting_operator`; the last open question settled makes it
// `waiting_agent`. A close retracts the open questions first. Every write to
// a closed thread is a 409.

import type { Api } from "../client";
import type { Agent, AgentEvent, InboxEventData, InboxItem, Question, Thread, ThreadEventData, ThreadLink, ThreadMessage, ThreadStatus } from "../types";
import type { MockWorld } from "./data";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

/** A stored thread: the thread with its messages; its questions are in `w.questions` with `thread`. */
export type MockThread = Omit<Thread, "questions" | "message_count" | "open_questions" | "last_at"> & { messages: ThreadMessage[] };

export interface ThreadMockContext {
  w: MockWorld;
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> }) => AgentEvent;
  /** An agent key: a ULID, `project/name`, or a name; `404` when unknown. */
  agent: (key: string) => Agent;
  audit: (action: string, target: string, project?: string, detail?: Record<string, unknown>) => void;
  inboxEvent: (d: InboxEventData) => void;
  /** Retract a question now, as the operator or its agent, with a reason (./questions.ts). */
  retractQuestion: (id: string, reason: string, byOwner: boolean) => void;
}

export const GLOBAL = "global";
/** The project whose lead a `global` thread reports to (D34). */
const GLOBAL_LEAD_PROJECT = "meta";
export const MAX_TOPIC = 200;
export const MAX_MESSAGE = 32 * 1024;
export const MAX_RESPONSE = 16 * 1024;
export const MAX_SUMMARY = 8 * 1024;
const MAX_LINKS = 50;
const MAX_REF = 500;
const MAX_TEMPLATE = 100;
const LIST_MAX = 200;
const LINK_KINDS = ["card", "mail", "wiki", "memory", "url", "other"];

const bytes = (s: string) => new TextEncoder().encode(s).length;

/** The ID of the Inbox item of a thread. */
export const threadItemId = (id: string) => `inbox-thread:${id}`;

/** The seed: one thread that waits for the operator with an open question, one that waits for the agent, one closed. */
export function seedThreads(now: number, agents: Agent[]): { threads: MockThread[]; questions: Question[] } {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const named = (p: string, n: string) => agents.find((a) => a.project === p && a.name === n);
  const lead = named("terrarium", "lead");
  const worker = named("terrarium", "reviewer") ?? named("terrarium", "runner-dev") ?? lead;
  const shopLead = named("webshop", "shop-lead");
  if (!lead || !worker || !shopLead) return { threads: [], questions: [] };
  const tid = (n: number) => `01JBTHRD00000000000000000${n}`;
  const mid = (n: number) => `01JBTMSG000000000000000${String(n).padStart(3, "0")}`;
  const msg = (n: number, a: Agent | "operator", text: string, min: number, kind: ThreadMessage["kind"] = "message"): ThreadMessage => ({
    id: mid(n),
    author: a === "operator" ? "operator" : a.id,
    author_name: a === "operator" ? "operator" : `${a.project}/${a.name}`,
    kind,
    text,
    at: ago(min),
  });
  const base = (n: number, topic: string, scope: string, a: Agent, reportTo: Agent, min: number) => ({
    id: tid(n),
    topic,
    scope,
    template: "lead-consult",
    opened_by: "operator",
    opened_by_name: "operator",
    agent: a.id,
    agent_name: `${a.project}/${a.name}`,
    report_to: reportTo.id,
    report_to_name: `${reportTo.project}/${reportTo.name}`,
    response: null,
    handoff: null,
    created_at: ago(min),
    closed_at: null,
    closed_by: null,
  });
  const threads: MockThread[] = [
    {
      ...base(1, "Split the event store by project?", "terrarium", worker, lead, 50),
      status: "waiting_operator",
      updated_at: ago(8),
      messages: [
        msg(1, "operator", "The event table is at 40 GB. Should we split it by project, or prune it?", 50),
        msg(2, worker, "I read `store/events.rs` and the sizes. **Pruning** keeps one table; a split needs a migration for each project.\n\nTwo questions below.", 13),
      ],
    },
    {
      ...base(2, "Checkout retries", "webshop", shopLead, shopLead, 120),
      template: null,
      status: "waiting_agent",
      updated_at: ago(30),
      messages: [msg(3, shopLead, "How many retries should the payment call get?", 110), msg(4, "operator", "Three, with a backoff. Write it up as a card.", 30)],
    },
    {
      ...base(3, "Rename the runner crate", "terrarium", worker, lead, 60 * 26),
      status: "closed",
      updated_at: ago(60 * 24),
      messages: [msg(5, worker, "Rename `runner` to `terrarium-runner`?", 60 * 26), msg(6, "operator", "Yes. Do it after v0.2.", 60 * 25, "response")],
      response: { text: "Yes. Do it after v0.2.", by: "operator", at: ago(60 * 25) },
      handoff: { summary: "TER-31 has the rename; nothing more is needed.", links: [{ kind: "card", ref: "TER-31" }], by: worker.id, at: ago(60 * 24) },
      closed_at: ago(60 * 24),
      closed_by: worker.id,
    },
  ];
  const q = (n: number, extra: Partial<Question>): Question => ({
    id: `01JBTQUE000000000000000${String(n).padStart(3, "0")}`,
    kind: "question",
    agent: worker.id,
    agent_name: `${worker.project}/${worker.name}`,
    project: worker.project,
    text: "",
    options: [],
    priority: "normal",
    status: "open",
    thread_id: `01JBTQTH000000000000000${String(n).padStart(3, "0")}`,
    thread: tid(1),
    created_at: ago(12),
    updated_at: ago(12),
    revision: 1,
    ...extra,
  });
  const questions: Question[] = [
    q(1, { text: "Prune events older than 90 days, or split the table by project?", options: ["Prune after 90 days", "Split by project"], context: "A split needs one migration per project." }),
    q(2, { text: "Keep the audit rows when we prune?", options: ["Keep them", "Prune them too"], created_at: ago(11), updated_at: ago(11) }),
  ];
  return { threads, questions };
}

export function createThreadMock(c: ThreadMockContext) {
  const { w, fail, reply } = c;
  if (!w.threads) {
    const s = seedThreads(Date.parse(c.iso()), w.agents);
    w.threads = s.threads;
    (w.questions ??= []).push(...s.questions);
  }
  const rows = () => (w.threads ??= []);
  const questionsOf = (id: string) => (w.questions ?? []).filter((q) => q.thread === id).sort((a, b) => a.created_at.localeCompare(b.created_at) || a.id.localeCompare(b.id));
  const openQuestions = (id: string) => questionsOf(id).filter((q) => q.status === "open");
  const row = (id: string): MockThread => rows().find((t) => t.id === id) ?? fail(404, "not_found", `no thread \`${id}\``);
  const project = (t: Pick<Thread, "scope">) => (t.scope === GLOBAL ? null : t.scope);

  const full = (t: MockThread): Thread => ({ ...t, messages: t.messages.map((m) => ({ ...m })), questions: questionsOf(t.id) });
  const listed = (t: MockThread): Thread => {
    const { messages, ...rest } = t;
    const last = [t.updated_at, ...messages.map((m) => m.at)].sort().at(-1)!;
    return { ...rest, message_count: messages.length, open_questions: openQuestions(t.id).length, last_at: last };
  };
  const itemOpen = (t: MockThread) => t.status === "waiting_operator" || (t.status !== "closed" && openQuestions(t.id).length > 0);
  const item = (t: MockThread): InboxItem => {
    const open = itemOpen(t);
    const oq = openQuestions(t.id);
    return {
      id: threadItemId(t.id),
      kind: "thread",
      open,
      urgent: open && oq.some((q) => q.priority !== "normal"),
      critical: open && oq.some((q) => q.priority === "critical"),
      project: project(t),
      at: t.created_at,
      thread: listed(t),
    };
  };

  /** The `thread` and `inbox` events of a change (store/threads.rs emit). */
  const emitChange = (t: MockThread, change: ThreadEventData["change"]) => {
    const data: ThreadEventData = { change, id: t.id, scope: t.scope, status: t.status };
    const p = project(t);
    c.emit({ kind: "thread", actor: "system", ...(p ? { project: p } : {}), summary: `thread ${t.id} ${change}: ${t.topic.slice(0, 80)}`, data: data as unknown as Record<string, unknown> });
    c.inboxEvent({ item: threadItemId(t.id), kind: "thread", change: change === "opened" ? "opened" : t.status === "closed" ? "closed" : "updated", ...(p ? { project: p } : {}) });
  };

  const notClosed = (t: MockThread, what: string) => {
    if (t.status === "closed") fail(409, "conflict", `thread ${t.id} is closed: ${what} needs an open thread`);
  };
  const checkText = (what: string, text: string | undefined, max: number): string => {
    const s = (text ?? "").trim();
    if (!s) fail(422, "invalid", `\`${what}\` is empty`);
    if (bytes(s) > max) fail(422, "invalid", `\`${what}\` is longer than ${max} bytes`);
    return s;
  };
  /** The lead of a project: its project_admin agent named `lead`, else its first project_admin by name. */
  const leadOf = (p: string): Agent | undefined => {
    const admins = w.agents.filter((a) => a.project === p && a.roles.includes("project_admin")).sort((a, b) => a.name.localeCompare(b.name));
    return admins.find((a) => a.name === "lead") ?? admins[0];
  };
  const addMessage = (t: MockThread, author: Agent | "operator", kind: ThreadMessage["kind"], text: string): ThreadMessage => {
    const m: ThreadMessage = {
      id: c.ulid("tmsg"),
      author: author === "operator" ? "operator" : author.id,
      author_name: author === "operator" ? "operator" : `${author.project}/${author.name}`,
      kind,
      text,
      at: c.iso(),
    };
    t.messages.push(m);
    return m;
  };

  const threads: Api["threads"] = {
    list: (q = {}) =>
      reply(() => {
        const st = q.status ?? "open";
        if (!["open", "all", "waiting_operator", "waiting_agent", "tracking", "closed"].includes(st)) fail(422, "invalid", `bad status \`${st}\`: use waiting_operator, waiting_agent, tracking, closed, open, or all`);
        return rows()
          .filter((t) => (st === "all" ? true : st === "open" ? t.status !== "closed" : t.status === st) && (!q.scope || t.scope === q.scope))
          .map(listed)
          .sort((a, b) => Number(b.status === "waiting_operator") - Number(a.status === "waiting_operator") || b.last_at!.localeCompare(a.last_at!) || b.id.localeCompare(a.id))
          .slice(0, LIST_MAX);
      }),
    get: (id) => reply(() => full(row(id))),
    open: (b) =>
      reply(() => {
        const topic = (b.topic ?? "").trim();
        if (!topic) fail(422, "invalid", "`topic` is empty");
        if ([...topic].length > MAX_TOPIC || /[\n\r\0]/.test(topic)) fail(422, "invalid", `\`topic\` is one line of at most ${MAX_TOPIC} characters`);
        const scope = b.scope?.trim() || GLOBAL;
        if (scope !== GLOBAL && !w.projects.some((p) => p.name === scope)) fail(404, "not_found", `no project \`${scope}\``);
        const template = b.template?.trim() || null;
        if (template && ([...template].length > MAX_TEMPLATE || /[\n\r\0]/.test(template))) fail(422, "invalid", `\`template\` is one line of at most ${MAX_TEMPLATE} characters`);
        const a = b.agent?.trim() ? c.agent(b.agent.trim()) : null;
        if (a && scope !== GLOBAL && a.project !== scope) fail(422, "invalid", `agent ${a.project}/${a.name} is not in project \`${scope}\`: a project thread's agent is of its project`);
        const leadProject = scope === GLOBAL ? GLOBAL_LEAD_PROJECT : scope;
        const lead = leadOf(leadProject) ?? fail(409, "conflict", `project \`${leadProject}\` has no lead (a project_admin agent), so a \`${scope}\` thread has nobody to report to (D34)`);
        const text = b.text !== undefined && b.text.trim() ? checkText("text", b.text, MAX_MESSAGE) : null;
        const at = c.iso();
        const t: MockThread = {
          id: c.ulid("thrd"),
          topic,
          scope,
          template,
          opened_by: "operator",
          opened_by_name: "operator",
          agent: a?.id ?? null,
          agent_name: a ? `${a.project}/${a.name}` : null,
          report_to: lead.id,
          report_to_name: `${lead.project}/${lead.name}`,
          status: text ? "waiting_agent" : "waiting_operator",
          messages: [],
          response: null,
          handoff: null,
          created_at: at,
          updated_at: at,
          closed_at: null,
          closed_by: null,
        };
        if (text) addMessage(t, "operator", "message", text);
        rows().push(t);
        c.audit("thread.open", t.id, project(t) ?? undefined, { topic, scope, template, agent: t.agent });
        emitChange(t, "opened");
        return full(t);
      }),
    post: (id, text) =>
      reply(() => {
        const t = row(id);
        notClosed(t, "a message");
        addMessage(t, "operator", "message", checkText("text", text, MAX_MESSAGE));
        setStatus(t, "waiting_agent");
        emitChange(t, "message");
        return full(t);
      }),
    respond: (id, text) =>
      reply(() => {
        const t = row(id);
        notClosed(t, "a response");
        const s = checkText("text", text, MAX_RESPONSE);
        const m = addMessage(t, "operator", "response", s);
        t.response = { text: s, by: "operator", at: m.at };
        setStatus(t, "waiting_agent");
        c.audit("thread.respond", t.id, project(t) ?? undefined, { text: s.slice(0, 200) });
        emitChange(t, "response");
        return full(t);
      }),
    close: (id, body = {}) =>
      reply(() => {
        const t = row(id);
        notClosed(t, "a close");
        const summary = body.summary?.trim() ? checkText("summary", body.summary, MAX_SUMMARY) : "";
        const links = body.links ?? [];
        if (links.length > MAX_LINKS) fail(422, "invalid", `a handoff has at most ${MAX_LINKS} links; it has ${links.length}`);
        const checked: ThreadLink[] = links.map((l, n) => {
          const kind = (l.kind ?? "").trim();
          if (!LINK_KINDS.includes(kind)) fail(422, "invalid", `link ${n + 1}: bad kind \`${kind}\`: use ${LINK_KINDS.join(", ")}`);
          const ref = (l.ref ?? "").trim();
          if (!ref || [...ref].length > MAX_REF || /[\n\r\0]/.test(ref)) fail(422, "invalid", `link ${n + 1}: \`ref\` is one line of 1 to ${MAX_REF} characters`);
          return { kind: kind as ThreadLink["kind"], ref };
        });
        const open = openQuestions(t.id).map((q) => q.id);
        // Closed first in the mock, so that the retracts do not move the status.
        const at = c.iso();
        Object.assign(t, { status: "closed", handoff: { summary, links: checked, by: "operator", at }, closed_at: at, closed_by: "operator", updated_at: at });
        for (const q of open) c.retractQuestion(q, `thread ${t.id} was closed`, false);
        c.audit("thread.close", t.id, project(t) ?? undefined, { by: "operator", links: checked.length, retracted: open });
        emitChange(t, "closed");
        return full(t);
      }),
  };

  function setStatus(t: MockThread, s: ThreadStatus) {
    t.status = s;
    t.updated_at = c.iso();
  }

  /** A question of a thread changed (store/threads.rs on_question). */
  const onQuestion = (q: Question, change: string) => {
    const t = q.thread ? rows().find((x) => x.id === q.thread) : undefined;
    if (!t || t.status === "closed") return;
    if (change === "asked") setStatus(t, "waiting_operator");
    else if (q.status !== "open" && openQuestions(t.id).length === 0) setStatus(t, "waiting_agent");
    else t.updated_at = c.iso();
    emitChange(t, "question");
  };

  /** The agent's side, for the sim and the tests: a post of the attached agent. */
  const agentPost = (id: string, text: string): Thread => {
    const t = row(id);
    notClosed(t, "a message");
    const a = t.agent ? c.agent(t.agent) : fail(409, "conflict", `thread ${t.id} has no agent`);
    addMessage(t, a, "message", checkText("text", text, MAX_MESSAGE));
    setStatus(t, "waiting_operator");
    emitChange(t, "message");
    return full(t);
  };
  /** The attached agent asks a question (or a bundle) in its thread. */
  const agentAsk = (id: string, q: Pick<Question, "text"> & Partial<Question>): Question => {
    const t = row(id);
    notClosed(t, "a question");
    const a = t.agent ? c.agent(t.agent) : fail(409, "conflict", `thread ${t.id} has no agent`);
    const at = c.iso();
    const out: Question = {
      id: c.ulid("ques"),
      kind: "question",
      agent: a.id,
      agent_name: `${a.project}/${a.name}`,
      project: a.project,
      options: [],
      priority: "normal",
      status: "open",
      thread_id: c.ulid("mthr"),
      created_at: at,
      updated_at: at,
      revision: 1,
      ...q,
      thread: t.id,
    };
    (w.questions ??= []).push(out);
    onQuestion(out, "asked");
    return out;
  };

  const items = (): InboxItem[] => rows().map(item);

  return { threads, items, onQuestion, agentPost, agentAsk };
}

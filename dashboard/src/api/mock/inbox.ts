// The operator's inbox in the mock: what waits for the operator. It reads
// the questions, the notices, the mail to `operator`, and the threads that
// the loop guard paused. It also has the mail calls that end a hold: release
// one message, or resume a thread.
//
// The items have the daemon's shapes (`GET /api/inbox`). A system item shows
// only while it is open. The "needs a human" notice of a paused thread is
// not a mail item: the thread item stands for it.

import type { Api } from "../client";
import type { Agent, InboxCounts, InboxEventData, InboxItem, InboxKind, InboxWatch, Message, PausedThread, Question } from "../types";
import type { MockWorld } from "./data";
import { questionItemId } from "./questions";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface InboxMockContext {
  w: MockWorld;
  iso: () => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  /** `TERRARIUM_MAIL_LOOP_LIMIT`. */
  loopLimit: number;
  /** Agent-to-agent messages on the thread since its last reset. */
  agentRun: (thread: string) => number;
  /** The row as the daemon renders it. */
  render: (m: Message) => Message;
  /** One reply for each participant, from `as`. */
  replyTo: (id: string, body: string, subject: string | undefined, as: string) => Message[];
  sendMail: (to: string, from: string, subject: string, body: string, thread?: string) => Message;
  /** A `mail` event about one message. */
  mailEvent: (m: Message, data: Record<string, unknown>) => void;
  inboxEvent: (d: InboxEventData) => void;
  /** An agent key: a ULID, `project/name`, or a name; `404` when unknown. */
  agent: (key: string) => Agent;
  audit: (action: string, target: string, project: string | undefined, detail: Record<string, unknown>) => void;
}

/** The most bytes of a forwarded context, as the daemon (`FORWARD_CONTEXT_MAX`). */
const FORWARD_CONTEXT_MAX = 4096;

/** The first line of a text that is not blank, cut at 80 characters (store/inbox.rs ask_mail). */
const firstLine = (s: string): string => {
  const line = (s.split("\n").find((l) => l.trim()) ?? "").trim();
  return [...line].length > 80 ? `${[...line].slice(0, 79).join("")}…` : line;
};

/** The subject and the body of the mail of an ask, or of a forward of `q` (store/inbox.rs ask_mail). */
export function askMail(text: string, q?: Question): { subject: string; body: string } {
  let body = "";
  let subject: string;
  if (!q) {
    body += `${text.trim()}\n\n`;
    subject = `Question from the operator: ${firstLine(text)}`;
  } else {
    if (text.trim()) body += `${text.trim()}\n\n`;
    const what = q.kind === "notice" ? "notice" : q.kind === "bundle" ? "bundle of questions" : "question";
    body += `The operator forwards this ${what} of ${q.agent_name} to you${q.card ? ` (card ${q.card})` : ""}:\n\n`;
    for (const l of q.text.split("\n")) body += `> ${l}\n`;
    q.options.forEach((o, i) => (body += `> ${i + 1}. ${o}\n`));
    (q.items ?? []).forEach((it, i) => {
      body += `> Item ${i + 1}: ${it.text}\n`;
      it.options.forEach((o, j) => (body += `>   ${j + 1}. ${o}\n`));
    });
    if (q.context?.trim()) {
      const c = q.context.length > FORWARD_CONTEXT_MAX ? `${q.context.slice(0, FORWARD_CONTEXT_MAX)}\n[cut at ${FORWARD_CONTEXT_MAX} bytes]` : q.context;
      body += ">\n> Context:\n";
      for (const l of c.split("\n")) body += `> ${l}\n`;
    }
    body += "\n";
    subject = `Forwarded ${what} from ${q.agent_name}: ${firstLine(q.text)}`;
  }
  body += "Answer the operator with `terrarium mail reply <this message>`: an acknowledgement, the answer, or your questions.\n";
  return { subject, body };
}

const byId = (a: Message, b: Message) => a.id.localeCompare(b.id);
const isUnread = (m: Message) => m.state === "queued" || m.state === "delivered";
/** The daemon's notice of a paused thread. */
export const isPauseNotice = (m: Message) => m.from_kind === "system" && /^Thread \S+ needs a human/.test(m.subject);

/** The project of a readable address: `project:<name>` or `project/agent`. */
const projectOfName = (name: string): string | null => {
  if (name.startsWith("project:")) return name.slice(8) || null;
  const i = name.indexOf("/");
  return i > 0 ? name.slice(0, i) : null;
};

export function createInboxMock(c: InboxMockContext) {
  const { w, fail, reply } = c;
  const held = () => (w.heldMail ??= []);
  const questions = (): Question[] => w.questions ?? [];

  const threadOf = (thread: string) => w.mail.filter((m) => m.thread_id === thread).sort(byId);
  const paused = (thread: string) => c.agentRun(thread) > c.loopLimit;
  /** The held messages of a thread that are not released. */
  const heldOn = (thread: string) => threadOf(thread).filter((m) => held().includes(m.id) && !m.released_at);

  // ----- the watches of "Ask and forward" -----
  const watchRows = () => (w.inboxWatches ??= []);
  const watchOf = (thread: string): InboxWatch | null => {
    const r = watchRows().find((x) => x.thread_id === thread);
    if (!r) return null;
    const msgs = threadOf(thread);
    const a = w.agents.find((x) => x.id === r.agent);
    return {
      thread_id: r.thread_id,
      agent: r.agent,
      agent_name: a ? `${a.project}/${a.name}` : r.agent,
      ...(r.question_id ? { question_id: r.question_id } : {}),
      message_id: r.message_id,
      subject: w.mail.find((m) => m.id === r.message_id)?.subject ?? "",
      created_at: r.created_at,
      ...(r.cancelled_at ? { cancelled_at: r.cancelled_at } : {}),
      waiting: msgs.at(-1)?.from_kind === "operator",
      replies: msgs.filter((m) => m.from_kind === "agent" && m.from === r.agent).length,
    };
  };
  const watchIsOpen = (x: InboxWatch | null) => !!x && x.waiting && !x.cancelled_at;
  const watchProject = (x: InboxWatch) => projectOfName(x.agent_name);
  const watchItem = (x: InboxWatch): InboxItem => ({
    id: `watch:${x.thread_id}`,
    kind: "watch",
    open: watchIsOpen(x),
    urgent: false,
    critical: false,
    project: watchProject(x),
    at: x.created_at,
    watch: x,
  });
  /** Is the watch of the thread open now? For the `inbox` event of a new message. */
  const watchOpen = (thread: string) => watchIsOpen(watchOf(thread));
  const watchEvent = (thread: string, was: boolean) => {
    const x = watchOf(thread);
    const now = watchIsOpen(x);
    if (!x || was === now) return;
    c.inboxEvent({ item: `watch:${thread}`, kind: "watch", change: now ? "opened" : "closed", ...(watchProject(x) ? { project: watchProject(x)! } : {}) });
  };

  const questionItem = (q: Question): InboxItem => {
    const open = q.status === "open";
    return {
      id: questionItemId(q),
      // A bundle is an item of the kind `question`.
      kind: q.kind === "notice" ? "notice" : "question",
      open,
      urgent: open && q.priority !== "normal",
      critical: open && q.priority === "critical",
      project: q.project,
      at: q.created_at,
      question: q,
    };
  };

  /** Mail to `operator` is open until the operator acknowledges it (the read). */
  const mailItem = (m: Message): InboxItem => ({
    id: `mail:${m.id}`,
    kind: "mail",
    open: isUnread(m),
    urgent: false,
    critical: false,
    project: m.from_kind === "system" ? null : projectOfName(m.from_name),
    at: m.created_at,
    message: c.render(m),
    ...(watchOf(m.thread_id) ? { watch: watchOf(m.thread_id)! } : {}),
  });

  const threadItem = (thread: string): InboxItem | null => {
    const msgs = threadOf(thread);
    const h = heldOn(thread);
    const isPaused = paused(thread);
    if (!isPaused && !h.length) return null;
    const agents = msgs.filter((m) => m.from_kind === "agent" || m.to_kind === "agent");
    const participants = [...new Set(agents.flatMap((m) => [m.from_kind === "agent" ? m.from_name : null, m.to_kind === "agent" ? m.to_name : null]))].filter((n): n is string => !!n);
    const first = msgs.find((m) => m.from_kind !== "system") ?? msgs[0];
    const system: PausedThread = {
      kind: "paused_thread",
      thread_id: thread,
      subject: first?.subject ?? "",
      paused: isPaused,
      agent_run: c.agentRun(thread),
      limit: c.loopLimit,
      participants,
      held: h.map(c.render),
    };
    return {
      id: `thread:${thread}`,
      kind: "system",
      open: true,
      urgent: false,
      critical: false,
      project: participants.map(projectOfName).find((p) => p) ?? null,
      at: (h[0] ?? msgs.at(-1))?.created_at ?? c.iso(),
      system,
    };
  };

  const items = (): InboxItem[] => {
    const threads = [...new Set(w.mail.map((m) => m.thread_id))];
    return [
      ...questions().map(questionItem),
      ...w.mail.filter((m) => m.to_kind === "operator" && !isPauseNotice(m)).map(mailItem),
      ...threads.map(threadItem).filter((x): x is InboxItem => !!x),
      ...watchRows().map((r) => watchItem(watchOf(r.thread_id)!)),
    ];
  };

  /** Open first, then critical, then urgent, then the newest (as the daemon). */
  const order = (a: InboxItem, b: InboxItem) =>
    Number(b.open) - Number(a.open) ||
    Number(!!b.critical) - Number(!!a.critical) ||
    Number(b.urgent) - Number(a.urgent) ||
    b.at.localeCompare(a.at) ||
    b.id.localeCompare(a.id);

  const counts = (project?: string): InboxCounts => {
    const all = items().filter((i) => i.open && (!project || i.project === project));
    // A watch waits for an agent: it is not in `open`.
    const open = all.filter((i) => i.kind !== "watch");
    const n = (k: InboxKind) => all.filter((i) => i.kind === k).length;
    return { open: open.length, urgent: open.filter((i) => i.urgent).length, critical: open.filter((i) => i.critical).length, question: n("question"), notice: n("notice"), mail: n("mail"), system: n("system"), watch: n("watch") };
  };

  /** Is the thread item open now? For the `inbox` event of a change. */
  const threadOpen = (thread: string) => threadItem(thread) !== null;
  const threadEvent = (thread: string, was: boolean) => {
    const now = threadItem(thread);
    if (!was && !now) return;
    c.inboxEvent({ item: `thread:${thread}`, kind: "system", change: !was ? "opened" : now ? "updated" : "closed", ...(now?.project ? { project: now.project } : {}) });
  };

  const release = (m: Message) => {
    m.released_at = c.iso();
    m.released_by = "operator";
    c.mailEvent(m, { released: true });
  };

  const inbox: Api["inbox"] = {
    list: (q = {}) =>
      reply(() => {
        const status = q.status ?? "open";
        if (!["open", "done", "all"].includes(status)) fail(422, "invalid", `bad status \`${status}\`: use open, done, or all`);
        if (q.kind && !["question", "notice", "mail", "system", "watch"].includes(q.kind)) fail(422, "invalid", `bad kind \`${q.kind}\`: use question, notice, mail, system, or watch`);
        return items()
          .filter((i) => (status === "all" || (status === "open") === i.open) && (!q.kind || i.kind === q.kind) && (!q.project || i.project === q.project))
          .sort(order);
      }),
    counts: (project) => reply(() => counts(project)),
    ackMail: (id, text) =>
      reply(() => {
        const m = w.mail.find((x) => x.id === id && x.to_kind === "operator" && !isPauseNotice(x)) ?? fail(404, "not_found", `no mail \`${id}\` to the operator`);
        if (!isUnread(m)) fail(409, "conflict", `mail ${m.id} is acknowledged`, { state: m.state });
        const t = text?.trim();
        const others = threadOf(m.thread_id).flatMap((x) => [x.from, x.to]).filter((a) => a !== "operator" && a !== "system");
        if (t && !others.length) fail(422, "invalid", "this mail has nobody to reply to");
        m.state = "read";
        m.read_at = c.iso();
        m.read_by ??= "operator";
        m.read_by_name ??= "operator";
        const replies = t ? c.replyTo(m.id, t, undefined, "operator") : [];
        c.inboxEvent({ item: `mail:${m.id}`, kind: "mail", change: "closed", ...(mailItem(m).project ? { project: mailItem(m).project! } : {}) });
        return { message: c.render(m), replies };
      }),
    ask: (body) =>
      reply(() => {
        const text = body.text ?? "";
        if (text.trim().length > 4000) fail(422, "invalid", "the text is over 4000 characters");
        const a = c.agent(body.to);
        const q = body.question ? (questions().find((x) => x.id === body.question) ?? fail(404, "not_found", `no question \`${body.question}\``)) : undefined;
        if (!q && !text.trim()) fail(422, "invalid", "the question text is empty");
        const { subject, body: mailBody } = askMail(text, q);
        const m = c.sendMail(a.id, "operator", subject, mailBody);
        watchRows().push({ thread_id: m.thread_id, agent: a.id, ...(q ? { question_id: q.id } : {}), message_id: m.id, created_at: c.iso() });
        const x = watchOf(m.thread_id)!;
        c.inboxEvent({ item: `watch:${x.thread_id}`, kind: "watch", change: "opened", ...(watchProject(x) ? { project: watchProject(x)! } : {}) });
        c.audit("inbox.ask", m.thread_id, a.project, { to: a.id, ...(q ? { question: q.id } : {}), message: m.id });
        return { message: m, watch: x };
      }),
    cancelWatch: (thread) =>
      reply(() => {
        const r = watchRows().find((x) => x.thread_id === thread) ?? fail(404, "not_found", `no watch of thread \`${thread}\``);
        if (r.cancelled_at) fail(409, "conflict", `the watch of thread ${thread} is cancelled already`);
        const was = watchOpen(thread);
        r.cancelled_at = c.iso();
        watchEvent(thread, was);
        const x = watchOf(thread)!;
        c.audit("inbox.watch_cancel", thread, watchProject(x) ?? undefined, {});
        return x;
      }),
  };

  const mail: Pick<Api["mail"], "release" | "resume"> = {
    release: (id) =>
      reply(() => {
        const m = w.mail.find((x) => x.id === id) ?? fail(404, "not_found", `message ${id} not found`);
        if (!held().includes(m.id)) fail(409, "conflict", `message ${m.id} is not held`);
        if (m.released_at) fail(409, "conflict", `message ${m.id} is released`);
        const was = threadOpen(m.thread_id);
        release(m);
        threadEvent(m.thread_id, was);
        return c.render(m);
      }),
    resume: (thread, note) =>
      reply(() => {
        const msgs = threadOf(thread);
        if (!msgs.length) fail(404, "not_found", `thread ${thread} not found`);
        const was = threadOpen(thread);
        // As the daemon: a thread that is not paused and has no held message is a conflict.
        if (!was) fail(409, "conflict", `thread ${thread} is not paused and has no held message`);
        // The run counts again from here.
        (w.mailResets ??= {})[thread] = msgs.at(-1)!.id;
        const freed = heldOn(thread);
        freed.forEach(release);
        const t = note?.trim();
        // The note goes to each participant, as a reply on the thread would.
        const to = [...new Set(msgs.flatMap((m) => [m.from, m.to]))].filter((a) => a !== "operator" && a !== "system");
        const subject = msgs.find((m) => m.from_kind !== "system")?.subject.replace(/^Re:\s*/i, "") ?? "";
        const sent = t ? to.map((a) => c.sendMail(a, "operator", subject ? `Re: ${subject}` : "", t, thread)) : [];
        threadEvent(thread, was);
        return { thread_id: thread, released: freed.map((m) => m.id), note: sent };
      }),
  };

  return { inbox, mail, threadOpen, threadEvent, mailItem, watchOpen, watchEvent };
}

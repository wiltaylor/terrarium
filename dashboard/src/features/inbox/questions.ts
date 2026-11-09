// Questions and notices of the leaders to the operator (docs/api.md §6.8):
// pure helpers. No Solid imports. The text and the options are the agent's
// words: the components show them as text nodes only, and the context
// (Markdown) only through the DOMPurify sink.

import type { AgentEvent, Question, QuestionAnswer, QuestionStatus } from "../../api/types";

/** The longest answer or reply, as in the daemon. */
const MAX_TEXT = 8000;

/** Open first: critical first, then urgent, then the oldest first. Then the others, the newest first. As the daemon. */
export function sortQuestions(list: Question[]): Question[] {
  const rank = (q: Question) => (q.status === "open" ? 2 - (q.priority === "critical" ? 2 : q.priority === "urgent" ? 1 : 0) : 3);
  return list.slice().sort((x, y) => {
    const r = rank(x) - rank(y);
    if (r) return r;
    if (x.status === "open") return x.created_at.localeCompare(y.created_at);
    return (y.answered_at ?? y.updated_at).localeCompare(x.answered_at ?? x.updated_at);
  });
}

/** Put the daemon's answer for one question into a list, and keep the order. */
export function putQuestion(list: Question[], q: Question): Question[] {
  const i = list.findIndex((x) => x.id === q.id);
  const next = i < 0 ? [q, ...list] : list.map((x) => (x.id === q.id ? q : x));
  return sortQuestions(next);
}

/** The body of an answer from the form: a choice, a text, or both; else the reason the form cannot send. */
export function answerBody(q: Pick<Question, "options" | "status">, text: string, option: number | null, skip = false): { body: QuestionAnswer } | { error: string } {
  if (q.status !== "open" && q.status !== "in_thread") return { error: `The question is ${q.status}.` };
  if (skip && !text.trim()) return { error: "Write a reason to skip the question." };
  const t = skip ? `Skipped question: ${text.trim()}` : text.trim();
  if (skip) option = null;
  if (option !== null && (option < 0 || option >= q.options.length)) return { error: "Pick one of the options." };
  if (!t && option === null) return { error: q.options.length ? "Pick an option or write an answer." : "Write an answer." };
  if (t.length > MAX_TEXT) return { error: `The answer is longer than ${MAX_TEXT} characters.` };
  const body: QuestionAnswer = {};
  if (option !== null) body.option = option;
  if (t) body.answer = t;
  return { body };
}

/** The optional reply of an acknowledge: the trimmed text, or undefined; else the reason the form cannot send. */
export function ackText(status: QuestionStatus, text: string): { text?: string } | { error: string } {
  if (status !== "open" && status !== "in_thread") return { error: `The notice is ${status}.` };
  const t = text.trim();
  if (t.length > MAX_TEXT) return { error: `The reply is longer than ${MAX_TEXT} characters.` };
  return t ? { text: t } : {};
}

/** The chip class of a status. */
export const statusChip = (s: QuestionStatus): string => ({ open: "info", in_thread: "info", answered: "ok", acknowledged: "ok", retracted: "" })[s] ?? "";

/** The label of the chosen option, if any. */
export const chosenLabel = (q: Pick<Question, "options" | "answer_option">): string | null =>
  q.answer_option !== null && q.answer_option !== undefined ? (q.options[q.answer_option] ?? null) : null;

/** Is the event about the questions or notices (of this agent, when one is given)? */
export const isQuestionEvent = (e: AgentEvent, agentId?: string): boolean =>
  e.kind === "question" && (!agentId || e.agent_id === agentId || e.data?.agent_id === agentId);

/** The first line, at most `max` characters, for a short label. */
export function shortQuestion(text: string, max = 80): string {
  const first = (text.split("\n").find((l) => l.trim()) ?? "").trim();
  return first.length > max || text.trim().split("\n").length > 1 ? `${first.slice(0, max)}…` : first;
}

/** The board with the drawer of `card` open (`?card=`). */
export const cardHref = (project: string, card: string): string => `/p/${encodeURIComponent(project)}/board?card=${encodeURIComponent(card)}`;

/** The Inbox item ID of a question, bundle, or notice: `<kind>:<id>` (docs/api.md §6.9). */
export const questionItemId = (q: Pick<Question, "id" | "kind">): string => `${q.kind}:${q.id}`;

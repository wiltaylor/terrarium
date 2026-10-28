// Questions of the leaders to the operator (docs/api.md §6.8): pure helpers.
// No Solid imports. The text and the options of a question are the agent's
// words: the components show them as text nodes only, and the context
// (Markdown) only through the DOMPurify sink.

import type { AgentEvent, Question, QuestionAnswer, QuestionStatus } from "../../api/types";

/** The filters of the Questions page. */
export const QUESTION_FILTERS = [
  { id: "open", label: "Open" },
  { id: "answered", label: "Answered" },
  { id: "retracted", label: "Retracted" },
  { id: "all", label: "All" },
] as const;
export type QuestionFilter = (typeof QUESTION_FILTERS)[number]["id"];

/** Open first: urgent first, then the oldest first. Then the others, the newest first. As the daemon. */
export function sortQuestions(list: Question[]): Question[] {
  const rank = (q: Question) => (q.status === "open" ? (q.priority === "urgent" ? 0 : 1) : 2);
  return list.slice().sort((x, y) => {
    const r = rank(x) - rank(y);
    if (r) return r;
    if (x.status === "open") return x.created_at.localeCompare(y.created_at);
    return (y.answered_at ?? y.updated_at).localeCompare(x.answered_at ?? x.updated_at);
  });
}

/** The badge of the nav item: the open questions, and how many of them are urgent. */
export function openCounts(list: Question[]): { open: number; urgent: number } {
  const open = list.filter((q) => q.status === "open");
  return { open: open.length, urgent: open.filter((q) => q.priority === "urgent").length };
}

/** Put the daemon's answer for one question into a list, and keep the order. */
export function putQuestion(list: Question[], q: Question): Question[] {
  const i = list.findIndex((x) => x.id === q.id);
  const next = i < 0 ? [q, ...list] : list.map((x) => (x.id === q.id ? q : x));
  return sortQuestions(next);
}

/** Does the question show under the filter? */
export const matchesFilter = (q: Question, f: QuestionFilter): boolean => f === "all" || q.status === f;

/** The body of an answer from the form: a choice, a text, or both; else the reason the form cannot send. */
export function answerBody(q: Pick<Question, "options" | "status">, text: string, option: number | null): { body: QuestionAnswer } | { error: string } {
  if (q.status !== "open") return { error: `The question is ${q.status}.` };
  const t = text.trim();
  if (option !== null && (option < 0 || option >= q.options.length)) return { error: "Pick one of the options." };
  if (!t && option === null) return { error: q.options.length ? "Pick an option or write an answer." : "Write an answer." };
  if (t.length > 8000) return { error: "The answer is longer than 8000 characters." };
  const body: QuestionAnswer = {};
  if (option !== null) body.option = option;
  if (t) body.answer = t;
  return { body };
}

/** The chip class of a status. */
export const statusChip = (s: QuestionStatus): string => ({ open: "info", answered: "ok", retracted: "" })[s] ?? "";

/** The label of the chosen option, if any. */
export const chosenLabel = (q: Pick<Question, "options" | "answer_option">): string | null =>
  q.answer_option !== null && q.answer_option !== undefined ? (q.options[q.answer_option] ?? null) : null;

/** Is the event about the questions (of this agent, when one is given)? */
export const isQuestionEvent = (e: AgentEvent, agentId?: string): boolean =>
  e.kind === "question" && (!agentId || e.agent_id === agentId || e.data?.agent_id === agentId);

/** The first line, at most `max` characters, for a short label. */
export function shortQuestion(text: string, max = 80): string {
  const first = (text.split("\n").find((l) => l.trim()) ?? "").trim();
  return first.length > max || text.trim().split("\n").length > 1 ? `${first.slice(0, max)}…` : first;
}

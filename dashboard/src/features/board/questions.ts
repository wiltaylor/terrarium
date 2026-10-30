// The questions of the cards on the board (docs/api.md §6.8, §8): pure
// helpers. No Solid imports. A question, a bundle, or a notice with `card`
// shows on that card. A bundle counts once.

import type { AgentEvent, Question, QuestionPriority } from "../../api/types";
import { priorityRank } from "../inbox/model";
import { shortQuestion } from "../inbox/questions";

/** The badge of one card: its open questions, the highest priority, and the chip class. */
export interface CardQuestionBadge {
  count: number;
  priority: QuestionPriority;
  /** The Inbox priority colours: `info` (normal), `bad` (urgent), `bad strong` (critical). */
  cls: string;
  label: string;
  /** One line for each question, for `title` and `aria-label`. */
  title: string;
}

/** The chip class of a priority, as the Inbox shows it. */
export const priorityChip = (p: QuestionPriority): string => (p === "critical" ? "chip bad strong" : p === "urgent" ? "chip bad" : "chip info");

/** The badge for each card with open questions, by card ID. Questions that are not open, or that have no card, do not count. */
export function cardQuestionBadges(questions: Question[]): Map<string, CardQuestionBadge> {
  const by = new Map<string, Question[]>();
  for (const q of questions) {
    if (q.status !== "open" || !q.card) continue;
    by.set(q.card, [...(by.get(q.card) ?? []), q]);
  }
  const out = new Map<string, CardQuestionBadge>();
  for (const [card, qs] of by) {
    const priority = qs.reduce<QuestionPriority>((p, q) => (priorityRank(q.priority) > priorityRank(p) ? q.priority : p), "normal");
    const noun = qs.length === 1 ? "open question" : "open questions";
    const lines = qs.map((q) => `${q.kind}${q.kind === "bundle" ? ` (${q.items?.length ?? 0} items)` : ""}${q.priority !== "normal" ? `, ${q.priority}` : ""}: ${shortQuestion(q.text, 80)}`);
    out.set(card, { count: qs.length, priority, cls: priorityChip(priority), label: `? ${qs.length}`, title: `${qs.length} ${noun} on ${card}${priority !== "normal" ? ` (${priority})` : ""}:\n${lines.join("\n")}` });
  }
  return out;
}

/** The open questions of the project that the operator can link to a card: no card yet. */
export const linkable = (questions: Question[], project: string): Question[] => questions.filter((q) => q.status === "open" && q.project === project && !q.card);

/** Is the event about a question of this project (for the badges), or of this card (for the drawer)? A change of the card names the old card too. */
export function isCardQuestionEvent(e: AgentEvent, filter: { project?: string; card?: string }): boolean {
  if (e.kind !== "question") return false;
  if (filter.project && e.project !== filter.project) return false;
  if (!filter.card) return true;
  const d = e.data ?? {};
  return e.card === filter.card || d.card_id === filter.card || d.previous_card_id === filter.card;
}

/** The questions of one card: the open ones, and the closed ones (answered, acknowledged, retracted) for the "Answered" group. */
export function splitCardQuestions(questions: Question[]): { open: Question[]; closed: Question[] } {
  return { open: questions.filter((q) => q.status === "open"), closed: questions.filter((q) => q.status !== "open") };
}

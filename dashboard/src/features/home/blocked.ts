// Home, Blocked (TER-51): the cards in the blocked column of every project
// the caller may read (`GET /api/cards?columns=blocked`, docs/api.md §8).
// Pure helpers for the section.

import type { ActiveCard } from "../../api/types";
import { duration } from "../../lib/format";

/** The first line of the blocking comment, trimmed; empty without one. */
export function reasonLine(c: Pick<ActiveCard, "blocked_reason">): string {
  return (c.blocked_reason ?? "").trim().split(/\r?\n/, 1)[0]!.trim();
}

/** How long the card has been blocked: `2h 05m`, or empty without a time. */
export function blockedFor(c: Pick<ActiveCard, "blocked_since">, now: number): string {
  if (!c.blocked_since) return "";
  const t = Date.parse(c.blocked_since);
  return Number.isNaN(t) ? "" : duration(now - t);
}

/** The count for the section title: `1 card` or `3 cards`. */
export function blockedSummary(cards: readonly ActiveCard[]): string {
  return `${cards.length} card${cards.length === 1 ? "" : "s"}`;
}

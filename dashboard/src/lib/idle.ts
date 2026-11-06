// The idle mark of a card (docs/api.md §6.2 "Idle alert", TER-67): the
// assignee rests with no wait past the project's idle alert, and the leads
// got a mail. Pure functions, no Solid imports.

import type { Card } from "../api/types";
import { span } from "./eta";

export interface IdleBadge {
  /** `idle 25m`. */
  label: string;
  /** The tooltip: since when, and that the leads were told. */
  title: string;
}

/** The badge of a card with an idle mark; `null` for a card without one, or a done or archived card. */
export function idleBadge(c: Pick<Card, "idle_since" | "column" | "archived">, now = Date.now()): IdleBadge | null {
  if (!c.idle_since || c.column === "done" || c.archived) return null;
  const since = Date.parse(c.idle_since);
  if (Number.isNaN(since)) return null;
  return {
    label: `idle ${span(now - since, false)}`,
    title: `The assignee rests with no wait since ${new Date(since).toLocaleString()}. The project's leads were told.`,
  };
}

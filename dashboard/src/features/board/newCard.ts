// The New card dialog of the board: pure helpers. No Solid imports. The "+"
// button of one column opens the dialog: the column where new cards go.

import type { NewCard } from "../../api/types";
import { parseList } from "../agents/form";
import { resolveEta } from "../../lib/eta";

/** The column where a new card goes when the client gives no column. It is the daemon default. */
export const DEFAULT_NEW_CARD_COLUMN = "backlog";

/** The column with the "+" button: `backlog`, or the first column of a board with no backlog column. */
export function newCardColumn(columns: readonly string[]): string | undefined {
  return columns.includes(DEFAULT_NEW_CARD_COLUMN) ? DEFAULT_NEW_CARD_COLUMN : columns[0];
}

/** The fields of the New card dialog, as the user types them. */
export interface NewCardForm {
  title: string;
  body: string;
  labels: string;
  assignee: string;
  column: string;
  dependsOn: string;
  eta: string;
}

export function emptyNewCardForm(column: string): NewCardForm {
  return { title: "", body: "", labels: "", assignee: "", column, dependsOn: "", eta: "" };
}

/**
 * The form after "Create and add another": the text fields are empty, and
 * the column, the assignee, and the labels stay, for the next card of a set.
 */
export function nextNewCardForm(f: NewCardForm): NewCardForm {
  return { ...emptyNewCardForm(f.column), labels: f.labels, assignee: f.assignee };
}

export type NewCardErrors = Partial<Record<"title" | "eta", string>>;

/**
 * The create body (`POST /api/projects/{name}/cards`) for the form, or the
 * field errors. The title is necessary. The ETA uses the daemon's rule
 * (`resolveEta`). An empty optional field is not in the body.
 */
export function newCardBody(f: NewCardForm, now = Date.now()): { body: NewCard } | { errors: NewCardErrors } {
  const errors: NewCardErrors = {};
  const title = f.title.trim();
  if (!title) errors.title = "A card needs a title.";
  const eta = f.eta.trim();
  if (eta) {
    const r = resolveEta(eta, now);
    if ("error" in r) errors.eta = r.error;
  }
  if (errors.title || errors.eta) return { errors };
  const labels = parseList(f.labels);
  const dependsOn = parseList(f.dependsOn);
  return {
    body: {
      title,
      column: f.column,
      ...(f.body.trim() ? { body: f.body } : {}),
      ...(labels.length ? { labels } : {}),
      ...(f.assignee ? { assignee: f.assignee } : {}),
      ...(dependsOn.length ? { depends_on: dependsOn } : {}),
      ...(eta ? { eta_at: eta } : {}),
    },
  };
}

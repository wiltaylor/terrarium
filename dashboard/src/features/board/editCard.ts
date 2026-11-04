// The Edit form of the card drawer: pure helpers. No Solid imports.

import type { Card, CardPatch } from "../../api/types";
import { parseList } from "../agents/form";

/** The fields of the Edit form, as the user types them. The lists are comma-separated. */
export interface EditCardForm {
  title: string;
  body: string;
  labels: string;
  dependsOn: string;
}

export function editCardForm(c: Card): EditCardForm {
  return { title: c.title, body: c.body, labels: c.labels.join(", "), dependsOn: c.depends_on.join(", ") };
}

const sameList = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * The patch (`PATCH /api/cards/{id}`) for the form: only the fields that
 * changed. An empty list field sends `[]`, which removes every label or
 * dependency (issue #13). The patch is empty when nothing changed.
 */
export function editCardPatch(c: Card, f: EditCardForm): CardPatch {
  const patch: CardPatch = {};
  if (f.title !== c.title) patch.title = f.title;
  if (f.body !== c.body) patch.body = f.body;
  const labels = parseList(f.labels);
  if (!sameList(labels, c.labels)) patch.labels = labels;
  const dependsOn = parseList(f.dependsOn);
  if (!sameList(dependsOn, c.depends_on)) patch.depends_on = dependsOn;
  return patch;
}

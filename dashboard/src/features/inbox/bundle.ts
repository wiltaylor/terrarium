// Question bundles (docs/api.md §6.8, kind `bundle`): pure helpers for the
// answer form. No Solid imports. The operator answers every item before the
// form sends, and the daemon refuses a partial answer (422). The draft is in
// sessionStorage for each bundle and revision, so a reload keeps it. When
// the agent changes the bundle, the answers to the items that did not
// change carry over to the new revision.

import type { BundleItem, Question, QuestionAnswer } from "../../api/types";
import { answerBody } from "./questions";

/** The longest answer of one item, as in the daemon. */
const MAX_TEXT = 8000;

/** The answer in progress for one item: the chosen option (0-based) and the text. */
export interface ItemDraft {
  options?: number[];
  option: number | null;
  text: string;
  skip?: boolean;
}

export type BundleDraft = ItemDraft[];

/** The items of a question; `[]` for a question or a notice. */
export const itemsOf = (q: Pick<Question, "items">): BundleItem[] => q.items ?? [];

/** An empty draft: one entry for each item. */
export const emptyDraft = (q: Pick<Question, "items">): BundleDraft => itemsOf(q).map(() => ({ option: null, text: "" }));

/** Does the item have an answer (an option, a text, or both)? */
export const itemDone = (d: ItemDraft | undefined): boolean => !!d && ((!d.skip && d.option !== null) || d.text.trim() !== "");

/** How many items have an answer. */
export const answeredCount = (draft: BundleDraft): number => draft.filter(itemDone).length;

/** The 1-based numbers of the items with no answer. */
export function missingItems(q: Pick<Question, "items">, draft: BundleDraft): number[] {
  return itemsOf(q)
    .map((_, i) => i + 1)
    .filter((n) => !(itemsOf(q)[n - 1]!.multi_select && !draft[n - 1]?.skip) && !itemDone(draft[n - 1]));
}

/** Can the form send: is the bundle open, and does every item have an answer? */
export const isComplete = (q: Pick<Question, "items" | "status">, draft: BundleDraft): boolean =>
  q.status === "open" && itemsOf(q).length > 0 && missingItems(q, draft).length === 0;

/** "3 of 5 answered". */
export const progressText = (q: Pick<Question, "items">, draft: BundleDraft): string => `${answeredCount(draft.slice(0, itemsOf(q).length))} of ${itemsOf(q).length} answered`;

/** "1, 3, and 4": a list of numbers for a message. */
export function andList(n: number[]): string {
  if (n.length <= 1) return n.join("");
  if (n.length === 2) return `${n[0]} and ${n[1]}`;
  return `${n.slice(0, -1).join(", ")}, and ${n[n.length - 1]}`;
}

/** The body of the answer of the whole bundle, with the revision; else the reason the form cannot send. */
export function bundleAnswerBody(q: Pick<Question, "items" | "status" | "revision">, draft: BundleDraft): { body: QuestionAnswer } | { error: string } {
  if (q.status !== "open") return { error: `The bundle is ${q.status}.` };
  const items = itemsOf(q);
  const missing = missingItems(q, draft);
  if (missing.length) return { error: `Answer ${missing.length === 1 ? "item" : "items"} ${andList(missing)} first. The bundle goes in one step.` };
  const out: QuestionAnswer["items"] = [];
  for (const [i, item] of items.entries()) {
    const d = draft[i] ?? { option: null, text: "" };
    if (item.multi_select) {
      const result = answerBody({ status: q.status, options: item.options, multi_select: true }, d.text, null, d.skip, d.options ?? []);
      if ("error" in result) return { error: `Item ${i + 1}: ${result.error}` };
      out.push(result.body);
      continue;
    }
    if (d.skip) {
      const result = answerBody({ status: q.status, options: item.options }, d.text, null, true);
      if ("error" in result) return { error: `Item ${i + 1}: ${result.error}` };
      out.push(result.body);
      continue;
    }
    const t = d.text.trim();
    if (d.option !== null && (d.option < 0 || d.option >= item.options.length)) return { error: `Item ${i + 1}: pick one of the options.` };
    if (t.length > MAX_TEXT) return { error: `Item ${i + 1}: the answer is longer than ${MAX_TEXT} characters.` };
    out.push({ ...(d.option !== null ? { option: d.option } : {}), ...(t ? { answer: t } : {}) });
  }
  return { body: { items: out, revision: q.revision } };
}

/** The sessionStorage key of a draft: one for each bundle and revision. */
export const draftKey = (q: Pick<Question, "id" | "revision">): string => `terrarium.bundle-draft.${q.id}.${q.revision}`;

/** A draft that fits the items, else `null`. Each entry is checked: storage is not trusted. */
export function parseDraft(q: Pick<Question, "items">, raw: string | null): BundleDraft | null {
  if (!raw) return null;
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return null;
  }
  const items = itemsOf(q);
  if (!Array.isArray(v) || v.length !== items.length) return null;
  const out: BundleDraft = [];
  for (const [i, e] of v.entries()) {
    if (!e || typeof e !== "object") return null;
    const { option, text, skip, options } = e as Record<string, unknown>;
    const selected = Array.isArray(options) ? [...new Set(options.filter((n): n is number => Number.isInteger(n) && n >= 0 && n < items[i]!.options.length))] : [];
    const opt = typeof option === "number" && Number.isInteger(option) && option >= 0 && option < items[i]!.options.length ? option : null;
    out.push({ option: skip === true ? null : opt, text: typeof text === "string" ? text : "", ...(items[i]!.multi_select ? { options: selected } : {}), ...(skip === true ? { skip: true } : {}) });
  }
  return out;
}

/** The part of storage that the draft needs. */
export type DraftStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** The session storage, or `null` when the browser refuses it. */
export function sessionDraftStorage(): DraftStorage | null {
  try {
    return typeof sessionStorage === "undefined" ? null : sessionStorage;
  } catch {
    return null;
  }
}

/** The stored draft of this revision, else `null`. Storage errors count as no draft. */
export function loadDraft(store: DraftStorage | null, q: Pick<Question, "id" | "revision" | "items">): BundleDraft | null {
  try {
    return parseDraft(q, store?.getItem(draftKey(q)) ?? null);
  } catch {
    return null;
  }
}

/** Keep the draft of this revision. An empty draft removes the key. Storage errors are ignored: the draft stays in the page. */
export function saveDraft(store: DraftStorage | null, q: Pick<Question, "id" | "revision">, draft: BundleDraft): void {
  try {
    if (!store) return;
    if (answeredCount(draft) === 0 && draft.every((d) => !d.text && !d.skip && !d.options?.length)) store.removeItem(draftKey(q));
    else store.setItem(draftKey(q), JSON.stringify(draft));
  } catch {
    /* a full or blocked storage: the draft stays in the page only */
  }
}

/** Remove the draft of a revision (after the answer, or after a new revision). */
export function clearDraft(store: DraftStorage | null, q: Pick<Question, "id" | "revision">): void {
  try {
    store?.removeItem(draftKey(q));
  } catch {
    /* ignored */
  }
}

const sameItem = (a: BundleItem, b: BundleItem) => !!a.multi_select === !!b.multi_select && a.text === b.text && a.options.length === b.options.length && a.options.every((o, i) => o === b.options[i]);

/**
 * The draft for a new revision of a bundle: an item whose text and options
 * did not change keeps its answer; the others start empty. `changed` gives
 * the 1-based numbers of the new items that lost an answer or are new.
 */
export function carryDraft(before: Pick<Question, "items">, draft: BundleDraft, after: Pick<Question, "items">): { draft: BundleDraft; changed: number[] } {
  const old = itemsOf(before);
  const changed: number[] = [];
  const next = itemsOf(after).map((item, i) => {
    const j = old.findIndex((o) => sameItem(o, item));
    if (j >= 0 && draft[j]) return { ...draft[j]! };
    if (!(old[i] && sameItem(old[i]!, item))) changed.push(i + 1);
    return { option: null, text: "" };
  });
  return { draft: next, changed };
}

/** The label of the chosen option of an answered item, if any. */
export const chosenItemLabel = (item: BundleItem): string | null =>
  item.multi_select ? (item.answer_options?.map((i) => item.options[i]).join(", ") || null) : item.answer_option !== null && item.answer_option !== undefined ? (item.options[item.answer_option] ?? null) : null;

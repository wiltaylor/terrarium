// The logic of the model combobox (ModelPicker.tsx): the filter, the limit,
// and the keyboard reducer. Pure, so the tests need no DOM.

import type { ModelInfo } from "../api/types";

/** The most matches the list shows at one time. */
export const MAX_SHOWN = 50;

/** `1000000` -> `1M`, `204800` -> `204.8K`. */
export function tokens(n: number): string {
  if (n >= 1_000_000) return `${+(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${+(n / 1_000).toFixed(1)}K`;
  return String(n);
}

export interface Filtered {
  /** The matches to show, in the list order, at most `limit`. */
  shown: ModelInfo[];
  /** The matches past the limit. */
  more: number;
}

/** True when `m` matches `query`: a case-insensitive substring of the id or the name. An empty query matches all. */
export function matches(m: ModelInfo, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return m.id.toLowerCase().includes(q) || (m.name ?? "").toLowerCase().includes(q);
}

/** The models that match `query`, at most `limit`, and how many more match. */
export function filterModels(models: readonly ModelInfo[], query: string, limit = MAX_SHOWN): Filtered {
  const all = models.filter((m) => matches(m, query));
  return { shown: all.slice(0, limit), more: Math.max(0, all.length - limit) };
}

/** The hint under a cut list. */
export function moreText(more: number): string | null {
  return more > 0 ? `${more} more — keep typing to narrow the list.` : null;
}

/** `context` in short form, or "". */
export function contextText(m: ModelInfo): string {
  return m.context ? `${tokens(m.context)} context` : "";
}

/** True when `value` is a model id that the loaded list does not have. `""` (the default) is never "not in the list". */
export function notInList(value: string, models: readonly ModelInfo[] | undefined, def?: string | null): boolean {
  const v = value.trim();
  if (!v || !models || v === def) return false;
  return !models.some((m) => m.id === v);
}

/** The state of the list: open or closed, and the highlighted row (-1: none). */
export interface PickerState {
  open: boolean;
  active: number;
}

export const CLOSED: PickerState = { open: false, active: -1 };

export type PickerKey = "ArrowDown" | "ArrowUp" | "Home" | "End" | "Enter" | "Escape" | "Tab";

export interface KeyResult {
  state: PickerState;
  /** The row to pick, when the key picks one. */
  pick?: number;
  /** True when the key belongs to the picker: the caller prevents the default (no form submit, no modal close). */
  handled: boolean;
}

/**
 * The keyboard reducer. `count` is the number of rows shown.
 *
 * - Down and Up open the list, then move the highlight, with a wrap.
 * - Home and End go to the first and the last row of an open list.
 * - Enter picks the highlighted row. With no highlight it only closes the
 *   list, so the typed text stays as the value.
 * - Escape closes an open list. Tab closes it and moves on.
 */
export function pickerKey(s: PickerState, key: string, count: number): KeyResult {
  const last = count - 1;
  switch (key) {
    case "ArrowDown":
      if (!s.open) return { state: { open: true, active: s.active >= 0 && s.active <= last ? s.active : count ? 0 : -1 }, handled: true };
      return { state: { open: true, active: count ? (s.active >= last ? 0 : s.active + 1) : -1 }, handled: true };
    case "ArrowUp":
      if (!s.open) return { state: { open: true, active: count ? last : -1 }, handled: true };
      return { state: { open: true, active: count ? (s.active <= 0 ? last : s.active - 1) : -1 }, handled: true };
    case "Home":
    case "End":
      if (!s.open || !count) return { state: s, handled: false };
      return { state: { open: true, active: key === "Home" ? 0 : last }, handled: true };
    case "Enter":
      if (!s.open) return { state: s, handled: false };
      if (s.active >= 0 && s.active <= last) return { state: CLOSED, pick: s.active, handled: true };
      return { state: CLOSED, handled: true };
    case "Escape":
      return s.open ? { state: CLOSED, handled: true } : { state: s, handled: false };
    case "Tab":
      return { state: CLOSED, handled: false };
    default:
      return { state: s, handled: false };
  }
}

/** After a keystroke in the text: the list opens with no highlight, so Enter keeps the typed id. */
export function afterInput(): PickerState {
  return { open: true, active: -1 };
}

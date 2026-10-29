// The keyboard of a sidebar tree: the WAI-ARIA tree pattern with a roving
// tabindex. Each tree (Projects, Agents) has its own instance. The key map is
// pure, so tests can read it.

import { createSignal } from "solid-js";

/** What the focused row can do. */
export interface TreeRow {
  /** The row key, as in `data-key`. */
  key: string;
  /** The key of the parent row. Absent for a root. */
  parent?: string;
  /** Set for a row that opens: true while it is open. */
  open?: boolean;
  /** True for a row that is not a link: Enter and Space activate it. */
  button?: boolean;
}

/** The result of a key on a row. */
export type TreeAction =
  | { focus: string }
  | { open: true }
  | { close: true }
  | { activate: true }
  /** Space on a link: Enter follows the link by itself; Space does not. */
  | { click: true }
  /** The key belongs to the tree, but there is nothing to do. */
  | { none: true };

/**
 * The action for `key` on `row`, with `keys` the rows in screen order.
 * Null for a key the tree does not use (the browser keeps it).
 */
export function treeKeyAction(keys: readonly string[], row: TreeRow, key: string): TreeAction | null {
  const idx = keys.indexOf(row.key);
  const to = (k: string | undefined): TreeAction => (k && k !== row.key ? { focus: k } : { none: true });
  switch (key) {
    case "ArrowDown":
      return to(keys[idx + 1]);
    case "ArrowUp":
      return to(idx > 0 ? keys[idx - 1] : undefined);
    case "Home":
      return to(keys[0]);
    case "End":
      return to(keys[keys.length - 1]);
    case "ArrowRight":
      if (row.open === undefined) return null;
      return row.open ? to(keys[idx + 1]) : { open: true };
    case "ArrowLeft":
      if (row.open) return { close: true };
      return row.parent ? { focus: row.parent } : { none: true };
    case "Enter":
      return row.button ? { activate: true } : null;
    case " ":
      return row.button ? { activate: true } : { click: true };
  }
  return null;
}

/**
 * The roving tabindex of one tree. `keys` gives the rows in screen order;
 * `preferred` gives the row that Tab reaches when no row had the focus yet.
 * The first key is the root.
 */
export function createRovingTree(opts: { keys: () => string[]; preferred?: () => string | null }) {
  const [focusKey, setFocusKey] = createSignal<string | null>(null);
  let tree: HTMLElement | undefined;

  // The one row that Tab reaches: the last focused row, else the preferred row, else the root.
  const tabKey = () => {
    const k = opts.keys();
    const f = focusKey();
    if (f && k.includes(f)) return f;
    const p = opts.preferred?.();
    if (p && k.includes(p)) return p;
    return k[0];
  };

  const focusRow = (key: string) => {
    setFocusKey(key);
    tree?.querySelector<HTMLElement>(`[data-key="${CSS.escape(key)}"]`)?.focus();
  };

  return {
    ref: (el: HTMLElement) => (tree = el),
    tabIndex: (key: string) => (tabKey() === key ? 0 : -1),
    setFocusKey,
    focusRow,
    /**
     * Handle a key on the row `row()` gives for the event target. The tree
     * gives the open, close, and activate steps of that row.
     */
    onKeyDown: (
      e: KeyboardEvent,
      row: (target: HTMLElement) =>
        | (TreeRow & {
            onOpen?: () => void;
            onClose?: () => void;
            onActivate?: () => void;
          })
        | null,
    ) => {
      const target = e.target as HTMLElement;
      const r = row(target);
      if (!r) return;
      const action = treeKeyAction(opts.keys(), r, e.key);
      if (!action) return;
      e.preventDefault();
      if ("focus" in action) focusRow(action.focus);
      else if ("open" in action) r.onOpen?.();
      else if ("close" in action) r.onClose?.();
      else if ("activate" in action) r.onActivate?.();
      else if ("click" in action) target.click();
    },
  };
}

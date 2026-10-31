// The root of the sidebar tree (Projects): a link to the list page, with a
// chevron that only opens or closes the root, an optional filter, and the
// group of child rows. TreeNode is the Agents and Repos node of a project:
// the same, but its row is not a link.

import { A } from "@solidjs/router";
import { Show, type JSX } from "solid-js";
import type { createRovingTree } from "./treeNav";
import { createExpandedStore } from "./treeModel";

/** The `data-key` of a tree root row. */
export const ROOT_KEY = "root";

/** The open set of the sidebar tree: the Projects root, the projects, and their Agents and Repos nodes. */
export const expanded = createExpandedStore(
  (() => {
    try {
      return typeof localStorage === "undefined" ? null : localStorage;
    } catch {
      return null;
    }
  })(),
);

export function TreeRoot(props: {
  label: string;
  href: string;
  /** The `data-key` of the row. The default is ROOT_KEY. */
  rowKey?: string;
  /** The row class after `tree-row`. The default is `tree-root`. */
  rowClass?: string;
  /** The `data-project` of the row, for a node under a project. */
  project?: string;
  /** The id of the child group, for aria-owns. Unique on the page. */
  groupId: string;
  open: boolean;
  onOpen: () => void;
  onToggle: () => void;
  nav: ReturnType<typeof createRovingTree>;
  /** After the label, on the root row. */
  badge?: JSX.Element;
  /** Show the filter input (when the root is open). */
  showFilter?: boolean;
  query?: string;
  onQuery?: (q: string) => void;
  children: JSX.Element;
}) {
  const key = () => props.rowKey ?? ROOT_KEY;
  return (
    // The root is a link (WAI-ARIA navigation tree): its group follows it and aria-owns joins them.
    <li role="none">
      <A
        href={props.href}
        end
        activeClass="active"
        class={`tree-row ${props.rowClass ?? "tree-root"}`}
        role="treeitem"
        aria-expanded={props.open}
        aria-owns={props.groupId}
        data-key={key()}
        data-project={props.project}
        tabIndex={props.nav.tabIndex(key())}
        onFocus={() => props.nav.setFocusKey(key())}
        onClick={() => {
          props.onOpen();
          props.nav.setFocusKey(key());
        }}
      >
        <span
          class={`tree-chevron${props.open ? " open" : ""}`}
          aria-hidden="true"
          title={props.open ? "Close" : "Open"}
          onClick={(e) => {
            // Toggle only: do not follow the link.
            e.preventDefault();
            e.stopPropagation();
            props.onToggle();
            props.nav.setFocusKey(key());
          }}
        >
          ▸
        </span>
        <span class="tree-name">{props.label}</span>
        {props.badge}
      </A>
      <Show when={props.open && props.showFilter}>
        <input
          type="search"
          class="tree-filter"
          placeholder={`Filter ${props.label.toLowerCase()}`}
          aria-label={`Filter ${props.label.toLowerCase()}`}
          value={props.query ?? ""}
          onInput={(e) => props.onQuery?.(e.currentTarget.value)}
        />
      </Show>
      <Show when={props.open}>
        <ul role="group" class="tree-group tree-root-group" id={props.groupId}>
          {props.children}
        </ul>
      </Show>
    </li>
  );
}

/**
 * A node under a project (Agents, Repos): its row only opens and closes the
 * node, so it is not a link and not selectable. A click on the row, Enter, or
 * Space toggles it (the tree gives the keys). `action` goes at the right of
 * the row (the "+" button). Its children follow the row, as on the root.
 */
export function TreeNode(props: {
  label: string;
  /** The accessible name of the row. The default is the label. */
  ariaLabel?: string;
  rowKey: string;
  project: string;
  /** The id of the child group, for aria-owns. Unique on the page. */
  groupId: string;
  open: boolean;
  onToggle: () => void;
  nav: ReturnType<typeof createRovingTree>;
  /** After the label. */
  badge?: JSX.Element;
  /** At the right of the row. */
  action?: JSX.Element;
  /** The key of the action, for aria-keyshortcuts. */
  actionKey?: string;
  showFilter?: boolean;
  query?: string;
  onQuery?: (q: string) => void;
  children: JSX.Element;
}) {
  return (
    <li role="none">
      <div
        class="tree-row tree-node"
        role="treeitem"
        aria-expanded={props.open}
        aria-owns={props.groupId}
        aria-label={props.ariaLabel ?? props.label}
        aria-keyshortcuts={props.actionKey}
        data-key={props.rowKey}
        data-project={props.project}
        tabIndex={props.nav.tabIndex(props.rowKey)}
        onFocus={(e) => {
          if (e.target === e.currentTarget) props.nav.setFocusKey(props.rowKey);
        }}
        onClick={() => {
          props.onToggle();
          props.nav.setFocusKey(props.rowKey);
        }}
      >
        <span class={`tree-chevron${props.open ? " open" : ""}`} aria-hidden="true">
          ▸
        </span>
        <span class="tree-name">{props.label}</span>
        {props.badge}
        {props.action}
      </div>
      <Show when={props.open && props.showFilter}>
        <input
          type="search"
          class="tree-filter"
          placeholder={`Filter ${props.label.toLowerCase()}`}
          aria-label={`Filter ${props.label.toLowerCase()}`}
          value={props.query ?? ""}
          onInput={(e) => props.onQuery?.(e.currentTarget.value)}
        />
      </Show>
      <Show when={props.open}>
        <ul role="group" class="tree-group tree-root-group" id={props.groupId}>
          {props.children}
        </ul>
      </Show>
    </li>
  );
}

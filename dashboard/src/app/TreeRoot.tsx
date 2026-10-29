// The root of a sidebar tree (Projects, Agents): a link to the list page,
// with a chevron that only opens or closes the root, an optional filter, and
// the group of child rows.

import { A } from "@solidjs/router";
import { Show, type JSX } from "solid-js";
import type { createRovingTree } from "./treeNav";
import { createExpandedStore } from "./treeModel";

/** The `data-key` of a tree root row. */
export const ROOT_KEY = "root";

/** The open set of both sidebar trees. One store, so the two trees do not write over each other. */
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
  return (
    // The root is a link (WAI-ARIA navigation tree): its group follows it and aria-owns joins them.
    <li role="none">
      <A
        href={props.href}
        end
        activeClass="active"
        class="tree-row tree-root"
        role="treeitem"
        aria-expanded={props.open}
        aria-owns={props.groupId}
        data-key={ROOT_KEY}
        tabIndex={props.nav.tabIndex(ROOT_KEY)}
        onFocus={() => props.nav.setFocusKey(ROOT_KEY)}
        onClick={() => {
          props.onOpen();
          props.nav.setFocusKey(ROOT_KEY);
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
            props.nav.setFocusKey(ROOT_KEY);
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

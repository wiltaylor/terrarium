import { A } from "@solidjs/router";
import { For, Show } from "solid-js";
import { bookmarks } from "../stores";
import { skippedReport } from "../stores/bookmarks";
import { ErrorBanner } from "../components/ui";
import { expanded, TreeNode } from "./TreeRoot";
import { createRovingTree } from "./treeNav";

import { BOOKMARKS_KEY } from "./treeModel";
export { BOOKMARKS_KEY } from "./treeModel";
export const bookmarkKey = (href: string) => `bookmark:${href}`;

/** A real node of the Projects navigation tree. */
export function Bookmarks(props: { nav?: ReturnType<typeof createRovingTree> }) {
  const nav = props.nav ?? createRovingTree({ keys: () => [BOOKMARKS_KEY, ...bookmarks.items().map((b) => bookmarkKey(b.href))] });
  return (
    <TreeNode label="Bookmarks" rowKey={BOOKMARKS_KEY} project="" groupId="bookmarks-group" open={expanded.isOpen(BOOKMARKS_KEY)} onToggle={() => expanded.toggle(BOOKMARKS_KEY)} nav={nav} actionKey="F2" action={
      <A href="/bookmarks" class="tree-cog" tabIndex={-1} activeClass="active" aria-label="Manage bookmarks" title="Manage bookmarks (F2)" onClick={(e) => e.stopPropagation()}>
        <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true">
          <path d="M6.6 1.5h2.8l.4 1.9 1.3.7 1.8-.7 1.4 2.4-1.5 1.3v1.6l1.5 1.3-1.4 2.4-1.8-.7-1.3.7-.4 1.9H6.6l-.4-1.9-1.3-.7-1.8.7-1.4-2.4 1.5-1.3V7.1L1.7 5.8l1.4-2.4 1.8.7 1.3-.7z" />
          <circle cx="8" cy="8" r="2" />
        </svg>
      </A>
    }>
      <For each={bookmarks.items()} fallback={<li role="none" class="muted small">No bookmarks.</li>}>
        {(b) => (
          <li role="none" class="row">
            <Show when={b.href.startsWith("/")} fallback={<a role="treeitem" data-key={bookmarkKey(b.href)} tabIndex={nav.tabIndex(bookmarkKey(b.href))} onFocus={() => nav.setFocusKey(bookmarkKey(b.href))} href={b.href} target="_blank" rel="noopener noreferrer" title="Opens in a new window">{b.title}</a>}>
              <A role="treeitem" data-key={bookmarkKey(b.href)} tabIndex={nav.tabIndex(bookmarkKey(b.href))} onFocus={() => nav.setFocusKey(bookmarkKey(b.href))} href={b.href}>{b.title}</A>
            </Show>
          </li>
        )}
      </For>
      <li role="none"><ErrorBanner error={bookmarks.error()} /></li>
      <li role="none"><ErrorBanner error={skippedReport(bookmarks.skipped())} onDismiss={bookmarks.dismissSkipped} /></li>
    </TreeNode>
  );
}

import { A } from "@solidjs/router";
import { createSignal, For, Show } from "solid-js";
import { bookmarks } from "../stores";
import { ErrorBanner } from "../components/ui";
import { expanded, TreeNode } from "./TreeRoot";
import { createRovingTree } from "./treeNav";

export const BOOKMARKS_KEY = "bookmarks";
export const bookmarkKey = (href: string) => `bookmark:${href}`;

/** A real node of the Projects navigation tree. */
export function Bookmarks(props: { nav?: ReturnType<typeof createRovingTree> }) {
  const nav = props.nav ?? createRovingTree({ keys: () => [BOOKMARKS_KEY, ...bookmarks.items().map((b) => bookmarkKey(b.href))] });
  const [title, setTitle] = createSignal("");
  const [url, setUrl] = createSignal("");
  const [error, setError] = createSignal(false);
  return (
    <TreeNode label="Bookmarks" rowKey={BOOKMARKS_KEY} project="" groupId="bookmarks-group" open={expanded.isOpen(BOOKMARKS_KEY)} onToggle={() => expanded.toggle(BOOKMARKS_KEY)} nav={nav}>
      <For each={bookmarks.items()} fallback={<li role="none" class="muted small">No bookmarks.</li>}>
        {(b) => (
          <li role="none" class="row">
            <Show when={b.href.startsWith("/")} fallback={<a role="treeitem" data-key={bookmarkKey(b.href)} tabIndex={nav.tabIndex(bookmarkKey(b.href))} onFocus={() => nav.setFocusKey(bookmarkKey(b.href))} href={b.href} target="_blank" rel="noopener noreferrer" title="Opens in a new window">{b.title}</a>}>
              <A role="treeitem" data-key={bookmarkKey(b.href)} tabIndex={nav.tabIndex(bookmarkKey(b.href))} onFocus={() => nav.setFocusKey(bookmarkKey(b.href))} href={b.href}>{b.title}</A>
            </Show>
            <button aria-label={`Remove bookmark ${b.title}`} onClick={() => bookmarks.remove(b.href)}>×</button>
          </li>
        )}
      </For>
      <li role="none"><ErrorBanner error={bookmarks.error()} /><form class="stack" onSubmit={(e) => {
        e.preventDefault();
        if (!bookmarks.add(title(), url())) { setError(true); return; }
        setTitle(""); setUrl(""); setError(false);
      }}>
        <input aria-label="Bookmark title" placeholder="Title" required value={title()} onInput={(e) => setTitle(e.currentTarget.value)} />
        <input aria-label="Bookmark URL" placeholder="https://…" required value={url()} onInput={(e) => setUrl(e.currentTarget.value)} />
        <Show when={error()}><p role="alert">Enter a wiki page URL or an HTTP(S) URL and a title.</p></Show>
        <button type="submit">Add bookmark</button>
      </form></li>
    </TreeNode>
  );
}

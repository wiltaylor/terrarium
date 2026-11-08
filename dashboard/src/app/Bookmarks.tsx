import { A } from "@solidjs/router";
import { createSignal, For, Show } from "solid-js";
import { bookmarks } from "../stores/bookmarks";

/** Browser-local bookmarks, below Projects in the sidebar. */
export function Bookmarks() {
  const [title, setTitle] = createSignal("");
  const [url, setUrl] = createSignal("");
  const [error, setError] = createSignal(false);
  return (
    <details class="nav-section">
      <summary>Bookmarks</summary>
      <p class="muted small">Saved in this browser.</p>
      <For each={bookmarks.items()} fallback={<p class="muted small">No bookmarks.</p>}>
        {(b) => (
          <div class="row">
            <Show when={b.href.startsWith("/")} fallback={<a href={b.href} target="_blank" rel="noopener noreferrer" title="Opens in a new window">{b.title}</a>}>
              <A href={b.href}>{b.title}</A>
            </Show>
            <button aria-label={`Remove bookmark ${b.title}`} onClick={() => bookmarks.remove(b.href)}>×</button>
          </div>
        )}
      </For>
      <form class="stack" onSubmit={(e) => {
        e.preventDefault();
        if (!bookmarks.add(title(), url())) { setError(true); return; }
        setTitle(""); setUrl(""); setError(false);
      }}>
        <input aria-label="Bookmark title" placeholder="Title" required value={title()} onInput={(e) => setTitle(e.currentTarget.value)} />
        <input aria-label="Bookmark URL" placeholder="https://…" required value={url()} onInput={(e) => setUrl(e.currentTarget.value)} />
        <Show when={error()}><p role="alert">Enter a wiki page URL or an HTTP(S) URL and a title.</p></Show>
        <button type="submit">Add bookmark</button>
      </form>
    </details>
  );
}

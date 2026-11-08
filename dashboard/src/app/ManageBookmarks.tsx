import { createSignal, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../components/ui";
import { bookmarks } from "../stores";
import type { Bookmark } from "../stores/bookmarks";

export default function ManageBookmarks() {
  const [editing, setEditing] = createSignal<Bookmark | null | undefined>();
  const [title, setTitle] = createSignal("");
  const [url, setUrl] = createSignal("");
  const [error, setError] = createSignal(false);
  const open = (bookmark: Bookmark | null) => {
    setTitle(bookmark?.title ?? "");
    setUrl(bookmark?.href ?? "");
    setError(false);
    setEditing(bookmark);
  };
  return <section class="stack">
    <div class="spread"><h1>Manage bookmarks</h1><button onClick={() => open(null)}>Add bookmark</button></div>
    <ErrorBanner error={bookmarks.error()} />
    <ul class="stack">
      <For each={bookmarks.items()} fallback={<li class="muted">No bookmarks.</li>}>{(b) => <li class="spread">
        <div><strong>{b.title}</strong><div class="muted">{b.href}</div></div>
        <div class="row">
          <button aria-label={`Edit bookmark ${b.title}`} onClick={() => open(b)}>Edit</button>
          <button aria-label={`Remove bookmark ${b.title}`} onClick={() => bookmarks.remove(b.href)}>Remove</button>
        </div>
      </li>}</For>
    </ul>
    <Show when={editing() !== undefined}>
      <Modal title={editing() ? "Edit bookmark" : "Add bookmark"} onClose={() => setEditing(undefined)}>
        <form class="stack" onSubmit={(e) => {
          e.preventDefault();
          if (!bookmarks.add(title(), url(), editing()?.href)) { setError(true); return; }
          setEditing(undefined);
        }}>
          <label>Name<input aria-label="Bookmark title" required value={title()} onInput={(e) => setTitle(e.currentTarget.value)} /></label>
          <label>Link<input aria-label="Bookmark URL" required value={url()} onInput={(e) => setUrl(e.currentTarget.value)} /></label>
          <Show when={error()}><p role="alert">Enter a wiki page URL or an HTTP(S) URL and a title (up to 500 bytes, URL up to 8192 bytes). The URL must not belong to another bookmark.</p></Show>
          <div class="row"><button type="submit">Save</button><button type="button" onClick={() => setEditing(undefined)}>Cancel</button></div>
        </form>
      </Modal>
    </Show>
  </section>;
}

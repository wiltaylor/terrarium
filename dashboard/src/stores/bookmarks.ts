import { createSignal } from "solid-js";
import type { Api } from "../api/client";

export type Bookmark = { title: string; href: string };
const KEY = "terrarium.bookmarks";

/** Only dashboard wiki paths and HTTP(S) links can become bookmarks. */
export function bookmarkHref(value: string, origin: string): string | null {
  try {
    const url = new URL(value, origin);
    if (!/^https?:$/.test(url.protocol) || url.username || url.password) return null;
    if (url.origin === origin && /^\/p\/[^/]+\/knowledge\/wiki(?:\/|$)/.test(url.pathname)) {
      return url.pathname + url.search + url.hash;
    }
    // Relative non-wiki paths are not external URLs.
    return /^https?:\/\//i.test(value.trim()) ? url.href : null;
  } catch {
    return null;
  }
}

export function createBookmarksStore(storage: Pick<Storage, "getItem" | "setItem"> | null, origin: string, remote?: Api["bookmarks"]) {
  const initial: Bookmark[] = [];
  try {
    const raw: unknown = JSON.parse(storage?.getItem(KEY) ?? "[]");
    if (Array.isArray(raw)) {
      for (const item of raw) {
        if (!item || typeof item.title !== "string" || typeof item.href !== "string") continue;
        const href = bookmarkHref(item.href, origin);
        if (href && item.title.trim() && !initial.some((b) => b.href === href)) initial.push({ title: item.title.trim(), href });
      }
    }
  } catch { /* Storage can be blocked or malformed. */ }
  const [items, setItems] = createSignal(initial);
  const [error, setError] = createSignal<unknown>(null);
  // Serialize reads, migration and writes so a late read cannot undo a mutation.
  let queue = Promise.resolve();
  let migrated = false;
  const enqueue = (work: () => Promise<void>) => {
    queue = queue.then(work).catch(async (e: unknown) => {
      // Do not leave a failed optimistic mutation looking saved.
      if (remote && migrated) {
        try { setItems(await remote.list()); } catch { /* Keep the last visible list while offline. */ }
      }
      setError(e);
    });
    return queue;
  };
  const migrate = async () => {
    if (!remote || migrated) return;
    const existing = await remote.list();
    for (const b of initial) {
      if (!existing.some((row) => row.href === b.href)) await remote.add(b.title, b.href);
    }
    // Clear the legacy list only after every import succeeded. Replays deduplicate.
    try { storage?.setItem(KEY, "[]"); } catch { /* Deduplication also makes replay safe. */ }
    migrated = true;
  };
  const refresh = () => enqueue(async () => {
    await migrate();
    if (remote) setItems(await remote.list());
    setError(null);
  });
  const save = (next: Bookmark[]) => {
    setItems(next);
    if (remote) return;
    try { storage?.setItem(KEY, JSON.stringify(next)); } catch { /* Keep the in-memory list. */ }
  };
  return {
    items,
    error,
    refresh,
    has: (href: string) => items().some((b) => b.href === bookmarkHref(href, origin)),
    add: (title: string, value: string, previousHref?: string) => {
      const href = bookmarkHref(value, origin);
      if (!href || !title.trim() || new TextEncoder().encode(title.trim()).length > 500 || new TextEncoder().encode(href).length > 8192) return false;
      if (previousHref && items().some((b) => b.href === href && b.href !== previousHref)) return false;
      const old = previousHref ?? href;
      const next = { title: title.trim(), href };
      save(items().some((b) => b.href === old) ? items().map((b) => b.href === old ? next : b) : [...items(), next]);
      if (remote) void enqueue(async () => {
        await migrate();
        setItems(await remote.add(title.trim(), href, previousHref));
        setError(null);
      });
      return true;
    },
    remove: (value: string) => {
      const href = bookmarkHref(value, origin);
      if (!href) return;
      save(items().filter((b) => b.href !== href));
      if (remote) void enqueue(async () => {
        await migrate();
        setItems(await remote.remove(href));
        setError(null);
      });
    },
  };
}

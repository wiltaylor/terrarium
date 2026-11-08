import { createSignal } from "solid-js";

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

export function createBookmarksStore(storage: Pick<Storage, "getItem" | "setItem"> | null, origin: string) {
  let initial: Bookmark[] = [];
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
  const save = (next: Bookmark[]) => {
    setItems(next);
    try { storage?.setItem(KEY, JSON.stringify(next)); } catch { /* Keep the in-memory list. */ }
  };
  return {
    items,
    has: (href: string) => items().some((b) => b.href === bookmarkHref(href, origin)),
    add: (title: string, value: string) => {
      const href = bookmarkHref(value, origin);
      if (!href || !title.trim()) return false;
      save([...items().filter((b) => b.href !== href), { title: title.trim(), href }]);
      return true;
    },
    remove: (href: string) => save(items().filter((b) => b.href !== href)),
  };
}

let storage: Storage | null = null;
try { storage = window.localStorage; } catch { /* Private browsing may block access. */ }
export const bookmarks = createBookmarksStore(storage, window.location.origin);

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import { ApiError } from "../api/http";

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

/** The daemon refused the value itself (422): retrying the same item cannot help. */
const refused = (e: unknown) => e instanceof ApiError && (e.status === 400 || e.status === 422);

/**
 * The bookmarks of the sidebar. With `remote`, the daemon keeps them for the
 * operator. A caller that the daemon forbids (403: not the operator) keeps
 * them in this browser instead, with no error banner on each refresh
 * (TER-257).
 */
const bytes = (s: string) => new TextEncoder().encode(s).length;

/** Why `add(title, value)` refuses a new bookmark, for the message that the page shows (TER-257). */
export function bookmarkAddError(title: string, value: string, origin: string): string {
  if (!title.trim()) return "This page needs a title before it can be pinned.";
  if (bytes(title.trim()) > 500) return "The title of this page is longer than 500 bytes, the limit of a bookmark title.";
  const href = bookmarkHref(value, origin);
  if (!href || bytes(href) > 8192) return "This URL cannot be a bookmark: it must be a wiki page or an HTTP(S) URL of up to 8192 bytes.";
  return "This URL already belongs to another bookmark.";
}

/** The one report of the legacy bookmarks that the daemon refused at the import. */
export function skippedReport(rows: readonly Bookmark[]): Error | null {
  if (!rows.length) return null;
  const names = rows.map((b) => (b.title.length > 60 ? `${b.title.slice(0, 60)}…` : b.title)).join(", ");
  return new Error(`${rows.length === 1 ? "1 old bookmark of this browser was" : `${rows.length} old bookmarks of this browser were`} not imported, the daemon refused them: ${names}.`);
}

export function createBookmarksStore(storage: Pick<Storage, "getItem" | "setItem"> | null, origin: string, server?: Api["bookmarks"]) {
  let remote = server;
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
  // The legacy bookmarks that the daemon refused at the import, for one report.
  const [skipped, setSkipped] = createSignal<Bookmark[]>([]);
  // Serialize reads, migration and writes so a late read cannot undo a mutation.
  let queue = Promise.resolve();
  let migrated = false;
  const enqueue = (work: () => Promise<void>) => {
    queue = queue.then(work).catch(async (e: unknown) => {
      if (remote && e instanceof ApiError && e.status === 403) {
        // Not the operator: the bookmarks stay in this browser.
        remote = undefined;
        save(items());
        setError(null);
        return;
      }
      // Do not leave a failed optimistic mutation looking saved.
      const r = remote;
      if (r && migrated) {
        try { setItems(await r.list()); } catch { /* Keep the last visible list while offline. */ }
      }
      setError(e);
    });
    return queue;
  };
  const migrate = async () => {
    const r = remote;
    if (!r || migrated) return;
    const existing = await r.list();
    const refusedRows: Bookmark[] = [];
    for (const b of initial) {
      if (existing.some((row) => row.href === b.href)) continue;
      try {
        await r.add(b.title, b.href);
      } catch (e) {
        // One bad legacy row must not block the import and every later change.
        if (!refused(e)) throw e;
        refusedRows.push(b);
      }
    }
    if (refusedRows.length) setSkipped(refusedRows);
    // Clear the legacy list only after every import succeeded or was refused. Replays deduplicate.
    try { storage?.setItem(KEY, "[]"); } catch { /* Deduplication also makes replay safe. */ }
    migrated = true;
  };
  const refresh = () => enqueue(async () => {
    await migrate();
    const r = remote;
    if (r) setItems(await r.list());
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
    /** The legacy bookmarks that the daemon refused at the import. They are not kept. */
    skipped,
    dismissSkipped: () => setSkipped([]),
    refresh,
    has: (href: string) => items().some((b) => b.href === bookmarkHref(href, origin)),
    add: (title: string, value: string, previousHref?: string) => {
      const href = bookmarkHref(value, origin);
      if (!href || !title.trim() || bytes(title.trim()) > 500 || bytes(href) > 8192) return false;
      if (previousHref && items().some((b) => b.href === href && b.href !== previousHref)) return false;
      const old = previousHref ?? href;
      const next = { title: title.trim(), href };
      save(items().some((b) => b.href === old) ? items().map((b) => b.href === old ? next : b) : [...items(), next]);
      const r = remote;
      if (r) void enqueue(async () => {
        await migrate();
        setItems(await r.add(title.trim(), href, previousHref));
        setError(null);
      });
      return true;
    },
    remove: (value: string) => {
      const href = bookmarkHref(value, origin);
      if (!href) return;
      save(items().filter((b) => b.href !== href));
      const r = remote;
      if (r) void enqueue(async () => {
        await migrate();
        setItems(await r.remove(href));
        setError(null);
      });
    },
  };
}

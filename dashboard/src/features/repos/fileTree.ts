// The file tree of the repo browser (Files tab): the loaded folders, the open
// set, the rows in screen order, and the filter. The folders load one level
// at a time (`files?path=`), on the first open. The model is pure, so tests
// can read it; createFileTree adds the signals and the loads.

import { createSignal } from "solid-js";
import type { TreeEntry, TreeListing } from "../../api/types";
import { dirOf } from "../../lib/diff";

/** One folder of the cache. */
export type DirState =
  | { status: "loading" }
  | { status: "ready"; entries: TreeEntry[]; truncated: boolean }
  | { status: "error"; error: unknown };

/** One row of the tree: an entry, or a note under a folder (loading, empty, error, cut). */
export type TreeItem =
  | { type: "entry"; key: string; entry: TreeEntry; depth: number; open?: boolean }
  | { type: "note"; key: string; depth: number; note: "loading" | "empty" | "error" | "truncated" };

const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

/** Folders first, then the other entries. Each group in name order, with no case (then with case). */
export function sortEntries(entries: readonly TreeEntry[]): TreeEntry[] {
  return [...entries].sort((a, b) => Number(a.kind !== "dir") - Number(b.kind !== "dir") || cmp(a.name.toLowerCase(), b.name.toLowerCase()) || cmp(a.name, b.name));
}

/** The folders above a path, from the root: `a/b/c.md` -> ["", "a", "a/b"]. The root has none. */
export function ancestorDirs(path: string): string[] {
  const parts = path.split("/").filter(Boolean);
  return parts.map((_, i) => parts.slice(0, i).join("/"));
}

// A path has no NUL (the daemon refuses it), so a note key cannot be a path.
const noteKey = (dir: string, note: string) => `\0${note}:${dir}`;

/** True for the key of an entry row; false for a note row. */
export const isEntryKey = (key: string): boolean => !key.startsWith("\0");

/**
 * The rows in screen order. Without a filter: the root, and each open folder
 * below its row. With a filter: each loaded entry whose name has the text
 * (no case), and the folders above it, open, whatever the open set says.
 */
export function visibleRows(dirs: Readonly<Record<string, DirState>>, expanded: ReadonlySet<string>, filter = ""): TreeItem[] {
  const q = filter.trim().toLowerCase();
  const out: TreeItem[] = [];

  // Without a filter: the rows of one folder, and its notes.
  const walk = (dir: string, depth: number) => {
    const d = dirs[dir];
    if (!d || d.status === "loading") return void out.push({ type: "note", key: noteKey(dir, "loading"), depth, note: "loading" });
    if (d.status === "error") return void out.push({ type: "note", key: noteKey(dir, "error"), depth, note: "error" });
    if (!d.entries.length) out.push({ type: "note", key: noteKey(dir, "empty"), depth, note: "empty" });
    for (const e of d.entries) {
      const open = e.kind === "dir" ? expanded.has(e.path) : undefined;
      out.push({ type: "entry", key: e.path, entry: e, depth, open });
      if (open) walk(e.path, depth + 1);
    }
    if (d.truncated) out.push({ type: "note", key: noteKey(dir, "truncated"), depth, note: "truncated" });
  };

  // With a filter: the matches of one folder, depth first. True when it has one.
  const match = (dir: string, depth: number): boolean => {
    const d = dirs[dir];
    if (!d || d.status !== "ready") return false;
    let any = false;
    for (const e of d.entries) {
      const at = out.length;
      const self = e.name.toLowerCase().includes(q);
      const below = e.kind === "dir" && match(e.path, depth + 1);
      if (self || below) {
        out.splice(at, 0, { type: "entry", key: e.path, entry: e, depth, open: e.kind === "dir" ? below : undefined });
        any = true;
      }
    }
    return any;
  };

  if (q) match("", 0);
  else walk("", 0);
  return out;
}

/** The entry at `path` in the cache, or undefined when its folder is not loaded (or has no such name). */
export function entryAt(dirs: Readonly<Record<string, DirState>>, path: string): TreeEntry | undefined {
  const d = dirs[dirOf(path)];
  return d?.status === "ready" ? d.entries.find((e) => e.path === path) : undefined;
}

/**
 * The tree of one repo tree at one head: the folder cache, the open set,
 * and the loads. `load` lists one folder (`api.repos.files`).
 */
export function createFileTree(load: (dir: string) => Promise<TreeListing>) {
  const [dirs, setDirs] = createSignal<Record<string, DirState>>({});
  const [expanded, setExpanded] = createSignal<ReadonlySet<string>>(new Set());
  const pending = new Map<string, Promise<void>>();

  const setDir = (dir: string, s: DirState): void => void setDirs((d) => ({ ...d, [dir]: s }));

  /** Load a folder once. A failed load tries again on the next call. */
  const ensure = (dir: string): Promise<void> => {
    const d = dirs()[dir];
    if (d?.status === "ready") return Promise.resolve();
    const p = pending.get(dir);
    if (p) return p;
    setDir(dir, { status: "loading" });
    const run = load(dir)
      .then((l) => setDir(dir, { status: "ready", entries: sortEntries(l.entries), truncated: l.truncated }))
      .catch((error: unknown) => setDir(dir, { status: "error", error }))
      .finally(() => pending.delete(dir));
    pending.set(dir, run);
    return run;
  };

  const setOpen = (dir: string, open: boolean) =>
    setExpanded((s) => {
      if (s.has(dir) === open) return s;
      const n = new Set(s);
      if (open) n.add(dir);
      else n.delete(dir);
      return n;
    });

  const open = (dir: string) => {
    setOpen(dir, true);
    return ensure(dir);
  };

  return {
    dirs,
    expanded,
    ensure,
    open,
    close: (dir: string) => setOpen(dir, false),
    toggle: (dir: string) => (expanded().has(dir) ? setOpen(dir, false) : void open(dir)),
    /** Open the folders above `path` and load each level (in parallel). */
    reveal: (path: string) => Promise.all([ensure(""), ...ancestorDirs(path).filter(Boolean).map(open)]).then(() => undefined),
    entry: (path: string) => entryAt(dirs(), path),
  };
}

export type FileTree = ReturnType<typeof createFileTree>;

/** At most this many trees stay in the cache (tree and head pairs). */
export const CACHE_SIZE = 6;

/**
 * A cache of file trees by key (repo, tree, head, refresh). The last used
 * tree is the newest; the oldest goes when the cache is full.
 */
export function createTreeCache(size = CACHE_SIZE) {
  const trees = new Map<string, FileTree>();
  return (key: string, load: (dir: string) => Promise<TreeListing>): FileTree => {
    let t = trees.get(key);
    if (t) trees.delete(key);
    else t = createFileTree(load);
    trees.set(key, t);
    while (trees.size > size) trees.delete(trees.keys().next().value!);
    return t;
  };
}

// Project folders in the sidebar (docs/api.md §6.12, TER-48): the tree of
// folders and projects, the checks of a move, and the names in the open set.
// Pure, so tests can read it.

import type { ProjectFolder, ProjectPlacement } from "../../api/types";

/** The deepest level of a folder: a root folder is level 1 (the daemon's MAX_DEPTH). */
export const MAX_FOLDER_DEPTH = 8;

/**
 * The name of a folder in the open set of the sidebar: `folder:<id>`. A
 * project name has no `:`, so this name cannot be a project or a node.
 */
export const folderNode = (id: string): string => `folder:${id}`;

/** The folder ID of an open-set name, or null for any other name. */
export function folderOfNode(name: string): string | null {
  return name.startsWith("folder:") ? name.slice("folder:".length) : null;
}

/** The `data-key` of a folder row. */
export const folderKey = (id: string): string => `f:${id}`;

export type TreeEntry = { kind: "folder"; folder: ProjectFolder; children: TreeEntry[] } | { kind: "project"; name: string };

/**
 * The tree of the sidebar. In each parent, the folders come first, in
 * position order, then the projects: the placed ones in position order,
 * and at the root after them the projects without a place, by name. A
 * folder whose parent is gone sits at the root; a placement of a project
 * that is not in `projects` is left out, and a project in a folder that is
 * gone sits at the root.
 */
export function buildTree(folders: readonly ProjectFolder[], placements: readonly ProjectPlacement[], projects: readonly string[]): TreeEntry[] {
  const ids = new Set(folders.map((f) => f.id));
  const parentOf = (f: ProjectFolder) => (f.parent_id && ids.has(f.parent_id) ? f.parent_id : null);
  const known = new Set(projects);
  const placed = new Map<string, ProjectPlacement>();
  for (const p of placements) if (known.has(p.project)) placed.set(p.project, p);
  const folderOf = (name: string) => {
    const f = placed.get(name)?.folder_id ?? null;
    return f && ids.has(f) ? f : null;
  };
  const byPos = <T extends { position: number }>(a: T, b: T) => a.position - b.position;
  const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
  const seen = new Set<string>();

  const level = (parent: string | null): TreeEntry[] => {
    const subs = folders
      .filter((f) => parentOf(f) === parent && !seen.has(f.id))
      .sort((a, b) => byPos(a, b) || cmp(a.name, b.name))
      .map((f): TreeEntry => {
        seen.add(f.id);
        return { kind: "folder", folder: f, children: level(f.id) };
      });
    const here = projects.filter((n) => folderOf(n) === parent);
    const withPlace = here.filter((n) => placed.get(n)?.folder_id === parent).sort((a, b) => placed.get(a)!.position - placed.get(b)!.position || cmp(a, b));
    const without = here.filter((n) => placed.get(n)?.folder_id !== parent).sort(cmp);
    return [...subs, ...[...withPlace, ...without].map((name): TreeEntry => ({ kind: "project", name }))];
  };
  const root = level(null);
  // A loop of parents (bad data) is not reachable from the root: show those folders at the root.
  for (const f of folders) {
    if (seen.has(f.id)) continue;
    seen.add(f.id);
    root.unshift({ kind: "folder", folder: f, children: [] });
  }
  return root;
}

/** The IDs of the folders above `id`, the root one first. Stops at a loop. */
export function ancestors(folders: readonly ProjectFolder[], id: string | null): string[] {
  const parent = new Map(folders.map((f) => [f.id, f.parent_id]));
  const out: string[] = [];
  let at = id ? (parent.get(id) ?? null) : null;
  while (at && !out.includes(at) && parent.has(at)) {
    out.unshift(at);
    at = parent.get(at) ?? null;
  }
  return out;
}

/** The folders that hold `project`, the root one first; empty at the root. */
export function projectAncestors(folders: readonly ProjectFolder[], placements: readonly ProjectPlacement[], project: string): string[] {
  const f = placements.find((p) => p.project === project)?.folder_id ?? null;
  if (!f || !folders.some((x) => x.id === f)) return [];
  return [...ancestors(folders, f), f];
}

/** The level of a folder: 1 at the root. */
export function folderLevel(folders: readonly ProjectFolder[], id: string): number {
  return ancestors(folders, id).length + 1;
}

/** The levels of the subtree of `id`, `id` included: 1 for a folder without subfolders. */
export function folderHeight(folders: readonly ProjectFolder[], id: string, seen = new Set<string>()): number {
  if (seen.has(id)) return 0;
  seen.add(id);
  const subs = folders.filter((f) => f.parent_id === id).map((f) => folderHeight(folders, f.id, seen));
  return 1 + Math.max(0, ...subs);
}

/** Whether `id` is `ancestor` or below it. */
export function isWithin(folders: readonly ProjectFolder[], id: string, ancestor: string): boolean {
  return id === ancestor || ancestors(folders, id).includes(ancestor);
}

/** What a drag or a move carries. */
export type Moving = { kind: "project"; name: string } | { kind: "folder"; id: string };

/**
 * Whether `what` may go into `target` (a folder ID, or null for the root):
 * not a folder into itself or below it, not deeper than MAX_FOLDER_DEPTH,
 * and not where it is already.
 */
export function canMoveTo(folders: readonly ProjectFolder[], placements: readonly ProjectPlacement[], what: Moving, target: string | null): boolean {
  if (target !== null && !folders.some((f) => f.id === target)) return false;
  if (what.kind === "project") return (placements.find((p) => p.project === what.name)?.folder_id ?? null) !== target;
  const f = folders.find((x) => x.id === what.id);
  if (!f || f.parent_id === target) return false;
  if (target === null) return true;
  if (isWithin(folders, target, what.id)) return false;
  return folderLevel(folders, target) + folderHeight(folders, what.id) <= MAX_FOLDER_DEPTH;
}

/** The path of a folder for display: `Work / Infra`. */
export function folderPath(folders: readonly ProjectFolder[], id: string): string {
  const name = (x: string) => folders.find((f) => f.id === x)?.name ?? "?";
  return [...ancestors(folders, id), id].map(name).join(" / ");
}

/** Every folder in tree order, with its level (1 at the root), for the Move to folder list. */
export function folderList(folders: readonly ProjectFolder[]): { folder: ProjectFolder; level: number }[] {
  const out: { folder: ProjectFolder; level: number }[] = [];
  const walk = (entries: TreeEntry[], depth: number) => {
    for (const e of entries)
      if (e.kind === "folder") {
        out.push({ folder: e.folder, level: depth });
        walk(e.children, depth + 1);
      }
  };
  walk(buildTree(folders, [], []), 1);
  return out;
}

/**
 * The rows of the folder tree in screen order. `projectRows(name)` gives
 * the project row and, when it is open, its rows below it.
 */
export function folderTreeKeys(entries: readonly TreeEntry[], isOpen: (name: string) => boolean, projectRows: (name: string) => string[]): string[] {
  return entries.flatMap((e) =>
    e.kind === "project" ? projectRows(e.name) : [folderKey(e.folder.id), ...(isOpen(folderNode(e.folder.id)) ? folderTreeKeys(e.children, isOpen, projectRows) : [])],
  );
}

/** The folder row that holds each row of the tree: `f:<id>` keys by project name and by folder ID. Absent at the root. */
export function parentFolders(entries: readonly TreeEntry[]): { projects: Map<string, string>; folders: Map<string, string> } {
  const projects = new Map<string, string>();
  const folders = new Map<string, string>();
  const walk = (list: readonly TreeEntry[], parent: string | null) => {
    for (const e of list) {
      if (e.kind === "project") {
        if (parent) projects.set(e.name, folderKey(parent));
      } else {
        if (parent) folders.set(e.folder.id, folderKey(parent));
        walk(e.children, e.folder.id);
      }
    }
  };
  walk(entries, null);
  return { projects, folders };
}

/** The number of projects in a folder and its subfolders. */
export function projectCount(entry: TreeEntry): number {
  return entry.kind === "project" ? 1 : entry.children.reduce((n, c) => n + projectCount(c), 0);
}

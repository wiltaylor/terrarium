// The sidebar Projects tree: the open root and folders, the project filter,
// and the rows in screen order. Pure, so tests can read it.

import { createSignal } from "solid-js";
import type { KeyValueStorage } from "../stores/auth";

export const EXPANDED_KEY = "terrarium.tree.expanded";

/** Above this many projects, the tree shows a filter input. */
export const FILTER_THRESHOLD = 8;

/**
 * The name of the Projects root in the open set. A project name has no `/`
 * (a-z, 0-9, `-`, `_`), so this name cannot be a project.
 */
export const PROJECTS_ROOT = "/projects";
/** Reserved node name, distinct from a project named `bookmarks`. */
export const BOOKMARKS_KEY = "/bookmarks";

/**
 * The root is open by default. The open set holds this marker while the root
 * is closed, so a list from before the root keeps the projects in view.
 */
export const ROOT_CLOSED = "!/projects";

/**
 * The name of the Agents root in the open set before the root was removed.
 * A stored list can still hold it, so prune drops it.
 */
export const OLD_AGENTS_ROOT = "/agents";

/** The agent ID (or `project/name`) in an /agents/<id> path, or null. */
export function agentFromPath(pathname: string): string | null {
  const m = /^\/agents\/([^/]+)/.exec(pathname);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]!);
  } catch {
    return m[1]!;
  }
}

/** The project in a /p/<name>/... path, or null. */
export function projectFromPath(pathname: string): string | null {
  const m = /^\/p\/([^/]+)/.exec(pathname);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]!);
  } catch {
    return m[1]!;
  }
}

/** The stored list: an array of strings, or empty for anything else. */
export function parseExpanded(raw: string | null): string[] {
  if (!raw) return [];
  try {
    const v: unknown = JSON.parse(raw);
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

/** Add `name` to the open set (no duplicates). */
export function expand(list: readonly string[], name: string): string[] {
  return list.includes(name) ? [...list] : [...list, name];
}

/** Remove `name` from the open set. */
export function collapse(list: readonly string[], name: string): string[] {
  return list.filter((n) => n !== name);
}

/** Open `name` if it is closed, close it if it is open. */
export function toggle(list: readonly string[], name: string): string[] {
  return list.includes(name) ? collapse(list, name) : expand(list, name);
}

/**
 * Drop the old project, project Agents, and project Repos open names. Only
 * the root markers, Bookmarks, and folders remain in the open set. A folder
 * stays while `folders` is null (not loaded, or no access), else while its
 * ID is in `folders`.
 */
export function prune(list: readonly string[], folders: readonly string[] | null = null): string[] {
  return list.filter(
    (n) =>
      n === ROOT_CLOSED ||
      n === BOOKMARKS_KEY ||
      (n.startsWith("folder:") && (folders === null || folders.includes(n.slice(7)))),
  );
}

/** True for the pages under the Projects root: the cards (/projects) and every /p/<name>/... page. */
export function inProjects(pathname: string): boolean {
  return /^\/projects\/?$/.test(pathname) || projectFromPath(pathname) !== null;
}

/** The projects whose name holds the query (case-insensitive). An empty query keeps all. */
export function filterProjects<T extends { name: string }>(projects: readonly T[], query: string): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...projects];
  return projects.filter((p) => p.name.toLowerCase().includes(q));
}

/**
 * The open set, persisted. `follow(path)` opens the Projects root on a
 * project page or the page of an agent whose project is loaded.
 */
export function createExpandedStore(storage: KeyValueStorage | null = null) {
  const read = () => {
    try {
      return parseExpanded(storage?.getItem(EXPANDED_KEY) ?? null);
    } catch {
      return [];
    }
  };
  const [list, setList] = createSignal<string[]>(read());
  const write = (next: string[]) => {
    setList(next);
    try {
      storage?.setItem(EXPANDED_KEY, JSON.stringify(next));
    } catch {
      // ignore
    }
  };
  let lastFollowed: string | null = null;

  const isOpen = (name: string) => (name === PROJECTS_ROOT ? !list().includes(ROOT_CLOSED) : list().includes(name));
  const open = (name: string) => {
    if (name === PROJECTS_ROOT) {
      if (list().includes(ROOT_CLOSED)) write(collapse(list(), ROOT_CLOSED));
    } else if (!list().includes(name)) write(expand(list(), name));
  };
  const close = (name: string) => {
    if (name === PROJECTS_ROOT) {
      if (!list().includes(ROOT_CLOSED)) write(expand(list(), ROOT_CLOSED));
    } else if (list().includes(name)) write(collapse(list(), name));
  };

  return {
    list,
    isOpen,
    toggle: (name: string) => (isOpen(name) ? close(name) : open(name)),
    open,
    close,
    /**
     * Open the Projects root when the URL moves to the project cards, a
     * project, or a different agent. `projectOfAgent` gives the project of
     * an agent ID or `project/name` (undefined while agents are not loaded).
     */
    follow: (pathname: string, projectOfAgent: (id: string) => string | undefined = () => undefined) => {
      const agent = agentFromPath(pathname);
      let at: string | null;
      if (agent) {
        const p = projectOfAgent(agent);
        // Wait for the agents: then the project of the agent is known.
        if (!p) return;
        at = `${p}\n${agent}`;
      } else {
        at = projectFromPath(pathname) ?? (inProjects(pathname) ? PROJECTS_ROOT : null);
      }
      if (at === lastFollowed) return;
      lastFollowed = at;
      if (at) open(PROJECTS_ROOT);
    },
    prune: (folders: readonly string[] | null = null) => {
      const next = prune(list(), folders);
      if (next.length !== list().length) write(next);
    },
  };
}

export type ExpandedStore = ReturnType<typeof createExpandedStore>;

// ----- the project tree rows -----

/** The `data-key` of a project row. */
export const projectKey = (name: string): string => `p:${name}`;

/** The rows of the project tree in screen order, for the roving tabindex. */
export function projectTreeKeys(t: { rootKey: string; rootOpen: boolean; projects: readonly string[] }): string[] {
  return t.rootOpen ? [t.rootKey, ...t.projects.map(projectKey)] : [t.rootKey];
}

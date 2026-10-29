// The sidebar trees (Projects and Agents): which nodes are open, the filters,
// and the order of the agents. Pure, so tests can read it.

import { createSignal } from "solid-js";
import type { Agent } from "../api/types";
import type { KeyValueStorage } from "../stores/auth";

export const EXPANDED_KEY = "terrarium.tree.expanded";

/** Above this many projects (or agents), the tree shows a filter input. */
export const FILTER_THRESHOLD = 8;

/**
 * The name of the Projects root in the open set. A project name has no `/`
 * (a-z, 0-9, `-`, `_`), so this name cannot be a project.
 */
export const PROJECTS_ROOT = "/projects";

/**
 * The root is open by default. The open set holds this marker while the root
 * is closed, so a list from before the root keeps the projects in view.
 */
export const ROOT_CLOSED = "!/projects";

/**
 * The name of the Agents root in the open set. It has a `/`, so it cannot be
 * a project. The root is closed by default: the agent list grows with the
 * fleet and pushes the other global pages down. The open set holds this name
 * while the root is open.
 */
export const AGENTS_ROOT = "/agents";

/**
 * The name of the Agents node of a project in the open set: `<project>/agents`.
 * A project name has no `/`, so this name cannot be a project or a root. The
 * node is closed by default.
 */
export const projectAgentsNode = (project: string): string => `${project}/agents`;

/** The project of a project Agents node name, or null for any other name. */
export function projectOfAgentsNode(name: string): string | null {
  const m = /^([^/]+)\/agents$/.exec(name);
  return m ? m[1]! : null;
}

/**
 * The project whose Agents node a URL opens: /p/<project>/agents, or the
 * page of an agent in that project. `projectOfAgent` gives the project of
 * an agent ID or `project/name` (undefined while the agents are not loaded).
 */
export function projectAgentsFromPath(pathname: string, projectOfAgent: (id: string) => string | undefined): string | null {
  const p = projectFromPath(pathname);
  if (p) return /^\/p\/[^/]+\/agents\/?$/.test(pathname) ? p : null;
  const id = agentFromPath(pathname);
  return id ? (projectOfAgent(id) ?? null) : null;
}

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

/** True for the pages under the Agents root: the list (/agents) and every /agents/<id> page. */
export function inAgents(pathname: string): boolean {
  return /^\/agents\/?$/.test(pathname) || agentFromPath(pathname) !== null;
}

/** The agents in sidebar order: by project, then by name. */
export function sortAgents<T extends Pick<Agent, "project" | "name">>(agents: readonly T[]): T[] {
  const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
  return [...agents].sort((a, b) => cmp(a.project, b.project) || cmp(a.name, b.name));
}

/** The agents whose `project/name` holds the query (case-insensitive). An empty query keeps all. */
export function filterAgents<T extends Pick<Agent, "project" | "name">>(agents: readonly T[], query: string): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...agents];
  return agents.filter((a) => `${a.project}/${a.name}`.toLowerCase().includes(q));
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

/** Drop the names of projects that are gone, and their Agents nodes. An empty `known` list (not loaded yet) keeps all. The root markers stay. */
export function prune(list: readonly string[], known: readonly string[]): string[] {
  if (known.length === 0) return [...list];
  return list.filter((n) => n === ROOT_CLOSED || n === AGENTS_ROOT || known.includes(n) || known.includes(projectOfAgentsNode(n) ?? ""));
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
 * The open set, persisted. `follow(path)` opens the project of a new URL, so
 * the project you go to is always open; you can close it again after that.
 * The name PROJECTS_ROOT is the Projects root: it is open while ROOT_CLOSED
 * is not in the list. The name AGENTS_ROOT is the Agents root: it is open
 * while it is in the list.
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
  let lastAgent: string | null = null;
  let lastProjectAgents: string | null = null;

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
     * Open the Projects root and the project of `pathname` when the URL moves
     * into the project pages or to a different project. Open the Agents root
     * when the URL moves into the agent pages or to a different agent.
     */
    follow: (pathname: string) => {
      const agent = agentFromPath(pathname) ?? (inAgents(pathname) ? AGENTS_ROOT : null);
      if (agent !== lastAgent) {
        lastAgent = agent;
        if (agent) open(AGENTS_ROOT);
      }
      const p = projectFromPath(pathname);
      const at = p ?? (inProjects(pathname) ? PROJECTS_ROOT : null);
      if (at === lastFollowed) return;
      lastFollowed = at;
      if (!at) return;
      open(PROJECTS_ROOT);
      if (p) open(p);
    },
    /**
     * Open the Agents node of `project` when the URL moves to the agent list
     * of the project or to the page of one of its agents
     * (projectAgentsFromPath). Null changes nothing.
     */
    followProjectAgents: (project: string | null) => {
      if (project === lastProjectAgents) return;
      lastProjectAgents = project;
      if (project) open(projectAgentsNode(project));
    },
    prune: (known: readonly string[]) => {
      const next = prune(list(), known);
      if (next.length !== list().length) write(next);
    },
  };
}

export type ExpandedStore = ReturnType<typeof createExpandedStore>;

// ----- the project tree rows -----

/** The `data-key` of a project row. */
export const projectKey = (name: string): string => `p:${name}`;
/** The `data-key` of a page row under a project (PROJECT_ITEMS). */
export const itemKey = (name: string, path: string): string => `i:${name}/${path}`;
/** The `data-key` of an agent row under the Agents node of a project. */
export const projectAgentKey = (id: string): string => `pa:${id}`;

/**
 * The rows of the project tree in screen order, for the roving tabindex.
 * `items` are the page paths of a project. The `agents` item is a node: open,
 * it shows `agentsOf(project)` (agent IDs, sorted and filtered).
 */
export function projectTreeKeys(t: {
  rootKey: string;
  rootOpen: boolean;
  projects: readonly string[];
  items: readonly string[];
  isOpen: (name: string) => boolean;
  agentsOf: (project: string) => readonly string[];
}): string[] {
  if (!t.rootOpen) return [t.rootKey];
  const rows = (p: string) =>
    t.items.flatMap((path) => [itemKey(p, path), ...(path === "agents" && t.isOpen(projectAgentsNode(p)) ? t.agentsOf(p).map(projectAgentKey) : [])]);
  return [t.rootKey, ...t.projects.flatMap((p) => [projectKey(p), ...(t.isOpen(p) ? rows(p) : [])])];
}

// The counts on the project tabs (TER-154): Agents shows the agents blocked or
// in error, Board the blocked cards, and Dashboard the open questions of the
// project. The counts come from `GET /api/projects/{name}/counts`, read again
// (debounced) on each `card` or `question` event of the project, and from the
// agents store. Each count is in the accessible name of its tab, not only in
// a colour.

import { createSignal, onCleanup, type Accessor } from "solid-js";
import type { Api } from "../../api/client";
import type { Agent, AgentEvent, ProjectCounts } from "../../api/types";

/** The badge of one tab: the number, its classes, and the words for the accessible name and the tooltip. */
export interface TabBadge {
  n: number;
  cls: string;
  text: string;
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The agents of a project that are blocked or in error, as the project row of the sidebar counts them. */
export function blockedAgents(agents: Pick<Agent, "status">[]): number {
  return agents.filter((a) => a.status === "blocked" || a.status === "error").length;
}

/** The badge of the tab with `path`, or null when it has none or its count is 0. Pure. */
export function tabBadge(path: string, blockedAgentCount: number, counts: ProjectCounts | null): TabBadge | null {
  if (path === "agents" && blockedAgentCount > 0) {
    return { n: blockedAgentCount, cls: "nav-count warn", text: plural(blockedAgentCount, "agent blocked or in error", "agents blocked or in error") };
  }
  if (path === "board" && counts && counts.blocked_cards > 0) {
    return { n: counts.blocked_cards, cls: "nav-count warn", text: plural(counts.blocked_cards, "blocked card", "blocked cards") };
  }
  const q = counts?.questions;
  if (path === "dashboard" && q && q.open > 0) {
    const more = q.critical > 0 ? `, ${q.critical} critical` : q.urgent > 0 ? `, ${q.urgent} urgent` : "";
    return { n: q.open, cls: q.urgent > 0 ? "nav-count warn" : "nav-count info", text: `${plural(q.open, "open question", "open questions")}${more}` };
  }
  return null;
}

/** Does `e` change the counts of `project`? A card or a question event of it. */
export const changesCounts = (e: Pick<AgentEvent, "kind" | "project">, project: string): boolean =>
  (e.kind === "card" || e.kind === "question") && e.project === project;

/**
 * The counts of the project that `project()` names. It reads them at once,
 * again when the name changes, and again `debounceMs` after the last card or
 * question event of the project. A failed read keeps the last value of the
 * same project; an older read that resolves later is dropped.
 * Call it inside a component or a root: it cleans up with the owner.
 */
export function createProjectCounts(
  api: Pick<Api, "projects">,
  events: { on(fn: (e: AgentEvent) => void): () => void },
  project: Accessor<string>,
  opts: { debounceMs?: number } = {},
): { counts: Accessor<ProjectCounts | null>; refresh: () => Promise<void> } {
  const [counts, setCounts] = createSignal<ProjectCounts | null>(null);
  let generation = 0;
  let current = "";
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refresh = async () => {
    const name = project();
    const gen = ++generation;
    if (name !== current) {
      current = name;
      setCounts(null);
    }
    try {
      const c = await api.projects.counts(name);
      if (gen === generation) setCounts(c);
    } catch {
      // A failed read keeps the last value.
    }
  };

  const off = events.on((e) => {
    if (!changesCounts(e, project())) return;
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), opts.debounceMs ?? 300);
  });
  onCleanup(() => {
    off();
    clearTimeout(timer);
    generation++;
  });
  return { counts, refresh };
}

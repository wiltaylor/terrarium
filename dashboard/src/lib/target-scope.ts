// Target scope (docs/api.md §17.2): a target is global, or one project owns
// it. The daemon sends `scope`; a daemon from before the field sends only
// `project`, so the client derives `scope` from it. Pure.

import type { Target, TargetScope } from "../api/types";

/** `scope` from the daemon, else `project` when the field is absent: no project is global. */
export const targetScope = (t: Pick<Target, "scope" | "project">): TargetScope => t.scope ?? (t.project ? "project" : "global");

/** The target with `scope` set (the client sets it on every target it reads). */
export const withScope = <T extends Pick<Target, "scope" | "project">>(t: T): T & { scope: TargetScope } => ({ ...t, scope: targetScope(t) });

/** The scope label: the project name, or "global". */
export const scopeLabel = (t: Pick<Target, "scope" | "project">): string => (targetScope(t) === "global" ? "global" : (t.project ?? "project"));

/** The scope filter of the Targets page in a project view. */
export type ScopeFilter = "all" | "project" | "global";

export const SCOPE_FILTERS: readonly { id: ScopeFilter; label: string }[] = [
  { id: "all", label: "All" },
  { id: "project", label: "Project only" },
  { id: "global", label: "Global only" },
];

/** The targets that the filter keeps. "project" keeps the targets of `project` (any project when it is absent). */
export function filterByScope<T extends Pick<Target, "scope" | "project">>(targets: T[], filter: ScopeFilter, project?: string | null): T[] {
  if (filter === "all") return targets;
  if (filter === "global") return targets.filter((t) => targetScope(t) === "global");
  return targets.filter((t) => targetScope(t) === "project" && (!project || t.project === project));
}

export interface ScopeGroup<T> {
  /** `global`, or the project name. */
  key: string;
  label: string;
  targets: T[];
}

/** The targets grouped by scope: the global group first, then one group per project by name. Each group is by target name. */
export function groupByScope<T extends Pick<Target, "scope" | "project" | "name">>(targets: T[]): ScopeGroup<T>[] {
  const groups = new Map<string, T[]>();
  for (const t of targets) {
    const key = targetScope(t) === "global" ? "" : (t.project ?? "");
    groups.set(key, [...(groups.get(key) ?? []), t]);
  }
  return [...groups.entries()]
    .sort(([a], [b]) => (a === "" ? -1 : b === "" ? 1 : a.localeCompare(b)))
    .map(([key, list]) => ({
      key: key || "global",
      label: key ? `Project ${key}` : "Global",
      targets: list.slice().sort((a, b) => a.name.localeCompare(b.name)),
    }));
}

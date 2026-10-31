// The health of a project: one colour for all its agents, for the dot before
// the project name (sidebar tree, project cards). Pure.
//
// Red is first: an agent is in error or blocked. Then yellow: an agent works,
// starts, restarts, or compacts. Then green: all agents are at rest (idle,
// waiting, stopped, paused). A project with no agents has no colour.

import type { Agent } from "../api/types";

export type ProjectHealthLevel = "red" | "yellow" | "green" | "none";

/** The words of the counts, in the order the label gives them. */
const ORDER = ["error", "blocked", "compacting", "working", "starting", "restarting", "waiting", "idle", "stopped", "paused"] as const;
type Bucket = (typeof ORDER)[number];

export interface ProjectHealth {
  level: ProjectHealthLevel;
  /** The number of agents in each state. A state with no agents is absent. */
  counts: Partial<Record<Bucket, number>>;
  /** The agents of the project. */
  total: number;
}

type HealthAgent = Pick<Agent, "status" | "status_reason" | "paused">;

/** The state of one agent as the dot counts it. Error and blocked win over the pause. */
function bucket(a: HealthAgent): Bucket {
  if (a.status === "error" || a.status === "blocked") return a.status;
  if (a.paused) return "paused";
  if (a.status_reason === "compacting") return "compacting";
  return a.status;
}

const LEVEL: Record<Bucket, ProjectHealthLevel> = {
  error: "red",
  blocked: "red",
  compacting: "yellow",
  working: "yellow",
  starting: "yellow",
  restarting: "yellow",
  waiting: "green",
  idle: "green",
  stopped: "green",
  paused: "green",
};
const RANK: Record<ProjectHealthLevel, number> = {
  none: 0,
  green: 1,
  yellow: 2,
  red: 3,
};

export function projectHealth(agents: readonly HealthAgent[]): ProjectHealth {
  const counts: ProjectHealth["counts"] = {};
  let level: ProjectHealthLevel = "none";
  for (const a of agents) {
    const b = bucket(a);
    counts[b] = (counts[b] ?? 0) + 1;
    if (RANK[LEVEL[b]] > RANK[level]) level = LEVEL[b];
  }
  return { level, counts, total: agents.length };
}

/** "terrarium: 1 blocked, 2 working, 3 idle", or "terrarium: no agents". */
export function projectHealthLabel(project: string, h: ProjectHealth): string {
  const parts = ORDER.filter((b) => h.counts[b]).map((b) => `${h.counts[b]} ${b}`);
  return `${project}: ${parts.length ? parts.join(", ") : "no agents"}`;
}

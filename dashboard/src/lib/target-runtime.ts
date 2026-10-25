// Mode 1 on targets (docs/api.md §17.2, §17.3, §17.8): the lab source of a
// vmlab target, the project prefix of a target name, the workspaces that a
// target has attached, the lab instances of a `lab_from_workspace` target,
// the two caps, and the running count of a host. Pure.

import { ApiError } from "../api/http";
import type { Host, LabInstance, ProjectCapDetails, Target, TargetState } from "../api/types";

/**
 * The name that `POST /api/targets` gives a project target: `<project>-<name>`,
 * unless `name` starts with `<project>-` already. A global target keeps its name.
 */
export function fullTargetName(name: string, project: string | null | undefined): string {
  const n = name.trim();
  if (!project || !n) return n;
  const p = `${project}-`;
  return n.startsWith(p) ? n : `${p}${n}`;
}

/** The three lab sources of a vmlab target (docs/api.md §17.2). */
export type LabSource = "dir" | "repo" | "workspace";

/** The lab source of a vmlab target, or null for another kind (or a vmlab target with none). */
export function labSource(t: Pick<Target, "kind" | "lab" | "lab_repo" | "lab_from_workspace">): LabSource | null {
  if (t.kind !== "vmlab") return null;
  if (t.lab_from_workspace) return "workspace";
  if (t.lab_repo) return "repo";
  if (t.lab) return "dir";
  return null;
}

/**
 * One line for the source of a vmlab target: the lab dir, `repo@ref:path`,
 * or `workspace:path`. A default ref or path shows as `default` and `/`.
 */
export function sourceLabel(t: Pick<Target, "kind" | "lab" | "lab_repo" | "lab_ref" | "lab_path" | "lab_from_workspace">): string | null {
  switch (labSource(t)) {
    case "dir":
      return t.lab!;
    case "repo":
      return `${t.lab_repo}@${t.lab_ref || "default"}:${t.lab_path || "/"}`;
    case "workspace":
      return `workspace:${t.lab_path || "/"}`;
    default:
      return null;
  }
}

/** The path of an attached workspace (`<project>/<agent>`) in the target: `/work/<project>/<agent>`. */
export const workspacePath = (attached: string): string => `/work/${attached}`;

/** One instance of a `lab_from_workspace` target, with the holder's ULID. */
export interface InstanceRow extends LabInstance {
  agent: string;
}

/** The instances of a `lab_from_workspace` target, by holder name. */
export const instanceRows = (t: Pick<Target, "runtime">): InstanceRow[] =>
  Object.entries(t.runtime?.instances ?? {})
    .map(([agent, i]) => ({ ...i, agent }))
    .sort((a, b) => a.agent_name.localeCompare(b.agent_name));

/** The verbs of one lab instance: `up` when it does not run, `down` when it runs, and `status`. */
export function instanceActions(state: TargetState | undefined): ("up" | "down" | "status")[] {
  return [state === "running" ? "down" : "up", "status"];
}

/** A cap refusal of `POST /api/targets` or of an `up` (docs/api.md §17.2, §17.3). */
export type CapError =
  /** `409 conflict`: the project has `TERRARIUM_MAX_TARGETS_PER_PROJECT` targets. */
  | ({ cap: "project"; message: string } & ProjectCapDetails)
  /** `429 rate_limited`: the host runs `max_running_targets` targets. */
  | { cap: "host"; message: string; host: string | null; running: number | null; max: number | null };

/** The cap behind an error, or null for any other error. */
export function capError(e: unknown): CapError | null {
  if (!(e instanceof ApiError)) return null;
  if (e.status === 409) {
    const d = e.details as Partial<ProjectCapDetails> | undefined;
    if (d && typeof d.project === "string" && typeof d.max === "number") return { cap: "project", message: e.message, project: d.project, targets: d.targets ?? d.max, max: d.max };
    return null;
  }
  if (e.status === 429) {
    // "host `lab1` runs 4 targets, and its max_running_targets is 4; bring one down first"
    const m = /host `([^`]+)` runs (\d+) targets?, and its max_running_targets is (\d+)/.exec(e.message);
    return { cap: "host", message: e.message, host: m?.[1] ?? null, running: m ? Number(m[2]) : null, max: m ? Number(m[3]) : null };
  }
  return null;
}

/** One sentence for a cap refusal. */
export function capSentence(c: CapError): string {
  if (c.cap === "project") return `Project ${c.project} has ${c.targets} targets, the most it may have (${c.max}). Delete one first.`;
  const who = c.host ? `Host ${c.host}` : "The host";
  const n = c.running !== null && c.max !== null ? ` runs ${c.running} of its ${c.max} targets` : " runs its max_running_targets";
  return `${who}${n}. Bring one down first.`;
}

/**
 * The docker and vmlab targets (and lab instances) that run on a host, as
 * the daemon counts them for `max_running_targets` (docs/api.md §17.3).
 */
export function runningOnHost(targets: Pick<Target, "kind" | "host" | "state" | "lab_from_workspace" | "runtime">[], host: string): number {
  let n = 0;
  for (const t of targets) {
    if (t.host !== host || (t.kind !== "docker" && t.kind !== "vmlab")) continue;
    if (t.lab_from_workspace) n += Object.values(t.runtime?.instances ?? {}).filter((i) => i.state === "running").length;
    else if (t.state === "running") n += 1;
  }
  return n;
}

/**
 * "3 / 4" for the running targets of a host. Without `max_running_targets`
 * (an offline host, or an older host agent: no cap), "3 (no cap reported)".
 */
export const runningLabel = (running: number, h: Pick<Host, "max_running_targets">): string =>
  h.max_running_targets === undefined ? `${running} (no cap reported)` : `${running} / ${h.max_running_targets}`;

/** One holder of a `lab_from_workspace` target: its instance, or none yet. */
export interface HolderRow {
  agent: string;
  agent_name: string | null;
  instance: LabInstance | null;
}

/**
 * The holders of a `lab_from_workspace` target: each instance of
 * `runtime.instances`, and each lease holder (`holderIds`, ULIDs) that has
 * no instance yet. `nameOf` names a holder from the agents store.
 */
export function holderRows(t: Pick<Target, "runtime">, holderIds: string[], nameOf: (id: string) => string | undefined = () => undefined): HolderRow[] {
  const out: HolderRow[] = instanceRows(t).map((i) => ({ agent: i.agent, agent_name: i.agent_name, instance: i }));
  for (const id of holderIds) if (!out.some((r) => r.agent === id)) out.push({ agent: id, agent_name: nameOf(id) ?? null, instance: null });
  return out;
}

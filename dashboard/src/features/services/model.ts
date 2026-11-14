// Services in the dashboard (TER-1269, docs/api.md §17.12): pure helpers
// for the Services page. No Solid imports.

import type { Service, ServiceRequest, ServiceRequestState, ServiceResources } from "../../api/types";

/** Every request state, in the order of the flow. */
export const REQUEST_STATES: readonly ServiceRequestState[] = ["queued", "waiting", "running", "done", "failed", "cancelled", "timed_out"];

/** A request in one of these states has not ended: it may be cancelled. */
export const isOpen = (s: ServiceRequestState): boolean => s === "queued" || s === "waiting" || s === "running";

/** The chip class of a state. */
export function stateTone(s: ServiceRequestState): "info" | "ok" | "warn" | "bad" | "" {
  switch (s) {
    case "running":
      return "info";
    case "done":
      return "ok";
    case "queued":
    case "waiting":
      return "warn";
    case "failed":
    case "timed_out":
      return "bad";
    default:
      return "";
  }
}

/** The label of a state: `timed_out` as "timed out". */
export const stateLabel = (s: ServiceRequestState): string => s.replace(/_/g, " ");

/** The number of requests in each state, in flow order, without the states that have none. */
export function stateCounts(rows: Pick<ServiceRequest, "state">[]): { state: ServiceRequestState; count: number }[] {
  return REQUEST_STATES.map((state) => ({ state, count: rows.filter((r) => r.state === state).length })).filter((c) => c.count > 0);
}

/**
 * The note before the state counts. The counts come from the loaded rows:
 * when the load hit its limit, they are of the newest rows only, and the
 * note says so (rev-1269 finding 2). The API has no count by state.
 */
export const countsNote = (loaded: number, limit: number): string => (loaded >= limit ? `of the newest ${limit}:` : "");

/** The resource claim in words, or "none". */
export function claimText(r: ServiceResources | undefined): string {
  if (!r) return "none";
  const parts: string[] = [];
  if (r.cpu_millis) parts.push(`${r.cpu_millis % 1000 === 0 ? r.cpu_millis / 1000 : (r.cpu_millis / 1000).toFixed(1)} CPU`);
  if (r.memory) parts.push(`${r.memory} memory`);
  if (r.gpus) {
    const what = `${r.gpus} GPU${r.gpus === 1 ? "" : "s"}`;
    parts.push(r.gpu_exclusive ? `${what}, exclusive` : r.gpu_vram ? `${what}, ${r.gpu_vram} VRAM each` : what);
  }
  return parts.length ? parts.join(", ") : "none";
}

/** What runs a request: the command of a script service, the template of an agent service. */
export const runsText = (s: Pick<Service, "kind" | "command" | "template">): string =>
  s.kind === "script" ? (s.command ?? []).join(" ") : `template ${s.template ?? "?"}`;

/** Who may call it, in words. Empty: the service's own project, or only its admins for a global service. */
export const callersText = (s: Pick<Service, "callers" | "project">): string =>
  s.callers?.length ? s.callers.join(", ") : s.project ? `the agents of project ${s.project}` : "its admins only";

/** The project filter value of a service: its project, or `global`. */
export const scopeKey = (s: Pick<Service, "project">): string => s.project ?? "global";

/** The project filter options: `global` first when present, then the projects by name. */
export function scopeOptions(services: Pick<Service, "project">[]): string[] {
  const keys = [...new Set(services.map(scopeKey))];
  return keys.sort((a, b) => (a === "global" ? -1 : b === "global" ? 1 : a.localeCompare(b)));
}

/** The services that match the filter (empty: all), by name. */
export const filterServices = <S extends Pick<Service, "name" | "project">>(services: S[], scope: string): S[] =>
  services.filter((s) => !scope || scopeKey(s) === scope).sort((a, b) => a.name.localeCompare(b.name));

/** A JSON value as indented text; empty for null and undefined. */
export const jsonText = (v: unknown): string => (v === null || v === undefined ? "" : JSON.stringify(v, null, 2));

/** The outcome of a request in one short line: the exit code and the error, the queue place, or nothing. */
export function outcomeText(r: Pick<ServiceRequest, "state" | "exit_code" | "error" | "position">): string {
  if (r.state === "queued" && r.position) return `#${r.position} in the queue`;
  const parts: string[] = [];
  if (r.exit_code !== null && r.exit_code !== undefined) parts.push(`exit ${r.exit_code}`);
  if (r.error) parts.push(r.error);
  return parts.join(": ");
}

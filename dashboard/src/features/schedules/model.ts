// The schedule list of a view and its live updates from `schedule` events
// (docs/api.md §19): `{change, schedule, run?, status?}`. `schedule` may be
// the whole schedule or only its ID; `run` may be the run or its ID. When
// the event does not carry enough to update the row, the view refetches.
// Pure; no Solid imports.

import type { AgentEvent, RunStatus, Schedule, ScheduleRun } from "../../api/types";
import type { ScopeFilter } from "../../lib/target-scope";

/** The view: one project (its schedules and the global ones), or every schedule (`null`). */
export interface ScheduleView {
  project: string | null;
}

export interface ScheduleListState {
  items: Schedule[];
  /** Schedule ID to the run that goes on now (`?` when the run ID is not known). */
  running: Record<string, string>;
}

export const scheduleScope = (s: Pick<Schedule, "project">): "global" | "project" => (s.project ? "project" : "global");
export const scheduleScopeLabel = (s: Pick<Schedule, "project">): string => s.project ?? "global";

/** Does the schedule belong in the view? */
export const inView = (s: Pick<Schedule, "project">, v: ScheduleView): boolean => v.project === null || s.project === null || s.project === v.project;

/** The schedules that the scope filter of a project view keeps. */
export function filterSchedules<T extends Pick<Schedule, "project">>(list: T[], filter: ScopeFilter): T[] {
  if (filter === "all") return list;
  return list.filter((s) => scheduleScope(s) === filter);
}

export interface ScheduleGroup {
  key: string;
  label: string;
  items: Schedule[];
}

/** Every schedule by scope: Global first, then one group per project by name. */
export function groupSchedules(list: Schedule[]): ScheduleGroup[] {
  const groups = new Map<string, Schedule[]>();
  for (const s of list) groups.set(s.project ?? "", [...(groups.get(s.project ?? "") ?? []), s]);
  return [...groups.entries()]
    .sort(([a], [b]) => (a === "" ? -1 : b === "" ? 1 : a.localeCompare(b)))
    .map(([key, items]) => ({ key: key || "global", label: key ? `Project ${key}` : "Global", items: sortSchedules(items) }));
}

export const sortSchedules = (list: Schedule[]): Schedule[] => list.slice().sort((a, b) => a.name.localeCompare(b.name) || (a.project ?? "").localeCompare(b.project ?? ""));

/** Put a schedule in (or out, when it left the view) of the list. */
export function upsertSchedule(items: Schedule[], s: Schedule, v: ScheduleView): Schedule[] {
  const rest = items.filter((x) => x.id !== s.id);
  return inView(s, v) ? sortSchedules([...rest, s]) : rest;
}

const isSchedule = (v: unknown): v is Schedule => typeof v === "object" && v !== null && typeof (v as Schedule).id === "string" && typeof (v as Schedule).name === "string" && "spec" in v;
const isRun = (v: unknown): v is ScheduleRun => typeof v === "object" && v !== null && typeof (v as ScheduleRun).id === "string" && typeof (v as ScheduleRun).started_at === "string";

/** The schedule ID of a `schedule` event, the schedule when the event has it, and the run. */
export function readScheduleEvent(e: AgentEvent): { change: string; id: string | null; schedule: Schedule | null; run: ScheduleRun | null; runId: string | null; status: string | null } | null {
  if (e.kind !== "schedule") return null;
  const d = e.data ?? {};
  const schedule = isSchedule(d.schedule) ? d.schedule : null;
  const id = schedule?.id ?? (typeof d.schedule === "string" ? d.schedule : typeof d.schedule_id === "string" ? d.schedule_id : null);
  const run = isRun(d.run) ? d.run : null;
  const runId = run?.id ?? (typeof d.run === "string" ? d.run : typeof d.run_id === "string" ? d.run_id : null);
  const status = typeof d.status === "string" ? d.status : (run?.status ?? null);
  return { change: typeof d.change === "string" ? d.change : "", id, schedule, run, runId, status };
}

/**
 * Apply a `schedule` event to the list. `refetch` says that the event did
 * not carry enough to update the list, so the view must read it again.
 */
export function applyScheduleEvent(state: ScheduleListState, e: AgentEvent, v: ScheduleView): { state: ScheduleListState; refetch: boolean } {
  const ev = readScheduleEvent(e);
  if (!ev) return { state, refetch: false };
  if (!ev.id) return { state, refetch: true };
  const id = ev.id;
  const running = { ...state.running };
  let items = state.items;
  switch (ev.change) {
    case "deleted":
      delete running[id];
      return { state: { items: items.filter((s) => s.id !== id), running }, refetch: false };
    case "created":
    case "updated":
    case "enabled":
    case "disabled":
      if (!ev.schedule) return { state, refetch: true };
      return { state: { items: upsertSchedule(items, ev.schedule, v), running }, refetch: false };
    case "run_started":
      running[id] = ev.runId ?? "?";
      if (ev.schedule) items = upsertSchedule(items, ev.schedule, v);
      return { state: { items, running }, refetch: false };
    case "run_finished": {
      delete running[id];
      if (ev.schedule) return { state: { items: upsertSchedule(items, ev.schedule, v), running }, refetch: false };
      const cur = items.find((s) => s.id === id);
      if (!cur || !ev.run) return { state: { items, running }, refetch: true };
      const status = (ev.status ?? ev.run.status) as RunStatus;
      const next: Schedule = {
        ...cur,
        runs_count: cur.runs_count + (cur.last_run?.run_id === ev.run.id ? 0 : 1),
        last_run: { at: ev.run.started_at, status, summary: ev.run.summary, run_id: ev.run.id },
      };
      // `next_run_at` may move after a run; only the daemon knows: refetch for it.
      return { state: { items: upsertSchedule(items, next, v), running }, refetch: true };
    }
    default:
      return { state, refetch: true };
  }
}

/**
 * Apply the answer of `POST .../run`. The daemon answers with the run when
 * it ends: then the row stops its spinner and shows the run as the last
 * run. A `running` answer (a daemon that answers at once) keeps the spinner.
 */
export function applyRunAnswer(state: ScheduleListState, r: ScheduleRun, v: ScheduleView): ScheduleListState {
  const running = { ...state.running };
  if (r.status === "running") {
    running[r.schedule] = r.id;
    return { items: state.items, running };
  }
  delete running[r.schedule];
  const cur = state.items.find((s) => s.id === r.schedule);
  if (!cur) return { items: state.items, running };
  const next: Schedule = {
    ...cur,
    runs_count: cur.last_run?.run_id === r.id ? cur.runs_count : cur.runs_count + 1,
    last_run: { at: r.finished_at ?? r.started_at, status: r.status, summary: r.summary, run_id: r.id },
  };
  return { items: upsertSchedule(state.items, next, v), running };
}

/** Does the event start or end a run of this schedule? */
export const touchesRuns = (e: AgentEvent, scheduleId: string): boolean => {
  const ev = readScheduleEvent(e);
  return !!ev && ev.id === scheduleId && (ev.change === "run_started" || ev.change === "run_finished");
};

/** The run in the event, for an open run history. */
export const eventRun = (e: AgentEvent, scheduleId: string): ScheduleRun | null => {
  const ev = readScheduleEvent(e);
  return ev && ev.id === scheduleId && ev.run ? ev.run : null;
};

/** The chip class of a run status. */
export const RUN_CHIP: Record<string, string> = { ok: "ok", failed: "bad", skipped: "warn", running: "info" };

/**
 * The output of a run as text. The daemon sends a JSON value: an exec run
 * has `stdout` and `stderr` (the tails); other actions have a small object
 * (the message ID, the delivery, the new status, the windows).
 */
export function runOutputText(output: unknown): string {
  if (output === null || output === undefined) return "";
  if (typeof output === "string") return output;
  if (typeof output !== "object") return String(output);
  const o = output as Record<string, unknown>;
  if (typeof o.stdout === "string" || typeof o.stderr === "string") {
    const parts: string[] = [];
    if (typeof o.cmd === "string" || Array.isArray(o.cmd)) parts.push(`$ ${Array.isArray(o.cmd) ? o.cmd.join(" ") : String(o.cmd)}${typeof o.exit_code === "number" ? `  (exit ${o.exit_code})` : ""}`);
    if (typeof o.stdout === "string" && o.stdout) parts.push(o.stdout.replace(/\n$/, ""));
    if (typeof o.stderr === "string" && o.stderr) parts.push(`--- stderr\n${o.stderr.replace(/\n$/, "")}`);
    return parts.join("\n");
  }
  return Object.keys(o).length ? JSON.stringify(o, null, 2) : "";
}

/** How long a run took, in seconds, or null while it runs. */
export const runSeconds = (r: Pick<ScheduleRun, "started_at" | "finished_at">): number | null =>
  r.finished_at ? Math.max(0, Math.round((Date.parse(r.finished_at) - Date.parse(r.started_at)) / 1000)) : null;

/** The schedules whose last run failed within `hours` before `now`: the attention items of Home. */
export function failedRecently(list: Schedule[], now = Date.now(), hours = 24): Schedule[] {
  const since = now - hours * 3_600_000;
  return list.filter((s) => s.last_run?.status === "failed" && Date.parse(s.last_run.at) >= since);
}

/** The page of a schedule, with its row open. */
export const scheduleHref = (s: Pick<Schedule, "id" | "project">): string =>
  s.project ? `/p/${encodeURIComponent(s.project)}/schedules?open=${encodeURIComponent(s.id)}` : `/schedules?open=${encodeURIComponent(s.id)}`;

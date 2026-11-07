// Project actions (TER-53, docs/api.md §19.10): a schedule with the spec
// `manual`. Its button is on the project dashboard and next to the project
// name in Home 'Work in progress'. A click asks for a confirm first, then
// runs it (`POST /api/schedules/{id}/run`, which answers when the run ends)
// and shows the result. Pure; no Solid imports.

import type { Schedule, ScheduleRun } from "../../api/types";
import { actionWords } from "../../lib/schedule";

/** Is it a project action? */
export const isAction = (s: Pick<Schedule, "spec">): boolean => s.spec.kind === "manual";

/** The actions of a project, by name. `enabledOnly`: Home shows only the ones that can run. */
export function projectActions(list: Schedule[], project: string, enabledOnly = false): Schedule[] {
  return list
    .filter((s) => isAction(s) && s.project === project && (!enabledOnly || s.enabled))
    .slice()
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** The actions by project, for Home: one list call for all projects. */
export function actionsByProject(list: Schedule[]): Map<string, Schedule[]> {
  const out = new Map<string, Schedule[]>();
  for (const s of list) {
    if (!isAction(s) || !s.project || !s.enabled) continue;
    out.set(s.project, [...(out.get(s.project) ?? []), s]);
  }
  for (const v of out.values()) v.sort((a, b) => a.name.localeCompare(b.name));
  return out;
}

/** Why the button is off, or null when it runs. A disabled action is `409` at the daemon. */
export const offReason = (s: Pick<Schedule, "enabled" | "disabled_reason">): string | null => (s.enabled ? null : s.disabled_reason ? `disabled: ${s.disabled_reason}` : "disabled");

/** Who the run acts as, in words. */
export function runsAs(s: Pick<Schedule, "owner" | "run_as" | "project">): string {
  if (s.owner === "project") return `project ${s.project ?? "?"} (its rules)`;
  if (s.owner === "system") return "the system";
  return s.run_as === "operator" ? "you (the operator)" : s.run_as;
}

/** The page of the action's runs: the Actions tab of the project's schedules, with the action open. */
export const historyHref = (s: Pick<Schedule, "id" | "project">): string => `/p/${encodeURIComponent(s.project ?? "")}/schedules?tab=actions&open=${encodeURIComponent(s.id)}`;

/** What the confirm step says about the run. */
export interface ConfirmText {
  /** One line: the action in words. */
  what: string;
  /** Who the run acts as. */
  as: string;
  /**
   * An exec: it runs on the target, how long it may take, and where the
   * result goes. `limitSecs` is the action's `timeout_secs`; absent, the
   * daemon's default applies, and the text names no number, so a change of
   * the default in the daemon does not make it wrong.
   */
  exec?: { target: string; cmd: string; limitSecs?: number };
}

export function confirmText(s: Schedule): ConfirmText {
  const t: ConfirmText = { what: actionWords(s.action, s.project), as: runsAs(s) };
  if (s.action.type === "exec") {
    const limit = s.action.timeout_secs;
    t.exec = { target: s.action.target, cmd: s.action.cmd.join(" "), ...(limit ? { limitSecs: limit } : {}) };
  }
  return t;
}

/** The limit of an exec in words: "5 min", "15 min", "1 h"; no limit given: the daemon's default. */
export function limitWords(secs?: number): string {
  if (!secs) return "the default exec limit";
  if (secs < 60) return `${secs} s`;
  if (secs < 3600) return `${Math.round(secs / 60)} min`;
  return `${Math.round(secs / 360) / 10} h`;
}

/** The result of a run, as the button shows it. */
export interface RunResult {
  tone: "ok" | "bad" | "warn" | "info";
  status: string;
  summary: string;
  error?: string;
}

const TONE: Record<string, RunResult["tone"]> = { ok: "ok", failed: "bad", skipped: "warn", running: "info" };

export function resultOf(r: Pick<ScheduleRun, "status" | "summary" | "error">): RunResult {
  return { tone: TONE[r.status] ?? "info", status: r.status, summary: r.summary, ...(r.error ? { error: r.error } : {}) };
}

/**
 * The result of a run call that threw. An API error (a status) is the
 * refusal: `403` rights, `409` disabled. No status: the request ended
 * before the run did (a network drop on a long exec); the run may still go
 * on, so the run history has the end.
 */
export function failureOf(e: unknown): RunResult {
  const err = e as { status?: number; message?: string } | null;
  if (err && typeof err.status === "number" && err.status > 0) return { tone: "bad", status: "refused", summary: err.message ?? `HTTP ${err.status}` };
  return { tone: "warn", status: "unknown", summary: "The request ended before the run did. The run may still go on: see the run history for its end." };
}

// The state of the schedule dialog (docs/api.md §19): the form, the cron
// helper (presets and the live "next 3 runs" preview), the body of `POST
// /api/schedules` or `PATCH /api/schedules/{id}`, and the answer. The scope
// is the selected project by default; "global" is an explicit choice that
// needs a sysadmin (`403`). A `422` names the field in `details.field`; the
// dialog shows it under that field. The client preview does not block a
// cron expression: the daemon decides, and its `422` shows when it
// disagrees. No JSX; the tests drive it with the mock API.

import { createSignal } from "solid-js";
import type { Api } from "../../api/client";
import { ApiError } from "../../api/http";
import type { Agent, AgentVerb, CatchUp, NewSchedule, Project, Schedule, ScheduleAction, ScheduleActionType, ScheduleSpec, ScheduleSpecKind, Target } from "../../api/types";
import { previewCron } from "../../lib/cron";
import { commandLine, parseEvery, splitCommand } from "../../lib/schedule";

export const GLOBAL = "global" as const;

export type CronPreset = "every5" | "every15" | "every60" | "hourly" | "daily" | "weekly" | "custom";

export const CRON_PRESETS: readonly { id: CronPreset; label: string }[] = [
  { id: "every5", label: "Every 5 min" },
  { id: "every15", label: "Every 15 min" },
  { id: "every60", label: "Every 60 min" },
  { id: "hourly", label: "Hourly at :MM" },
  { id: "daily", label: "Daily at HH:MM" },
  { id: "weekly", label: "Weekly on a day at HH:MM" },
  { id: "custom", label: "Custom expression" },
];

export const ACTION_TYPES: readonly { id: ScheduleActionType; label: string }[] = [
  { id: "mail", label: "Send mail" },
  { id: "turn", label: "Give an agent a turn" },
  { id: "exec", label: "Run a command on a target" },
  { id: "agent", label: "Start, stop, or restart an agent" },
  { id: "probe", label: "Probe a subscription" },
];

export interface ScheduleForm {
  name: string;
  /** A project name, `GLOBAL`, or "" (not chosen). */
  scope: string;
  enabled: boolean;
  catchUp: CatchUp;
  specKind: ScheduleSpecKind;
  preset: CronPreset;
  /** `HH:MM` for the daily and weekly presets. */
  time: string;
  /** The minute of the hourly preset. */
  minute: number;
  /** 0-6, Sunday is 0. */
  weekday: number;
  /** The expression that goes to the daemon. A preset writes it. */
  expr: string;
  tz: string;
  every: string;
  /** A `datetime-local` value, in the browser's time zone. */
  at: string;
  actionType: ScheduleActionType;
  mailTo: string;
  mailSubject: string;
  mailBody: string;
  /** The agent of a turn or agent action: `project/name`. */
  agent: string;
  text: string;
  startIfStopped: boolean;
  verb: AgentVerb;
  target: string;
  /** The command as one line; `splitCommand` makes the argv. */
  cmd: string;
  cwd: string;
  timeout: string;
  subscription: string;
}

export type FormKey = keyof ScheduleForm;

/** The daemon's `details.field` to the form field that shows it. */
export const FIELD_OF: Record<string, FormKey> = {
  name: "name",
  project: "scope",
  spec: "specKind",
  "spec.kind": "specKind",
  "spec.expr": "expr",
  "spec.tz": "tz",
  "spec.every": "every",
  "spec.at": "at",
  action: "actionType",
  "action.type": "actionType",
  "action.to": "mailTo",
  "action.subject": "mailSubject",
  "action.body": "mailBody",
  "action.agent": "agent",
  "action.text": "text",
  "action.start_if_stopped": "startIfStopped",
  "action.verb": "verb",
  "action.target": "target",
  "action.cmd": "cmd",
  "action.cwd": "cwd",
  "action.timeout_secs": "timeout",
  "action.subscription": "subscription",
  catch_up: "catchUp",
  enabled: "enabled",
};

export type ScheduleOutcome =
  | { kind: "idle" }
  | { kind: "sending" }
  /** The form is not complete; `field` is the field to fix. */
  | { kind: "invalid"; message: string; field?: FormKey }
  /** `403 forbidden`: the viewer may not write a schedule in that scope. */
  | { kind: "forbidden"; message: string; scope: string }
  /** `422`: the daemon refused a field. `field` is the form field, when known; `daemonField` is the daemon's name. */
  | { kind: "rejected"; message: string; field?: FormKey; daemonField?: string }
  | { kind: "error"; error: unknown }
  | { kind: "done"; schedule: Schedule };

const pad = (n: number) => String(n).padStart(2, "0");
const hm = (time: string): [number, number] => {
  const m = /^(\d{1,2}):(\d{2})$/.exec(time.trim());
  return m ? [Math.min(23, Number(m[1])), Math.min(59, Number(m[2]))] : [9, 0];
};

/** The expression of a preset. `custom` keeps the expression of the form. */
export function presetExpr(f: Pick<ScheduleForm, "preset" | "time" | "minute" | "weekday" | "expr">): string {
  const [h, m] = hm(f.time);
  switch (f.preset) {
    case "every5":
      return "*/5 * * * *";
    case "every15":
      return "*/15 * * * *";
    case "every60":
      return "0 * * * *";
    case "hourly":
      return `${f.minute} * * * *`;
    case "daily":
      return `${m} ${h} * * *`;
    case "weekly":
      return `${m} ${h} * * ${f.weekday}`;
    case "custom":
      return f.expr;
  }
}

/** The preset of an expression, for editing: the fields that make it, or `custom`. */
export function presetOf(expr: string): Pick<ScheduleForm, "preset"> & Partial<Pick<ScheduleForm, "time" | "minute" | "weekday">> {
  const e = expr.trim().replace(/\s+/g, " ");
  if (e === "*/5 * * * *") return { preset: "every5" };
  if (e === "*/15 * * * *") return { preset: "every15" };
  if (e === "0 * * * *") return { preset: "every60" };
  let m = /^(\d{1,2}) \* \* \* \*$/.exec(e);
  if (m && Number(m[1]) < 60) return { preset: "hourly", minute: Number(m[1]) };
  m = /^(\d{1,2}) (\d{1,2}) \* \* \*$/.exec(e);
  if (m && Number(m[1]) < 60 && Number(m[2]) < 24) return { preset: "daily", time: `${pad(Number(m[2]))}:${pad(Number(m[1]))}` };
  m = /^(\d{1,2}) (\d{1,2}) \* \* ([0-7])$/.exec(e);
  if (m && Number(m[1]) < 60 && Number(m[2]) < 24) return { preset: "weekly", time: `${pad(Number(m[2]))}:${pad(Number(m[1]))}`, weekday: Number(m[3]) % 7 };
  return { preset: "custom" };
}

/** A `datetime-local` value (browser time) from an RFC 3339 time. */
export function localInput(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** The empty form. The scope is the selected project; with no project, the viewer must choose. Never global by default. */
export const emptyScheduleForm = (selectedProject: string | null | undefined, init: Partial<ScheduleForm> = {}): ScheduleForm => ({
  name: "",
  scope: selectedProject ?? "",
  enabled: true,
  catchUp: "skip",
  specKind: "cron",
  preset: "every15",
  time: "09:00",
  minute: 0,
  weekday: 1,
  expr: "*/15 * * * *",
  tz: "UTC",
  every: "15m",
  at: "",
  actionType: "mail",
  mailTo: "",
  mailSubject: "",
  mailBody: "",
  agent: "",
  text: "",
  startIfStopped: false,
  verb: "restart",
  target: "",
  cmd: "",
  cwd: "",
  timeout: "",
  subscription: "",
  ...init,
});

/** The form of an existing schedule, for Edit. */
export function formFromSchedule(s: Schedule): ScheduleForm {
  const f = emptyScheduleForm(s.project ?? GLOBAL, { name: s.name, enabled: s.enabled, catchUp: s.catch_up, specKind: s.spec.kind, actionType: s.action.type });
  if (s.spec.kind === "cron") Object.assign(f, { expr: s.spec.expr, tz: s.spec.tz || "UTC" }, presetOf(s.spec.expr));
  else if (s.spec.kind === "interval") f.every = s.spec.every;
  else f.at = localInput(s.spec.at);
  const a = s.action;
  if (a.type === "mail") Object.assign(f, { mailTo: a.to, mailSubject: a.subject, mailBody: a.body });
  else if (a.type === "turn") Object.assign(f, { agent: a.agent, text: a.text, startIfStopped: !!a.start_if_stopped });
  else if (a.type === "exec") Object.assign(f, { target: a.target, cmd: commandLine(a.cmd), cwd: a.cwd ?? "", timeout: a.timeout_secs ? String(a.timeout_secs) : "" });
  else if (a.type === "agent") Object.assign(f, { agent: a.agent, verb: a.verb });
  else f.subscription = a.subscription;
  return f;
}

type Built<T> = { value: T } | { error: string; field: FormKey };

function buildSpec(f: ScheduleForm): Built<ScheduleSpec> {
  if (f.specKind === "cron") {
    const expr = f.expr.trim();
    if (!expr) return { error: "Give a cron expression.", field: "expr" };
    return { value: { kind: "cron", expr, tz: f.tz.trim() || "UTC" } };
  }
  if (f.specKind === "interval") {
    if (parseEvery(f.every) === null) return { error: "The interval is a duration such as 15m, 1h, or 1h30m.", field: "every" };
    return { value: { kind: "interval", every: f.every.trim() } };
  }
  const at = new Date(f.at);
  if (!f.at || Number.isNaN(at.getTime())) return { error: "Choose the date and time.", field: "at" };
  return { value: { kind: "at", at: at.toISOString() } };
}

function buildAction(f: ScheduleForm): Built<ScheduleAction> {
  const need = (v: string, field: FormKey, message: string): { error: string; field: FormKey } | null => (v.trim() ? null : { error: message, field });
  switch (f.actionType) {
    case "mail": {
      // The daemon takes an empty subject; the body is the message.
      return need(f.mailTo, "mailTo", "Choose the recipient.") ?? { value: { type: "mail", to: f.mailTo.trim(), subject: f.mailSubject.trim(), body: f.mailBody } };
    }
    case "turn": {
      const miss = need(f.agent, "agent", "Choose the agent.") ?? need(f.text, "text", "Write the text of the turn.");
      return miss ?? { value: { type: "turn", agent: f.agent.trim(), text: f.text, ...(f.startIfStopped ? { start_if_stopped: true } : {}) } };
    }
    case "exec": {
      const miss = need(f.target, "target", "Choose the target.");
      if (miss) return miss;
      const split = splitCommand(f.cmd);
      if ("error" in split) return { error: `The command: ${split.error}.`, field: "cmd" };
      if (!split.argv.length) return { error: "Write the command.", field: "cmd" };
      const action: ScheduleAction = { type: "exec", target: f.target.trim(), cmd: split.argv };
      if (f.cwd.trim()) action.cwd = f.cwd.trim();
      if (f.timeout.trim()) {
        const n = Number(f.timeout.trim());
        if (!Number.isInteger(n) || n < 1 || n > 3600) return { error: "The timeout is a whole number of seconds, 1 to 3600.", field: "timeout" };
        action.timeout_secs = n;
      }
      return { value: action };
    }
    case "agent":
      return need(f.agent, "agent", "Choose the agent.") ?? { value: { type: "agent", agent: f.agent.trim(), verb: f.verb } };
    case "probe":
      return need(f.subscription, "subscription", "Choose the subscription.") ?? { value: { type: "probe", subscription: f.subscription.trim() } };
  }
}

/** The body of `POST /api/schedules` (and of the PATCH) from the form, or the reason the form is not complete. */
export function scheduleBody(f: ScheduleForm): { body: NewSchedule } | { error: string; field: FormKey } {
  const name = f.name.trim();
  if (!name) return { error: "Give the schedule a name.", field: "name" };
  if (!f.scope) return { error: "Choose a scope: a project, or global.", field: "scope" };
  const spec = buildSpec(f);
  if ("error" in spec) return spec;
  const action = buildAction(f);
  if ("error" in action) return action;
  return { body: { name, project: f.scope === GLOBAL ? null : f.scope, enabled: f.enabled, spec: spec.value, action: action.value, catch_up: f.catchUp } };
}

/** The "next 3 runs" of the cron form, from the client parser. */
export function formPreview(f: Pick<ScheduleForm, "expr" | "tz">, now: number): { runs: number[] } | { error: string } {
  return previewCron(f.expr, f.tz.trim() || "UTC", now, 3);
}

// ----- the pickers -----

export interface Option {
  value: string;
  label: string;
}
export interface OptionGroup {
  label: string;
  options: Option[];
}

const scopeProject = (scope: string) => (scope && scope !== GLOBAL ? scope : null);

/** The agents that the action may name: the project's, or every agent for a global schedule. */
export function agentOptions(agents: Pick<Agent, "project" | "name">[], scope: string): Option[] {
  const p = scopeProject(scope);
  return agents
    .filter((a) => !p || a.project === p)
    .map((a) => ({ value: `${a.project}/${a.name}`, label: p ? a.name : `${a.project}/${a.name}` }))
    .sort((a, b) => a.label.localeCompare(b.label));
}

/** The mail recipients: the operator, the project inboxes (the scope's first), and the agents (the scope's first). */
export function recipientOptions(projects: Pick<Project, "name">[], agents: Pick<Agent, "project" | "name">[], scope: string): OptionGroup[] {
  const p = scopeProject(scope);
  const first = <T>(list: T[], mine: (t: T) => boolean) => [...list.filter(mine), ...list.filter((t) => !mine(t))];
  const inboxes = first(projects.slice().sort((a, b) => a.name.localeCompare(b.name)), (x) => x.name === p).map((x) => ({ value: `project:${x.name}`, label: `${x.name} inbox (project:${x.name})` }));
  const all = agents.slice().sort((a, b) => `${a.project}/${a.name}`.localeCompare(`${b.project}/${b.name}`));
  const people = first(all, (a) => a.project === p).map((a) => ({ value: `${a.project}/${a.name}`, label: `${a.project}/${a.name}` }));
  return [
    { label: "Operator", options: [{ value: "operator", label: "operator" }] },
    { label: "Project inboxes", options: inboxes },
    { label: "Agents", options: people },
  ];
}

/** The targets that the action may use: the global ones, and those of the scope's project. */
export function targetOptions(targets: Pick<Target, "name" | "project">[], scope: string): Option[] {
  const p = scopeProject(scope);
  return targets
    .filter((t) => !t.project || t.project === p)
    .map((t) => ({ value: t.name, label: t.project ? t.name : `${t.name} (global)` }))
    .sort((a, b) => a.label.localeCompare(b.label));
}

/** The time zones of the picker: UTC, the browser's zone, then every zone the browser knows. */
export function timeZones(local?: string): string[] {
  let all: string[];
  try {
    all = (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf?.("timeZone") ?? [];
  } catch {
    all = [];
  }
  return [...new Set(["UTC", ...(local ? [local] : []), ...all])];
}

// ----- the dialog -----

export function createScheduleDialog(api: Pick<Api, "schedules">, opts: { project?: string | null; schedule?: Schedule; init?: Partial<ScheduleForm> }) {
  const editing = opts.schedule ?? null;
  const [form, setForm] = createSignal<ScheduleForm>(editing ? { ...formFromSchedule(editing), ...opts.init } : emptyScheduleForm(opts.project, opts.init));
  const [outcome, setOutcome] = createSignal<ScheduleOutcome>({ kind: "idle" });

  const set = <K extends FormKey>(key: K, value: ScheduleForm[K]) => {
    setForm((f) => {
      const next = { ...f, [key]: value };
      // A preset, or a field of one, writes the expression; typing an expression makes it custom.
      if (key === "preset" || key === "time" || key === "minute" || key === "weekday") next.expr = presetExpr(next);
      if (key === "expr") Object.assign(next, presetOf(String(value)), { expr: String(value) });
      return next;
    });
    if (outcome().kind !== "sending" && outcome().kind !== "done") setOutcome({ kind: "idle" });
  };

  return {
    form,
    outcome,
    editing: () => editing,
    set,
    /** The message under a field: the form's own check, or the daemon's `422`. */
    fieldError(key: FormKey): string | null {
      const o = outcome();
      return (o.kind === "invalid" || o.kind === "rejected") && o.field === key ? o.message : null;
    },
    preview: (now: number) => formPreview(form(), now),
    /** Send the schedule. Answers it, or null on a refusal (see `outcome`). */
    async submit(): Promise<Schedule | null> {
      if (outcome().kind === "sending") return null;
      const b = scheduleBody(form());
      if ("error" in b) {
        setOutcome({ kind: "invalid", message: b.error, field: b.field });
        return null;
      }
      setOutcome({ kind: "sending" });
      try {
        // The PATCH body has no `project`: the daemon refuses unknown fields, and the scope does not change.
        const { project: _scope, ...patch } = b.body;
        const s = editing ? await api.schedules.update(editing.id, patch) : await api.schedules.create(b.body);
        setOutcome({ kind: "done", schedule: s });
        return s;
      } catch (e) {
        setOutcome(refusal(e, form().scope));
        return null;
      }
    },
  };
}

/** Sort a refusal: `403` and `422` show inline; any other error shows as a banner. */
export function refusal(e: unknown, scope: string): ScheduleOutcome {
  if (e instanceof ApiError && e.status === 403) return { kind: "forbidden", message: e.message, scope };
  if (e instanceof ApiError && (e.status === 422 || e.code === "invalid")) {
    const d = e.details as { field?: unknown } | undefined;
    const daemonField = typeof d?.field === "string" ? d.field : undefined;
    return { kind: "rejected", message: e.message, field: daemonField ? FIELD_OF[daemonField] : undefined, daemonField };
  }
  return { kind: "error", error: e };
}

export type ScheduleDialogState = ReturnType<typeof createScheduleDialog>;

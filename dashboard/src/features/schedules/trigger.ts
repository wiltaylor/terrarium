// The trigger part of the schedule form (docs/api.md §19.4): the kinds, the
// match rules, the optional dedupe and escalation, and max_per_hour, to and
// from the spec `on`. A trigger runs only mail, exec, or card: to notify an
// agent, it mails it. The daemon has the last word on each field; its `422`
// shows under the field that `details.field` names. Pure; no Solid imports.

import type { MatchOp, MatchRule, ScheduleActionType, TriggerActionType, TriggerSpec } from "../../api/types";
import { parseEvery } from "../../lib/schedule";

/** One row of the match editor. `value` is text; for `in` it is a comma list. */
export interface MatchRow {
  path: string;
  op: MatchOp;
  value: string;
}

/** The trigger fields of the schedule form. */
export interface TriggerFields {
  /** Kind globs, split by commas or spaces. */
  kinds: string;
  rules: MatchRow[];
  dedupe: boolean;
  dedupeKey: string;
  /** A duration: `90s`, `10m`, `1h`; 1 s to 7 d. */
  dedupeWindow: string;
  escalate: boolean;
  escalateKey: string;
  /** The matches in a row that escalate: 1 or more. */
  escalateAfter: string;
  /** Kind globs that set the count to zero, split by commas or spaces. */
  escalateResetOn: string;
  /** An agent, `project:<name>`, or `operator`. Empty: the daemon's default, `operator`. */
  escalateTo: string;
  /** 1-600. Empty: the daemon's default, 30. */
  maxPerHour: string;
}

export type TriggerKey = keyof TriggerFields;

/** The actions of a trigger. `turn`, `agent`, and `probe` are not: the daemon refuses them (`422`, field `action`). */
export const TRIGGER_ACTIONS: readonly TriggerActionType[] = ["mail", "exec", "card"];
export const isTriggerAction = (t: ScheduleActionType): t is TriggerActionType => (TRIGGER_ACTIONS as readonly string[]).includes(t);

export const MATCH_OPS: readonly { id: MatchOp; label: string }[] = [
  { id: "eq", label: "is (eq)" },
  { id: "ne", label: "is not (ne)" },
  { id: "glob", label: "matches glob" },
  { id: "in", label: "is one of (in)" },
];

/** The kinds the forges send (docs/api.md §20.4), for the suggestions. */
export const KIND_SUGGESTIONS: readonly string[] = ["ci.run.failed", "ci.run.succeeded", "ci.job.failed", "ci.run.*", "git.push", "pr.*", "pr.opened", "pr.merged"];

/** The match paths that the events of a forge have. */
export const PATH_SUGGESTIONS: readonly string[] = ["data.branch", "data.repo", "data.workflow", "data.job", "data.conclusion", "data.sender", "data.provider", "data.pr.base", "project", "kind", "actor", "summary"];

/** The placeholders of the action strings. A missing value is empty; another placeholder is `422`. */
export const TEMPLATE_PLACEHOLDERS: readonly { text: string; meaning: string }[] = [
  { text: "{{event.kind}}", meaning: "the kind, for example ci.run.failed" },
  { text: "{{event.summary}}", meaning: "the one-line summary" },
  { text: "{{event.project}}", meaning: "the project of the event" },
  { text: "{{event.seq}}", meaning: "the position in the event log" },
  { text: "{{event.actor}}", meaning: "who made it, for example source:terrarium/gh" },
  { text: "{{event.data.repo}}", meaning: "owner/name" },
  { text: "{{event.data.branch}}", meaning: "the branch" },
  { text: "{{event.data.commit}}", meaning: "the commit SHA" },
  { text: "{{event.data.run_url}}", meaning: "the web page of the run (of the job, for ci.job.failed)" },
  { text: "{{event.data.workflow}}", meaning: "the workflow name or file" },
  { text: "{{event.data.sender}}", meaning: "the login that caused the webhook" },
  { text: "{{event.data.pr.number}}", meaning: "the pull request, for pr.* events" },
];

export const emptyTriggerFields = (): TriggerFields => ({
  kinds: "",
  rules: [],
  dedupe: false,
  dedupeKey: "{{event.data.repo}}:{{event.data.branch}}",
  dedupeWindow: "10m",
  escalate: false,
  escalateKey: "{{event.data.repo}}:{{event.data.branch}}",
  escalateAfter: "3",
  escalateResetOn: "ci.run.succeeded",
  escalateTo: "",
  maxPerHour: "",
});

export const emptyRow = (): MatchRow => ({ path: "data.branch", op: "eq", value: "" });

/** A list typed as text: split by commas and white space; no empty items. */
export const splitList = (text: string): string[] =>
  text
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);

/** A match value as text, for the editor. */
const valueText = (v: MatchRule["value"]): string => (Array.isArray(v) ? v.map(String).join(", ") : String(v));

export const rowFromRule = (m: MatchRule): MatchRow => ({ path: m.path, op: m.op ?? "eq", value: valueText(m.value) });

/** The rule of a row. `in` takes the comma list (an item may hold spaces); the others take the trimmed text. */
export function ruleFromRow(r: MatchRow): MatchRule {
  const path = r.path.trim();
  if (r.op === "in")
    return {
      path,
      op: "in",
      value: r.value
        .split(",")
        .map((x) => x.trim())
        .filter(Boolean),
    };
  return { path, op: r.op, value: r.value.trim() };
}

const KNOWN_PATH = /^(project|kind|actor|summary|data\.[^.]+(\.[^.]+)*)$/;

/** Is the row empty, so that the form drops it? */
const blank = (r: MatchRow) => !r.path.trim() && !r.value.trim();

/** The trigger fields of a spec, for Edit. */
export function triggerFieldsFromSpec(spec: TriggerSpec): TriggerFields {
  const f = emptyTriggerFields();
  f.kinds = spec.kinds.join(", ");
  f.rules = (spec.match ?? []).map(rowFromRule);
  if (spec.dedupe) Object.assign(f, { dedupe: true, dedupeKey: spec.dedupe.key, dedupeWindow: spec.dedupe.window });
  if (spec.escalate) {
    const e = spec.escalate;
    Object.assign(f, { escalate: true, escalateKey: e.key, escalateAfter: String(e.after), escalateResetOn: (e.reset_on ?? []).join(", "), escalateTo: e.to ?? "" });
  }
  f.maxPerHour = spec.max_per_hour !== undefined ? String(spec.max_per_hour) : "";
  return f;
}

export type BuiltSpec = { value: TriggerSpec } | { error: string; field: TriggerKey };

/** The spec `on` from the trigger fields, or the reason it is not complete. Optional parts stay out of the body. */
export function buildTriggerSpec(f: TriggerFields): BuiltSpec {
  const kinds = splitList(f.kinds);
  if (!kinds.length) return { error: "Name one or more event kinds, for example ci.run.failed.", field: "kinds" };
  const match: MatchRule[] = [];
  for (const [i, r] of f.rules.entries()) {
    if (blank(r)) continue;
    const n = i + 1;
    if (!KNOWN_PATH.test(r.path.trim())) return { error: `Rule ${n}: the path is project, kind, actor, summary, or data.<path>.`, field: "rules" };
    const rule = ruleFromRow(r);
    if (r.op === "in" && !(rule.value as string[]).length) return { error: `Rule ${n}: "in" needs one or more values, split by commas.`, field: "rules" };
    if (r.op === "glob" && !rule.value) return { error: `Rule ${n}: "glob" needs a pattern.`, field: "rules" };
    match.push(rule);
  }
  const spec: TriggerSpec = { kind: "on", kinds };
  if (match.length) spec.match = match;
  if (f.dedupe) {
    if (!f.dedupeKey.trim()) return { error: "Give the dedupe key, for example {{event.data.repo}}.", field: "dedupeKey" };
    const sec = parseEvery(f.dedupeWindow);
    if (sec === null || sec < 1 || sec > 7 * 86400) return { error: "The window is a duration from 1s to 7d, for example 10m.", field: "dedupeWindow" };
    spec.dedupe = { key: f.dedupeKey.trim(), window: f.dedupeWindow.trim() };
  }
  if (f.escalate) {
    if (!f.escalateKey.trim()) return { error: "Give the escalation key, for example {{event.data.repo}}.", field: "escalateKey" };
    const after = Number(f.escalateAfter.trim());
    if (!f.escalateAfter.trim() || !Number.isInteger(after) || after < 1) return { error: "After is a whole number, 1 or more.", field: "escalateAfter" };
    const resetOn = splitList(f.escalateResetOn);
    spec.escalate = { key: f.escalateKey.trim(), after, ...(resetOn.length ? { reset_on: resetOn } : {}), ...(f.escalateTo.trim() ? { to: f.escalateTo.trim() } : {}) };
  }
  if (f.maxPerHour.trim()) {
    const n = Number(f.maxPerHour.trim());
    if (!Number.isInteger(n) || n < 1 || n > 600) return { error: "Max per hour is a whole number, 1 to 600.", field: "maxPerHour" };
    spec.max_per_hour = n;
  }
  return { value: spec };
}

/** The fields that the preset "CI failure → mail an agent" sets (docs/api.md §19.4, the first example). */
export interface CiPreset extends Partial<TriggerFields> {
  name?: string;
  actionType: "mail";
  mailTo?: string;
  mailSubject: string;
  mailBody: string;
}

/**
 * The preset "CI failure → mail an agent": a failed run on main mails the
 * agent, once per repo in 10 minutes; after 3 failures in a row it mails the
 * operator instead, until a run succeeds. `agent` is the recipient (empty:
 * keep the one in the form); `name` fills an empty name.
 */
export function ciFailurePreset(current: { name: string; mailTo: string }, agent?: string): CiPreset {
  return {
    ...(current.name.trim() ? {} : { name: "ci-failed" }),
    kinds: "ci.run.failed",
    rules: [{ path: "data.branch", op: "eq", value: "main" }],
    dedupe: true,
    dedupeKey: "{{event.data.repo}}",
    dedupeWindow: "10m",
    escalate: true,
    escalateKey: "{{event.data.repo}}",
    escalateAfter: "3",
    escalateResetOn: "ci.run.succeeded",
    escalateTo: "",
    actionType: "mail",
    ...(agent && !current.mailTo ? { mailTo: agent } : {}),
    mailSubject: "CI failed: {{event.data.repo}}",
    mailBody: "Fix the build on {{event.data.branch}}. Run: {{event.data.run_url}}",
  };
}

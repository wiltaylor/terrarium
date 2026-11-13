// The words and the edit helpers of the column cog (TER-1116, kanban rework
// C6): each check, the queue and each rule of a column in plain words, the
// PUT body, and the field of a 409 or 422 answer (docs/api.md §8.2.1).

import { ApiError } from "../../api/http";
import type { CatalogField, ColumnChecks, ColumnOptions, ColumnOptionsPut, ColumnQueue, ColumnRule, RuleAction, RuleCatalog, RuleTrigger, RuleWhen } from "../../api/types";

/** A column reference of a rule: `@review` is the column with that role. */
export function columnRef(ref: unknown): string {
  const s = String(ref ?? "");
  return s.startsWith("@") ? `the ${s.slice(1).replace(/_/g, " ")} column` : s;
}

const list = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : v === undefined || v === null || v === "" ? [] : [String(v)]);
/** "a", "a or b", "a, b or c". */
export function orList(items: string[], word = "or"): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} ${word} ${items.at(-1)}`;
}

/** A duration in seconds as words: `1800` is "30 minutes". */
export function seconds(n: number): string {
  const unit = (v: number, u: string) => `${v} ${u}${v === 1 ? "" : "s"}`;
  if (n % 86400 === 0) return unit(n / 86400, "day");
  if (n % 3600 === 0) return unit(n / 3600, "hour");
  if (n % 60 === 0) return unit(n / 60, "minute");
  return unit(n, "second");
}

const EVENT_WORDS: Record<string, string> = { queued: "is queued", started: "starts", requeued: "is queued again", verdict: "has a verdict", cancelled: "is cancelled", ended: "ends" };

/** The trigger as the start of a sentence. */
export function triggerText(t: RuleTrigger): string {
  switch (t.kind) {
    case "lands": return "When a card lands in this column (after its queue)";
    case "leaves": return "When a card leaves this column";
    case "job_result": {
      const kinds = list(t.job_kinds);
      const job = kinds.length ? `the ${orList(kinds)} job` : "the job";
      const events = list(t.events).map((e) => EVENT_WORDS[e] ?? e);
      return `When ${job} of a card here ${events.length ? orList(events) : "has an event"}`;
    }
    case "timer": return `Every ${seconds(Number(t.every_seconds) || 0)} while a card is here`;
    case "count_reached": return `When this column holds ${Number(t.count) || 0} cards`;
    case "wait_met": return "When the wait of a card here is met";
    case "archived": return "When a card here is archived";
    default: return `When ${t.kind}`;
  }
}

const OP_WORDS: Record<string, string> = { eq: "is", ne: "is not", glob: "matches", in: "is one of" };
export function whenText(w: RuleWhen): string {
  const value = Array.isArray(w.value) ? orList(w.value.map(String)) : String(w.value ?? "");
  return `the event's ${w.path} ${OP_WORDS[w.op] ?? w.op} ${value}`;
}

const CARD_WORDS: Record<string, (v: unknown) => string> = {
  group_key_prefix: (v) => `its group key starts with ${String(v)}`,
  column: (v) => `it is in ${orList(list(v).map(columnRef))}`,
  not_column: (v) => `it is not in ${orList(list(v).map(columnRef))}`,
  has_labels: (v) => `it has the label${list(v).length === 1 ? "" : "s"} ${orList(list(v), "and")}`,
  lacks_labels: (v) => `it has none of the labels ${orList(list(v))}`,
  assigned: (v) => (v ? "it has an assignee" : "it has no assignee"),
  archived: (v) => (v ? "it is archived" : "it is not archived"),
};
export function cardConditionText(c: Record<string, unknown>): string {
  return orList(Object.entries(c).map(([k, v]) => CARD_WORDS[k]?.(v) ?? `${k} is ${String(v)}`), "and");
}
export function jobConditionText(c: Record<string, unknown>): string {
  return orList(Object.entries(c).map(([k, v]) => (typeof v === "boolean" ? `the job ${v ? "is" : "is not"} a ${k} job` : `the job ${k} is ${orList(list(v))}`)), "and");
}

/** An action as words: `card.move` to `@accepted` is "move the card to the accepted column". */
export function actionText(a: RuleAction): string {
  const args = a.args ?? {};
  switch (a.verb) {
    case "card.move": return `move the card to ${columnRef(args.to)}`;
    case "card.label": {
      const parts = [list(args.add).length ? `add the label ${orList(list(args.add), "and")}` : "", list(args.remove).length ? `remove the label ${orList(list(args.remove), "and")}` : ""].filter(Boolean);
      return parts.length ? parts.join(" and ") : "change the labels";
    }
    case "card.comment": return `comment "${String(args.body ?? "")}"`;
    case "card.assign": return args.to === null || args.to === "none" ? "clear the assignee" : `assign the card to ${String(args.to ?? "")}`;
    case "mail": return `mail ${String(args.to ?? "")}${args.subject ? ` "${String(args.subject)}"` : ""}`;
    default: {
      const rest = Object.entries(args).map(([k, v]) => `${k} ${Array.isArray(v) ? v.join(", ") : String(v)}`);
      return `${a.verb}${rest.length ? ` (${rest.join("; ")})` : ""}`;
    }
  }
}

/** The whole rule in plain words. */
export function ruleSentence(r: ColumnRule): string {
  const ifs = [
    ...(r.when ?? []).map(whenText),
    ...(r.if_card ?? []).map((c) => `the card: ${cardConditionText(c)}`),
    ...(r.if_job ?? []).map(jobConditionText),
  ].filter(Boolean);
  const actions = r.actions.map(actionText);
  const limit = r.max_per_hour ? ` At most ${r.max_per_hour} times an hour.` : "";
  return `${triggerText(r.trigger)}${ifs.length ? `, if ${orList(ifs, "and")}` : ""}: ${actions.length ? orList(actions, "and then") : "nothing"}.${limit}`;
}

export const CHECK_TEXT: Record<keyof ColumnChecks, string> = {
  require_comment_on_enter: "A move into this column needs a comment.",
  require_dependencies_met: "A card enters only when its dependencies are accepted or done.",
  terminal_for_members: "A card here is final for members: only the operator or a project admin moves, edits or assigns it.",
  clear_eta_on_enter: "A card that enters loses its ETA.",
};
export const CHECK_KEYS = Object.keys(CHECK_TEXT) as (keyof ColumnChecks)[];

export function queueText(q: ColumnQueue): string {
  switch (q.release) {
    case "wip": return "Cards enter when the WIP limit has room.";
    case "job": return "Cards are released one job at a time.";
    case "batch": {
      const cap = q.max_batch_cards ? `, at most ${q.max_batch_cards} cards in a batch` : "";
      return `Cards are released in batches of ${q.batch_size ?? "?"}, or when the oldest has waited ${q.max_wait_seconds ? seconds(q.max_wait_seconds) : "?"}${cap}.`;
    }
    default: return String(q.release);
  }
}

/** The `PUT` body from the options as edited. The queue numbers go only with `batch`. */
export function putBody(read: ColumnOptions, edited: { checks: ColumnChecks; queue: ColumnQueue; rules: ColumnRule[] }): ColumnOptionsPut {
  const q = edited.queue;
  const queue: ColumnQueue = q.release === "batch"
    ? { release: "batch", batch_size: q.batch_size ?? null, max_wait_seconds: q.max_wait_seconds ?? null, max_batch_cards: q.max_batch_cards ?? null }
    : { release: q.release, batch_size: null, max_wait_seconds: null, max_batch_cards: null };
  return { base_version: read.version, checks: { ...edited.checks }, queue, rules: edited.rules.map(cleanRule) };
}

/** A rule without empty condition lists and empty arguments, as the daemon stores it. */
export function cleanRule(r: ColumnRule): ColumnRule {
  const out: ColumnRule = { ...(r.id ? { id: r.id } : {}), name: r.name.trim(), enabled: r.enabled !== false, trigger: r.trigger, actions: r.actions.map((a) => ({ verb: a.verb, args: Object.fromEntries(Object.entries(a.args ?? {}).filter(([, v]) => v !== undefined && v !== "" && !(Array.isArray(v) && !v.length))) })) };
  if (r.description?.trim()) out.description = r.description.trim();
  if (r.when?.length) out.when = r.when;
  const ifCard = (r.if_card ?? []).filter((c) => Object.keys(c).length);
  if (ifCard.length) out.if_card = ifCard;
  const ifJob = (r.if_job ?? []).filter((c) => Object.keys(c).length);
  if (ifJob.length && r.trigger.kind === "job_result") out.if_job = ifJob;
  if (r.max_per_hour) out.max_per_hour = r.max_per_hour;
  return out;
}

/** A new rule: the first trigger and the first action of the catalog. */
export function newRule(catalog: RuleCatalog, taken: string[]): ColumnRule {
  let n = taken.length + 1;
  while (taken.includes(`rule-${n}`)) n++;
  const trigger = catalog.triggers[0]?.kind ?? "lands";
  const verb = catalog.actions[0]?.verb ?? "card.comment";
  return { name: `rule-${n}`, enabled: true, trigger: { kind: trigger }, actions: [{ verb, args: {} }] };
}

/** A trigger of another kind, keeping no fields of the old one. */
export function triggerOf(catalog: RuleCatalog, kind: string): RuleTrigger {
  const t: RuleTrigger = { kind };
  for (const f of catalog.triggers.find((x) => x.kind === kind)?.fields ?? []) if (f.required && f.min !== undefined && f.type === "count") t[f.name] = f.min;
  return t;
}

/** The value of an input for a catalog field: a list from "a, b", a number for a count, a bool. */
export function fieldValue(f: CatalogField, raw: string | boolean | string[]): unknown {
  if (f.type === "bool") return raw === true || raw === "true";
  if (Array.isArray(raw)) return raw;
  if (typeof raw === "boolean") return raw;
  if (f.type === "count") return raw.trim() === "" ? undefined : Number(raw);
  if (f.type === "list" || f.type === "columns") return raw.split(",").map((s) => s.trim()).filter(Boolean);
  return raw;
}
/** The text of a field value in an input. */
export function fieldText(v: unknown): string {
  if (v === undefined || v === null) return "";
  return Array.isArray(v) ? v.join(", ") : String(v);
}

/** The 409 `stale_base` of a PUT: someone wrote the options since they were read. */
export function isStale(e: unknown): boolean {
  return e instanceof ApiError && e.status === 409 && e.code === "stale_base";
}

/** The problems of a 422 by field path (`queue.batch_size`, `rules[1].actions[0].args.to`). */
export function problemsByPath(e: unknown): Record<string, string> {
  if (!(e instanceof ApiError) || e.status !== 422) return {};
  const d = (e.details ?? {}) as { path?: unknown; problems?: unknown };
  const out: Record<string, string> = {};
  if (Array.isArray(d.problems)) for (const p of d.problems as { path?: unknown; message?: unknown }[]) if (typeof p?.path === "string") out[p.path] = out[p.path] ? `${out[p.path]}; ${String(p.message ?? "")}` : String(p.message ?? "");
  if (typeof d.path === "string" && !(d.path in out)) out[d.path] = e.message;
  return out;
}

/** The problems at a path or under it (`rules[0]` takes `rules[0].name`), for a rule that is folded. */
export function problemsUnder(problems: Record<string, string>, prefix: string): string[] {
  return Object.entries(problems).filter(([p]) => p === prefix || p.startsWith(`${prefix}.`) || p.startsWith(`${prefix}[`)).map(([p, m]) => `${p}: ${m}`);
}

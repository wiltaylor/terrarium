// The Rules tab of the project (docs/api.md §19.12, TER-171): the pure
// parts, so the tests need no DOM.

import type { BoardFlow, EffectiveRule, RuleChange, RuleFinding, RuleRun, RuleToggle } from "../../api/types";

/** One row of the rules table. */
export interface RuleRow {
  name: string;
  origin: EffectiveRule["origin"];
  originLabel: string;
  enabled: boolean;
  events: string;
  verbs: string;
  description: string;
  wcl: string;
}

const ORIGIN_LABEL: Record<EffectiveRule["origin"], string> = {
  pack: "pack",
  override: "pack, changed in rules.wcl",
  own: "rules.wcl",
};

/** `job.verdict (2 when)`. */
export function eventsText(r: Pick<EffectiveRule, "on" | "when">): string {
  const on = r.on.length ? r.on.join(", ") : "-";
  const n = r.when?.length ?? 0;
  return n ? `${on} (${n} when)` : on;
}

export function ruleRows(rules: EffectiveRule[]): RuleRow[] {
  return rules.map((r) => ({
    name: r.name,
    origin: r.origin,
    originLabel: ORIGIN_LABEL[r.origin] ?? r.origin,
    enabled: r.enabled,
    events: eventsText(r),
    verbs: r.actions.map((a) => a.verb).join(", ") || "-",
    description: r.description ?? "",
    wcl: r.wcl,
  }));
}

/** What the flow means, in one line. */
export function flowText(flow: BoardFlow): string {
  return flow === "rules"
    ? "board_flow rules: the built-in pack runs the card flow, with the rules of the project's rules.wcl."
    : "board_flow code: the daemon runs the card flow. Only the rules of the project's rules.wcl run.";
}

export function findingText(f: RuleFinding): string {
  const kind = f.code === "missing_column" ? "Missing column" : "Stuck column";
  return `${kind} ${f.column}: ${f.message}`;
}

/**
 * The notice after a switch, with the lint findings of the rules that run now as a warning: an enable of a pack
 * rule that the migration turned off can name a missing column again (TER-226).
 */
export function toggleText(name: string, r: Pick<RuleToggle, "enabled" | "changed" | "findings">): string {
  const state = r.enabled ? "on" : "off";
  const head = r.changed ? `${name} is ${state}: rules.wcl changed.` : `${name} was ${state} already.`;
  const found = r.findings ?? [];
  return found.length ? `${head} Lint: ${found.map(findingText).join("; ")}.` : head;
}

export function changeText(c: RuleChange): string {
  return `${c.change} ${c.rule}: ${c.reasons.join("; ")}`;
}

/**
 * Offer the migration: the project still runs the coded flow, or the pack names a column that the board lacks
 * (a switch by hand, without the migration).
 */
export function offerMigration(flow: BoardFlow, findings: RuleFinding[]): boolean {
  return flow === "code" || findings.some((f) => f.code === "missing_column");
}

/** `done`, `noop`: fine; `failed`: bad; `skipped`, `pending`: in between. */
export function runTone(state: RuleRun["state"]): "ok" | "bad" | "warn" {
  if (state === "failed") return "bad";
  if (state === "done" || state === "noop") return "ok";
  return "warn";
}

/** The steps of a run in one line: `card.label ok, card.move failed: ...`. */
export function stepsText(r: Pick<RuleRun, "steps" | "reason">): string {
  const steps = r.steps.map((s) => `${s.verb} ${s.ok ? (s.changed === false ? "no change" : "ok") : `failed${s.error ? `: ${s.error}` : ""}`}`);
  const text = steps.join(", ");
  if (r.reason) return text ? `${text} (${r.reason})` : r.reason;
  return text || "-";
}

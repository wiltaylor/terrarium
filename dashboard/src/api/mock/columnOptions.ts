// The column options and the rule catalog in the mock (docs/api.md §8.2.1,
// TER-1112, TER-1120): today's checks as defaults, the queue settings, the
// rules of a column, the versioned PUT, and `GET /api/rules/catalog`. The
// answers have the daemon's shapes. The mock caller is the operator, so
// `can_edit` is true. It runs no rule.

import type { Api } from "../client";
import type { Column, ColumnChecks, ColumnOptions, ColumnOptionsPut, ColumnQueue, ColumnRule, RuleCatalog } from "../types";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface ColumnOptionsMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => void;
  emit: (e: { kind: "board"; actor: string; project: string; summary: string; data: Record<string, unknown> }) => unknown;
  /** A column by name or role alias (throws `422` for an unknown one). */
  column: (project: string, name: string) => Column;
  ulid: () => string;
}

const ONE_OF = (name: string, values: string[], required = true) => ({ name, type: "one_of", required, values });

/** A short copy of `board_rules::catalog` (crates/terrarium-core/src/board_rules.rs). */
export const MOCK_CATALOG: RuleCatalog = {
  model_version: 1,
  triggers: [
    { kind: "lands", description: "a card enters the column (after the column's queue)", fields: [] },
    { kind: "leaves", description: "a card leaves the column", fields: [] },
    { kind: "job_result", description: "while a card is in the column: an event of its job", fields: [
      { name: "events", type: "one_of_list", required: true, values: ["queued", "started", "requeued", "verdict", "cancelled", "ended"], min: 1 },
      { name: "job_kinds", type: "one_of_list", required: false, values: ["validate", "merge", "e2e", "release"] },
    ] },
    { kind: "timer", description: "while a card is in the column: every `every_seconds`", fields: [{ name: "every_seconds", type: "count", required: true, min: 60, max: 2592000 }] },
    { kind: "count_reached", description: "the column holds `count` cards", fields: [{ name: "count", type: "count", required: true, min: 1, max: 1000 }] },
    { kind: "wait_met", description: "while a card is in the column: its wait is met", fields: [] },
    { kind: "archived", description: "a card in the column is archived", fields: [] },
  ],
  conditions: {
    when: { description: "a condition on the event; every one must hold", paths: ["project", "kind", "actor", "summary", "data.<path>"], ops: ["eq", "ne", "glob", "in"] },
    if_card: { description: "a condition on the current card; each field that is set must hold", fields: [
      { name: "group_key_prefix", type: "text" }, { name: "column", type: "columns" }, { name: "not_column", type: "columns" },
      { name: "has_labels", type: "list" }, { name: "lacks_labels", type: "list" }, { name: "assigned", type: "bool" }, { name: "archived", type: "bool" },
    ] },
    if_job: { description: "a condition on the current job (job_result only)", fields: [
      { name: "state", type: "one_of_list", values: ["queued", "running", "passed", "failed", "error", "cancelled"] },
      { name: "outcome", type: "one_of_list", values: ["passed", "red", "conflict", "infra", "error"] },
      { name: "split", type: "bool" }, { name: "batch", type: "bool" }, { name: "rerun", type: "bool" }, { name: "halves", type: "bool" },
    ] },
  },
  actions: [
    { verb: "card.move", args: [{ name: "to", type: "column", required: true }, { name: "comment", type: "text", required: false }] },
    { verb: "card.label", args: [{ name: "add", type: "list", required: false }, { name: "remove", type: "list", required: false }] },
    { verb: "card.comment", args: [{ name: "body", type: "text", required: true }] },
    { verb: "card.assign", args: [{ name: "to", type: "text", required: true }] },
    { verb: "mail", args: [{ name: "to", type: "text", required: true }, { name: "subject", type: "text", required: true }, { name: "body", type: "text", required: false }] },
    { verb: "agent.park", args: [] },
    { verb: "agent.end", args: [ONE_OF("outcome", ["done", "merged", "archived"])] },
  ],
  placeholders: { event: ["kind", "summary", "project", "seq", "actor", "data.<path>"], card: ["id", "title", "column"], job: ["id", "kind", "state"], group: ["key"], rule: ["name"] },
  limits: { max_rules_per_column: 50, max_rules_per_project: 300, max_actions: 20, max_rule_bytes: 32768, max_name_chars: 64, max_description_chars: 500, max_per_hour: 600 },
};

const NAME = /^[A-Za-z0-9._-]{1,64}$/;

export function createColumnOptionsMock(ctx: ColumnOptionsMockContext) {
  const { reply, fail } = ctx;
  /** Stored options by `project/column`; a column without an entry has today's defaults. */
  const stored = new Map<string, Omit<ColumnOptions, "project" | "column" | "can_edit">>();
  const defaults = (col: Column) => ({
    version: 1,
    checks: { require_comment_on_enter: col.role === "blocked", require_dependencies_met: col.role === "working", terminal_for_members: col.name === "done", clear_eta_on_enter: true } satisfies ColumnChecks,
    queue: { release: "wip", batch_size: null, max_wait_seconds: null, max_batch_cards: null } satisfies ColumnQueue,
    rules: [] as ColumnRule[],
  });
  const read = (project: string, name: string): ColumnOptions => {
    const col = ctx.column(project, name);
    const o = stored.get(`${project}/${col.name}`) ?? defaults(col);
    return structuredClone({ project, column: col.name, ...o, can_edit: true });
  };
  const problemsOf = (body: ColumnOptionsPut, current: ColumnRule[]) => {
    const problems: { path: string; message: string }[] = [];
    const q = body.queue;
    if (q.release === "batch") {
      if (!q.batch_size || q.batch_size < 1) problems.push({ path: "queue.batch_size", message: "a batch queue needs a batch size of 1 to 1000" });
      if (!q.max_wait_seconds || q.max_wait_seconds < 1) problems.push({ path: "queue.max_wait_seconds", message: "a batch queue needs the most seconds to wait" });
    }
    const names = new Set<string>();
    (body.rules ?? []).forEach((r, i) => {
      if (!NAME.test(r.name)) problems.push({ path: `rules[${i}].name`, message: "a-z, A-Z, 0-9, `.`, `_`, `-`, at most 64" });
      else if (names.has(r.name)) problems.push({ path: `rules[${i}].name`, message: `\`${r.name}\` is twice in the column` });
      names.add(r.name);
      if (r.id && !current.some((c) => c.id === r.id)) problems.push({ path: `rules[${i}].id`, message: "not a rule of this column" });
      if (!MOCK_CATALOG.triggers.some((t) => t.kind === r.trigger?.kind)) problems.push({ path: `rules[${i}].trigger.kind`, message: "unknown trigger" });
      if (!r.actions.length) problems.push({ path: `rules[${i}].actions`, message: "a rule needs at least one action" });
      r.actions.forEach((a, j) => {
        const verb = MOCK_CATALOG.actions.find((v) => v.verb === a.verb);
        if (!verb) return problems.push({ path: `rules[${i}].actions[${j}].verb`, message: `unknown verb \`${a.verb}\`` });
        for (const arg of verb.args) if (arg.required && (a.args?.[arg.name] === undefined || a.args?.[arg.name] === "")) problems.push({ path: `rules[${i}].actions[${j}].args.${arg.name}`, message: "required" });
      });
    });
    return problems;
  };
  const board: Pick<Api["board"], "options" | "putOptions"> = {
    options: (project, column) => reply(() => read(project, column)),
    putOptions: (project, column, body) => reply(() => {
      const before = read(project, column);
      if (body.base_version !== before.version) fail(409, "stale_base", `the options of \`${before.column}\` are at version ${before.version}, not ${body.base_version}; read them again`, { version: before.version });
      const problems = problemsOf(body, before.rules);
      if (problems.length) fail(422, "invalid", `${problems[0]!.path}: ${problems[0]!.message}`, { path: problems[0]!.path, problems });
      const rules = body.rules === undefined ? before.rules : body.rules.map((r) => ({ ...r, id: r.id ?? ctx.ulid(), enabled: r.enabled ?? true }));
      const after = { checks: structuredClone(body.checks), queue: structuredClone(body.queue), rules: structuredClone(rules) };
      const same = JSON.stringify(after) === JSON.stringify({ checks: before.checks, queue: before.queue, rules: before.rules });
      if (same) return before;
      const version = before.version + 1;
      stored.set(`${project}/${before.column}`, { version, ...after });
      ctx.audit("board.column_options", project, project, { column: before.column, version, before, after });
      ctx.emit({ kind: "board", actor: "operator", project, summary: `${project}: options of ${before.column} changed`, data: { column: before.column, options_version: version, checks: after.checks, queue: after.queue, rule_count: rules.length } });
      return read(project, column);
    }),
  };
  /** Project `terrarium`: a batch queue and two rules on review (the mock board has no merge_queue). */
  const seed = () => {
    stored.set("terrarium/review", {
      version: 3,
      checks: { require_comment_on_enter: false, require_dependencies_met: false, terminal_for_members: false, clear_eta_on_enter: true },
      queue: { release: "batch", batch_size: 4, max_wait_seconds: 1800, max_batch_cards: 8 },
      rules: [
        { id: "01M7MOCKRULE0000000000001", name: "merge-pass", enabled: true, trigger: { kind: "job_result", events: ["verdict"], job_kinds: ["merge"] }, if_job: [{ outcome: ["passed"] }], actions: [{ verb: "card.move", args: { to: "done" } }] },
        { id: "01M7MOCKRULE0000000000002", name: "merge-red-note", description: "Tell the card why the merge failed.", enabled: false, trigger: { kind: "job_result", events: ["ended"] }, if_card: [{ lacks_labels: ["hold"] }], actions: [{ verb: "card.comment", args: { body: "Merge failed: {{job.state}}" } }] },
      ],
    });
  };
  return { board, catalog: () => reply(() => structuredClone(MOCK_CATALOG)), seed };
}

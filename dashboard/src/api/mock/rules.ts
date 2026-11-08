// The board rules in the mock (docs/api.md §19.8, §19.9, §19.12, TER-171):
// the rules that run, the lint, the run log, one rule on or off, and the
// migration to `board_flow = rules`. The answers have the daemon's shapes.
//
// The mock keeps a small copy of the built-in pack (the rules that name a
// column) and, for each project, its flow, the pack rules that its
// rules.wcl maps to the board, and the pack rules that it turns off. It does
// not parse WCL and it runs no rule: `runs` answers the seeded run log.

import type { Api } from "../client";
import type { BoardFlow, Column, EffectiveRule, RuleChange, RuleFinding, RuleRun } from "../types";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface RulesMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => void;
  /** The project (throws `404` for an unknown one). */
  project: (name: string) => unknown;
  /** The columns of the project's board. */
  columns: (project: string) => Column[];
  iso: () => string;
}

/** A pack rule of the mock: the move target (a column reference), if any. */
interface PackRule {
  name: string;
  description?: string;
  on: string[];
  verbs: string[];
  to?: string;
}

/** A short copy of crates/terrarium-core/src/rules.default.wcl. */
export const MOCK_PACK: PackRule[] = [
  { name: "merge-queued", on: ["job"], verbs: ["card.label", "card.comment"] },
  { name: "job-started", on: ["job"], verbs: ["card.label"] },
  { name: "validate-pass", on: ["job.verdict"], verbs: ["card.label", "card.move", "card.assign"], to: "@review" },
  { name: "merge-pass", on: ["job.verdict"], verbs: ["card.label", "card.move", "card.assign"], to: "@accepted" },
  { name: "job-fail-red", description: "A red job that does not run again fails.", on: ["job.verdict"], verbs: ["card.label", "card.prepend", "card.move", "card.assign", "mail"], to: "@ready" },
  { name: "job-cancelled", on: ["job.verdict"], verbs: ["card.label"] },
  { name: "agent-park", description: "The card waits on others: its bound agent parks.", on: ["card"], verbs: ["agent.park"] },
  { name: "agent-unpark", on: ["card"], verbs: ["agent.unpark"] },
  { name: "agent-end-done", on: ["card"], verbs: ["agent.end"] },
];

/** The wait roles of the pack's `agent-park`, with legacy name fallback. */
const WAIT_COLUMNS = ["validating", "review", "merge_queue", "accepted"];

interface ProjectRules {
  flow: BoardFlow;
  /** Pack rules that rules.wcl maps to the board (the migration). */
  mapped: Set<string>;
  /** Pack rules that rules.wcl turns off. */
  off: Set<string>;
  hash: number;
}

const resolves = (cols: Column[], ref: string): boolean => {
  const x = ref.startsWith("@") ? ref.slice(1) : null;
  return x === null ? cols.some((c) => c.name === ref) : cols.some((c) => c.role === x) || cols.some((c) => !c.role && c.name === x);
};

const hasColumn = (cols: Column[], name: string) => cols.some((c) => c.role === name || (!c.role && c.name === name));

/** The verbs of a pack rule as the migration maps it to `cols`. */
const mappedVerbs = (r: PackRule, cols: Column[]): string[] => (r.to && !resolves(cols, r.to) ? r.verbs.map((v) => (v === "card.move" ? "card.comment" : v)) : r.verbs);

/** What the migration changes for `cols`. */
function plan(cols: Column[]): RuleChange[] {
  const out: RuleChange[] = [];
  for (const r of MOCK_PACK) {
    if (r.to && !resolves(cols, r.to)) out.push({ rule: r.name, change: "mapped", reasons: [`the board has no column \`${r.to}\`: the move becomes its comment`] });
  }
  const missing = WAIT_COLUMNS.filter((n) => !hasColumn(cols, n));
  if (missing.length) {
    const why = missing.map((n) => `the board has no column \`${n}\`: dropped from \`data.to_role\``);
    out.push({ rule: "agent-park", change: "mapped", reasons: why });
    out.push({ rule: "agent-unpark", change: "mapped", reasons: why });
  }
  return out;
}

function wcl(r: EffectiveRule): string {
  const lines = [`rule "${r.name}" {`];
  if (r.description) lines.push(`  description = "${r.description}"`);
  if (!r.enabled) lines.push("  enabled = false");
  if (r.on.length) lines.push(`  on = [${r.on.map((o) => `"${o}"`).join(", ")}]`);
  for (const a of r.actions) lines.push(`  ${a.verb.replace(".", "_")} {}`);
  lines.push("}");
  return `${lines.join("\n")}\n`;
}

export function createRulesMock(ctx: RulesMockContext) {
  const state = new Map<string, ProjectRules>();
  const runs: RuleRun[] = [];

  const of = (project: string): ProjectRules => {
    ctx.project(project);
    let s = state.get(project);
    if (!s) {
      s = { flow: "code", mapped: new Set(), off: new Set(), hash: 0 };
      state.set(project, s);
    }
    return s;
  };

  const hasFile = (s: ProjectRules) => s.mapped.size > 0 || s.off.size > 0;
  const hashOf = (s: ProjectRules) => (hasFile(s) ? `mockhash${s.hash}` : null);

  const effective = (project: string): EffectiveRule[] => {
    const s = of(project);
    if (s.flow !== "rules") {
      // Code flow: only the rules of rules.wcl run (no pack). The mock's file holds only the short forms that turn pack rules off.
      return [...s.off].map((name, i) => {
        const rule: EffectiveRule = { name, enabled: false, on: [], actions: [], line: i + 1, origin: "own", wcl: "" };
        rule.wcl = wcl(rule);
        return rule;
      });
    }
    const cols = ctx.columns(project);
    return MOCK_PACK.map((r, i) => {
      const override = s.mapped.has(r.name) || s.off.has(r.name);
      const rule: EffectiveRule = {
        name: r.name,
        ...(r.description ? { description: r.description } : {}),
        enabled: !s.off.has(r.name),
        on: s.off.has(r.name) && !s.mapped.has(r.name) ? [] : r.on,
        actions: (s.mapped.has(r.name) ? mappedVerbs(r, cols) : r.verbs).map((verb) => ({ verb })),
        line: i + 1,
        origin: override ? "override" : "pack",
        wcl: "",
      };
      rule.wcl = wcl(rule);
      return rule;
    });
  };

  const findings = (project: string): RuleFinding[] => {
    const s = of(project);
    if (s.flow !== "rules") return [];
    const cols = ctx.columns(project);
    const out: RuleFinding[] = [];
    for (const r of MOCK_PACK) {
      if (!r.to || s.off.has(r.name) || s.mapped.has(r.name) || resolves(cols, r.to)) continue;
      out.push({ code: "missing_column", column: r.to, rules: [r.name], message: `the board has no column \`${r.to}\`: each run of \`${r.name}\` fails` });
    }
    return out;
  };

  const api: Api["rules"] = {
    effective: (project) =>
      ctx.reply(() => {
        const s = of(project);
        return { project, board_flow: s.flow, exists: hasFile(s), hash: hashOf(s), source: hasFile(s) ? "api" : null, rules: effective(project) };
      }),
    lint: (project, text) =>
      ctx.reply(() => {
        const s = of(project);
        if (text !== undefined && !text.includes("rule")) ctx.fail(422, "invalid", "rules.wcl:1:1: not a rules file", { problems: [{ line: 1, column: 1, message: "not a rules file" }] });
        return { project, board_flow: s.flow, source: text === undefined ? "stored" : "text", findings: findings(project) };
      }),
    runs: (project, q = {}) =>
      ctx.reply(() => {
        of(project);
        const list = runs.filter((r) => r.project === project && (!q.rule || r.rule === q.rule));
        return { runs: list.slice(0, q.limit ?? 100) };
      }),
    enable: (project, rule) =>
      ctx.reply(() => {
        const s = of(project);
        if (!MOCK_PACK.some((r) => r.name === rule)) ctx.fail(404, "not_found", `no rule \`${rule}\` in the project's rules or the pack`);
        const changed = s.off.delete(rule);
        if (changed) {
          s.hash++;
          ctx.audit("rules.put", project, project, { hash: hashOf(s) });
        }
        return { project, rule, enabled: true, changed, hash: hashOf(s), findings: findings(project) };
      }),
    disable: (project, rule) =>
      ctx.reply(() => {
        const s = of(project);
        if (!MOCK_PACK.some((r) => r.name === rule)) ctx.fail(404, "not_found", `no rule \`${rule}\` in the project's rules or the pack`);
        const changed = !s.off.has(rule);
        if (changed) {
          s.off.add(rule);
          s.hash++;
          ctx.audit("rules.put", project, project, { hash: hashOf(s) });
        }
        return { project, rule, enabled: false, changed, hash: hashOf(s), findings: findings(project) };
      }),
    migrate: (project, apply = false, baseHash) =>
      ctx.reply(() => {
        const s = of(project);
        if (apply && baseHash !== undefined && baseHash !== (hashOf(s) ?? "")) ctx.fail(409, "conflict", `the rules of project \`${project}\` changed: read them again`);
        const changes = plan(ctx.columns(project)).filter((c) => !s.mapped.has(c.rule));
        const text = changes.length ? `// The built-in pack mapped to this board (terrarium rule migrate, TER-171).\n${changes.map((c) => `rule "${c.rule}" { ... }\n`).join("")}` : "";
        if (apply) {
          const before = s.flow;
          for (const c of changes) s.mapped.add(c.rule);
          if (changes.length) s.hash++;
          s.flow = "rules";
          ctx.audit("rules.migrate", project, project, { board_flow: "rules", previous_board_flow: before, hash: hashOf(s), changes: changes.map((c) => ({ rule: c.rule, change: c.change })) });
        }
        return { project, applied: apply, board_flow: apply ? "rules" : s.flow, changed: changes.length > 0, text, hash: hashOf(s), changes, findings: apply ? findings(project) : [] };
      }),
  };

  /** A few runs for project `terrarium`, newest first. */
  const seed = () => {
    const at = ctx.iso();
    runs.push(
      { id: "01JBRULRUN0000000000000002", project: "terrarium", rule: "job-started", rule_hash: "pack+-", event_seq: 41, card: "TER-12", state: "done", step: 1, steps: [{ verb: "card.label", ok: true, changed: true }], created_at: at, updated_at: at },
      { id: "01JBRULRUN0000000000000001", project: "terrarium", rule: "agent-park", rule_hash: "pack+-", event_seq: 40, card: "TER-12", state: "noop", step: 1, steps: [{ verb: "agent.park", ok: true, changed: false }], reason: null, created_at: at, updated_at: at },
    );
  };

  return { api, seed };
}

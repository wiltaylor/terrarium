// The model of the Tests tab (docs/api.md, test runs): counts, the stacked bar,
// the compare sections, the case filter, ANSI stripping, and durations. Pure, no
// Solid imports, so the tests and the mock use it.

import type { CardTests, TestCase, TestCaseDiff, TestCaseStatus, TestCompare, TestCounts, TestRunStatus, TestRunSummary, TestSuite } from "../../api/types";

/** The statuses of a case, in the order of the bar and the counts text. */
export const CASE_STATUSES: readonly TestCaseStatus[] = ["passed", "failed", "errored", "skipped"];

/** The icon and the word of a case status. The page shows both: the colour is never the only sign. */
export const CASE_ICON: Record<TestCaseStatus, string> = { passed: "✓", failed: "✗", errored: "!", skipped: "–" };

/** The chip class of a status (styles/global.css `.chip.ok`, `.chip.bad`; no class for a plain chip). */
export const STATUS_TONE: Record<TestCaseStatus | TestRunStatus, string> = {
  passed: "ok",
  failed: "bad",
  errored: "bad",
  skipped: "",
  empty: "",
};

export const emptyCounts = (): TestCounts => ({ passed: 0, failed: 0, errored: 0, skipped: 0, total: 0 });

/** Failed plus errored. */
export const failing = (c: TestCounts): number => c.failed + c.errored;

/** True for a failed or an errored case. */
export const isFailing = (s: TestCaseStatus | null | undefined): boolean => s === "failed" || s === "errored";

/** The run status from its counts, as the daemon gives it. */
export function runStatus(c: TestCounts): TestRunStatus {
  if (failing(c) > 0) return "failed";
  return c.total > 0 ? "passed" : "empty";
}

/** Add the counts of one case. */
export function addCase(c: TestCounts, s: TestCaseStatus): TestCounts {
  return { ...c, [s]: c[s] + 1, total: c.total + 1 };
}

export interface BarSegment {
  status: TestCaseStatus;
  count: number;
  /** The width in percent, 0 to 100. */
  pct: number;
}

/** The segments of the stacked bar: one for each status with cases, in CASE_STATUSES order. The widths add to 100. */
export function barSegments(c: TestCounts): BarSegment[] {
  const total = CASE_STATUSES.reduce((n, s) => n + c[s], 0);
  if (!total) return [];
  return CASE_STATUSES.filter((s) => c[s] > 0).map((s) => ({ status: s, count: c[s], pct: (c[s] / total) * 100 }));
}

/** "120 passed, 2 failed, 3 skipped". The statuses with no cases are not in the text. "no tests" for an empty run. */
export function countsText(c: TestCounts): string {
  const parts = CASE_STATUSES.filter((s) => c[s] > 0).map((s) => `${c[s]} ${s}`);
  return parts.length ? parts.join(", ") : "no tests";
}

/** The text of the board chip of a card: "✓ 120 passed", "✗ 3 failed", or "– no tests". */
export function chipText(t: Pick<CardTests, "counts">): string {
  const f = failing(t.counts);
  if (f > 0) return `✗ ${f} failed`;
  if (t.counts.total === 0) return "– no tests";
  return `✓ ${t.counts.passed} passed`;
}

/** "850 ms", "5.3 s", "2m 05s", "1h 02m". Null gives "". */
export function testDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !isFinite(ms) || ms < 0) return "";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

/** A short commit: the first 10 characters of a hex SHA, else the text as it is. */
export const shortCommit = (c: string | null | undefined): string => (c && /^[0-9a-f]{11,}$/i.test(c) ? c.slice(0, 10) : (c ?? ""));

// eslint-disable-next-line no-control-regex
const ANSI_RE = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]|\x9b[0-?]*[ -/]*[@-~]/g;

/**
 * The text with no ANSI escape sequences: CSI (colours, cursor moves), OSC
 * (titles, links), and the two-byte escapes. The other control characters
 * except tab and line ends go too, so the text shows as plain text.
 */
export function stripAnsi(s: string): string {
  // eslint-disable-next-line no-control-regex
  return s.replace(ANSI_RE, "").replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "");
}

/** The combined status of the runs of a version or a card: failed when one failed; passed when one passed; empty when all are empty; null for no runs. */
export function combinedStatus(runs: readonly Pick<TestRunSummary, "status">[]): TestRunStatus | null {
  if (!runs.length) return null;
  if (runs.some((r) => r.status === "failed")) return "failed";
  if (runs.some((r) => r.status === "passed")) return "passed";
  return "empty";
}

/** The sum of the counts of the runs. */
export function sumCounts(runs: readonly Pick<TestRunSummary, "counts">[]): TestCounts {
  const out = emptyCounts();
  for (const r of runs) for (const k of [...CASE_STATUSES, "total"] as const) out[k] += r.counts[k];
  return out;
}

/** The newest run of each name, newest first: the runs that tell the state of a version or a card. */
export function latestByName<R extends Pick<TestRunSummary, "name" | "created_at">>(runs: readonly R[]): R[] {
  const seen = new Map<string, R>();
  for (const r of runs) {
    const o = seen.get(r.name);
    if (!o || Date.parse(r.created_at) > Date.parse(o.created_at)) seen.set(r.name, r);
  }
  return [...seen.values()].sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at));
}

// ---- cases ----

/** The filter of the case list: `failed` is failed or errored, as on the daemon. `q` matches the suite, the class, and the name, with no case. */
export interface CaseFilter {
  status?: TestCaseStatus;
  q?: string;
}

export function matchCase(c: Pick<TestCase, "status" | "suite" | "classname" | "name">, f: CaseFilter): boolean {
  if (f.status === "failed" ? !isFailing(c.status) : f.status && c.status !== f.status) return false;
  const q = f.q?.trim().toLowerCase();
  if (q && ![c.suite, c.classname ?? "", c.name].some((x) => x.toLowerCase().includes(q))) return false;
  return true;
}

export interface SuiteGroup {
  name: string;
  duration_ms: number | null;
  /** The counts of the whole suite (all pages, no filter). */
  counts: TestCounts;
  /** The loaded cases of the suite that match the filter, in file order. */
  cases: TestCase[];
}

/**
 * The loaded cases by suite, in the order of `suites`. A suite with no loaded
 * case is not in the list; a case of a suite that `suites` does not name gets
 * a group with the counts of its cases.
 */
export function groupBySuite(suites: readonly TestSuite[], cases: readonly TestCase[]): SuiteGroup[] {
  const groups = new Map<string, SuiteGroup>();
  for (const s of suites) groups.set(s.name, { name: s.name, duration_ms: s.duration_ms, counts: s.counts, cases: [] });
  const extra = new Set<string>();
  for (const c of cases) {
    let g = groups.get(c.suite);
    if (!g) {
      g = { name: c.suite, duration_ms: null, counts: emptyCounts(), cases: [] };
      groups.set(c.suite, g);
      extra.add(c.suite);
    }
    if (extra.has(c.suite)) g.counts = addCase(g.counts, c.status);
    g.cases.push(c);
  }
  return [...groups.values()].filter((g) => g.cases.length);
}

/** "Failed only" is on at first when the run has a failed or an errored case. */
export const defaultFailedOnly = (r: Pick<TestRunSummary, "counts">): boolean => failing(r.counts) > 0;

/** The case text blocks to show, in order, with no ANSI codes. Empty blocks are not in the list. */
export function caseDetails(c: Pick<TestCase, "message" | "trace" | "stdout" | "stderr">): { label: string; text: string }[] {
  const out: { label: string; text: string }[] = [];
  for (const [label, v] of [
    ["Message", c.message],
    ["Trace", c.trace],
    ["Stdout", c.stdout],
    ["Stderr", c.stderr],
  ] as const) {
    const text = v ? stripAnsi(v) : "";
    // A trace that only repeats the message adds nothing.
    if (text.trim() && !(label === "Trace" && text.trim() === stripAnsi(c.message ?? "").trim())) out.push({ label, text });
  }
  return out;
}

// ---- compare ----

export type CompareKey = "newly_failing" | "fixed" | "still_failing" | "new_tests" | "removed_tests";

export interface CompareSection {
  key: CompareKey;
  title: string;
  /** The full number; `items` has at most 1000. */
  count: number;
  items: TestCaseDiff[];
  tone: string;
  /** Open at first: the newly failing and the fixed cases. */
  open: boolean;
}

const SECTIONS: { key: CompareKey; title: string; tone: string; open: boolean }[] = [
  { key: "newly_failing", title: "Newly failing", tone: "bad", open: true },
  { key: "fixed", title: "Fixed", tone: "ok", open: true },
  { key: "still_failing", title: "Still failing", tone: "warn", open: false },
  { key: "new_tests", title: "New tests", tone: "", open: false },
  { key: "removed_tests", title: "Removed tests", tone: "", open: false },
];

/** The sections of a compare, in order. Empty when there is no base run. */
export function compareSections(c: TestCompare): CompareSection[] {
  if (!c.base) return [];
  return SECTIONS.map((s) => ({ ...s, count: c.counts[s.key], items: c[s.key] }));
}

/** The match key of a case in a compare: suite, class, and name. */
export const caseKey = (c: Pick<TestCase, "suite" | "classname" | "name">): string => `${c.suite}\u0000${c.classname ?? ""}\u0000${c.name}`;

/** The compare lists of two case lists (the mock's daemon logic). Each list keeps the order of its source and at most `cap` items. */
export function diffCases(
  run: readonly TestCase[],
  base: readonly TestCase[],
  cap = 1000,
): Pick<TestCompare, "counts" | CompareKey> {
  const before = new Map(base.map((c) => [caseKey(c), c]));
  const now = new Map(run.map((c) => [caseKey(c), c]));
  const diff = (c: TestCase, b: TestCase | undefined, inRun: boolean): TestCaseDiff => ({
    suite: c.suite,
    classname: c.classname,
    name: c.name,
    status: inRun ? c.status : null,
    base_status: inRun ? (b?.status ?? null) : c.status,
    message: inRun ? c.message : null,
  });
  const lists: Record<CompareKey, TestCaseDiff[]> = { newly_failing: [], fixed: [], still_failing: [], new_tests: [], removed_tests: [] };
  for (const c of run) {
    const b = before.get(caseKey(c));
    if (!b) lists.new_tests.push(diff(c, b, true));
    else if (isFailing(c.status) && isFailing(b.status)) lists.still_failing.push(diff(c, b, true));
    else if (isFailing(c.status)) lists.newly_failing.push(diff(c, b, true));
    else if (isFailing(b.status)) lists.fixed.push(diff(c, b, true));
  }
  for (const b of base) if (!now.has(caseKey(b))) lists.removed_tests.push(diff(b, undefined, false));
  const counts = Object.fromEntries(Object.entries(lists).map(([k, v]) => [k, v.length])) as TestCompare["counts"];
  return { counts, ...(Object.fromEntries(Object.entries(lists).map(([k, v]) => [k, v.slice(0, cap)])) as Record<CompareKey, TestCaseDiff[]>) };
}

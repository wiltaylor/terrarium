// Test runs of the mock (docs/api.md, test runs), after the daemon's
// `store/test_runs`: the list filters and paging, the run with its suites and
// one page of cases, the compare, the links, the delete, and the raw file. The
// mock does not parse every format: an upload of JUnit XML or TAP gets a crude
// count, any other file is `422 invalid`. The seed gives project `terrarium`
// one run in each format; two `unit` runs show the compare.

import type { Api } from "../client";
import type { AgentEvent, Card, CardTests, TestCase, TestCaseStatus, TestCompare, TestFormat, TestRun, TestRunLinksPatch, TestRunListQuery, TestRunSummary, TestSuite } from "../types";
import { addCase, caseKey, diffCases, emptyCounts, matchCase, runStatus } from "../../features/tests/model";
import { paginate } from "../../lib/paging";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface TestRunMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: AgentEvent["kind"]; actor: string; project: string; summary: string; data: Record<string, unknown> }) => unknown;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  cards: () => Card[];
  /** The ULID of an agent of project `terrarium` by name: the `created_by` address, as the daemon stores it. */
  agentId: (name: string) => string;
  /** The time of the seed runs. */
  now: number;
}

interface Stored {
  summary: TestRunSummary;
  suites: { name: string; duration_ms: number | null }[];
  cases: TestCase[];
  raw: string;
  /** The insert order: the daemon's `pk`. */
  pk: number;
}

export const DEFAULT_CASE_LIMIT = 500;
export const MAX_CASE_LIMIT = 5000;

const ESC = "\x1b";
const red = (s: string) => `${ESC}[31m${s}${ESC}[39m`;
const green = (s: string) => `${ESC}[32m${s}${ESC}[39m`;
const dim = (s: string) => `${ESC}[2m${s}${ESC}[22m`;

type CaseDraft = Partial<TestCase> & { suite: string; name: string; status: TestCaseStatus };
const tc = (suite: string, name: string, status: TestCaseStatus, duration_ms: number | null, extra: Partial<TestCase> = {}): CaseDraft => ({ suite, name, status, duration_ms, ...extra });

const xmlEsc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** A plausible original file of the run, for "Download original". */
function rawOf(format: TestFormat, name: string, tool: string | null, cases: TestCase[], suites: string[]): string {
  const secs = (ms: number | null) => ((ms ?? 0) / 1000).toFixed(3);
  switch (format) {
    case "junit":
      return [
        '<?xml version="1.0" encoding="UTF-8"?>',
        `<testsuites name="${xmlEsc(name)}" tests="${cases.length}">`,
        ...suites.flatMap((s) => [
          `  <testsuite name="${xmlEsc(s)}" tests="${cases.filter((c) => c.suite === s).length}">`,
          ...cases
            .filter((c) => c.suite === s)
            .map((c) => {
              const inner =
                c.status === "failed" ? `<failure message="${xmlEsc(c.message ?? "")}">${xmlEsc(c.trace ?? "")}</failure>` : c.status === "errored" ? `<error message="${xmlEsc(c.message ?? "")}"/>` : c.status === "skipped" ? "<skipped/>" : "";
              return `    <testcase name="${xmlEsc(c.name)}" classname="${xmlEsc(c.classname ?? s)}" time="${secs(c.duration_ms)}">${inner}</testcase>`;
            }),
          "  </testsuite>",
        ]),
        "</testsuites>",
        "",
      ].join("\n");
    case "trx":
      return [
        '<?xml version="1.0" encoding="utf-8"?>',
        '<TestRun id="9f6c" name="mock" xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">',
        "  <Results>",
        ...cases.map((c) => `    <UnitTestResult testName="${xmlEsc(c.name)}" outcome="${{ passed: "Passed", failed: "Failed", errored: "Error", skipped: "NotExecuted" }[c.status]}" duration="00:00:${secs(c.duration_ms).padStart(6, "0")}0000" />`),
        "  </Results>",
        "</TestRun>",
        "",
      ].join("\n");
    case "ctrf":
      return JSON.stringify({ results: { tool: { name: tool ?? "mock" }, tests: cases.map((c) => ({ name: c.name, status: c.status === "errored" ? "failed" : c.status, duration: c.duration_ms ?? 0, suite: c.suite, message: c.message ?? undefined })) } }, null, 2);
    case "libtest-json":
      return cases.map((c) => JSON.stringify({ type: "test", event: c.status === "passed" ? "ok" : c.status === "skipped" ? "ignored" : "failed", name: `${c.suite}::${c.name}`, exec_time: (c.duration_ms ?? 0) / 1000 })).join("\n") + "\n";
    case "vitest-json":
    case "jest-json":
      return JSON.stringify(
        {
          numTotalTests: cases.length,
          testResults: suites.map((s) => ({
            name: s,
            assertionResults: cases.filter((c) => c.suite === s).map((c) => ({ title: c.name, ancestorTitles: c.classname ? [c.classname] : [], status: c.status === "skipped" ? "pending" : c.status === "errored" ? "failed" : c.status, duration: c.duration_ms, failureMessages: c.message ? [c.message] : [] })),
          })),
        },
        null,
        2,
      );
    case "tap":
      return ["TAP version 14", `1..${cases.length}`, ...cases.map((c, i) => `${isBad(c.status) ? "not ok" : "ok"} ${i + 1} - ${c.name}${c.status === "skipped" ? " # SKIP" : ""}`), ""].join("\n");
  }
}

const isBad = (s: TestCaseStatus) => s === "failed" || s === "errored";

/** Crude counts of an uploaded file: JUnit `<testcase>` elements or TAP `ok` lines. */
function crudeParse(text: string): { format: TestFormat; cases: CaseDraft[] } | null {
  if (/^\s*(TAP version|1\.\.\d+|(not )?ok\b)/m.test(text) && !text.trimStart().startsWith("<")) {
    const cases: CaseDraft[] = [];
    for (const m of text.matchAll(/^(not ok|ok)\b\s*\d*\s*-?\s*(.*)$/gm)) {
      const skip = /#\s*skip/i.test(m[2]!);
      cases.push(tc("tap", m[2]!.replace(/\s*#.*$/, "") || `test ${cases.length + 1}`, skip ? "skipped" : m[1] === "ok" ? "passed" : "failed", null));
    }
    return { format: "tap", cases };
  }
  if (/<testsuites?\b/.test(text)) {
    if (/<!DOCTYPE/i.test(text)) return null;
    const cases: CaseDraft[] = [];
    for (const m of text.matchAll(/<testcase\b([^>]*?)(\/>|>([\s\S]*?)<\/testcase>)/g)) {
      const name = /name="([^"]*)"/.exec(m[1]!)?.[1] ?? `case ${cases.length + 1}`;
      const body = m[3] ?? "";
      cases.push(tc("junit", name, /<failure\b/.test(body) ? "failed" : /<error\b/.test(body) ? "errored" : /<skipped\b/.test(body) ? "skipped" : "passed", null));
    }
    return { format: "junit", cases };
  }
  return null;
}

export function createTestRunMock(ctx: TestRunMockContext) {
  const runs: Stored[] = [];
  let pk = 0;
  let idn = 0;
  const newId = () => `01JBTRUN${String(++idn).padStart(18, "0")}`;

  /** A label that a run links to, as `store::link_label`: trimmed, 1 to 200 characters, no control characters. */
  const labelOf = (raw: string): string => {
    const l = raw.trim();
    if (!l) return ctx.fail(422, "invalid", "a label is empty");
    // eslint-disable-next-line no-control-regex
    if ([...l].length > 200 || /[\u0000-\u001f\u007f-\u009f]/.test(l)) return ctx.fail(422, "invalid", `bad label \`${l}\`: use 1-200 characters, no control characters`);
    return l;
  };
  const cardOf = (project: string, id: string): Card => {
    const c = ctx.cards().find((x) => x.id === id) ?? ctx.fail(404, "not_found", `card ${id} not found`);
    if (c.project !== project) ctx.fail(422, "invalid", "a test run links to a card of its project");
    return c;
  };
  const byId = (id: string): Stored => runs.find((r) => r.summary.id === id) ?? ctx.fail(404, "not_found", `test run ${id} not found`);

  const suitesOf = (s: Stored): TestSuite[] =>
    s.suites.map((x) => ({
      ...x,
      counts: s.cases.filter((c) => c.suite === x.name).reduce((n, c) => addCase(n, c.status), emptyCounts()),
    }));

  const add = (r: {
    project: string;
    name: string;
    format: TestFormat;
    tool: string | null;
    cases: CaseDraft[];
    at: string;
    by: string;
    byName: string;
    commit?: string | null;
    branch?: string | null;
    fileName: string;
    cards?: string[];
    labels?: string[];
    notes?: string[];
    raw?: string;
  }): Stored => {
    const suiteNames = [...new Set(r.cases.map((c) => c.suite))];
    const cases: TestCase[] = r.cases.map((c, i) => ({ seq: i + 1, classname: null, file: null, line: null, duration_ms: null, message: null, trace: null, stdout: null, stderr: null, ...c }));
    const counts = cases.reduce((n, c) => addCase(n, c.status), emptyCounts());
    const ms = cases.reduce((n, c) => n + (c.duration_ms ?? 0), 0);
    const id = newId();
    const raw = r.raw ?? rawOf(r.format, r.name, r.tool, cases, suiteNames);
    const summary: TestRunSummary = {
      id,
      project: r.project,
      name: r.name,
      format: r.format,
      tool: r.tool,
      status: runStatus(counts),
      counts,
      duration_ms: cases.some((c) => c.duration_ms !== null) ? ms : null,
      started_at: r.at,
      commit: r.commit ?? null,
      branch: r.branch ?? null,
      file_name: r.fileName,
      raw_size: new TextEncoder().encode(raw).length,
      notes: r.notes ?? [],
      cards: r.cards ?? [],
      labels: [...new Set((r.labels ?? []).map(labelOf))],
      created_by: r.by,
      created_by_name: r.byName,
      created_at: r.at,
      url: `/p/${r.project}/tests/${id}`,
    };
    const s: Stored = {
      summary,
      suites: suiteNames.map((n) => ({ name: n, duration_ms: cases.filter((c) => c.suite === n).reduce((t, c) => t + (c.duration_ms ?? 0), 0) })),
      cases,
      raw,
      pk: ++pk,
    };
    runs.push(s);
    return s;
  };

  const event = (s: Stored, change: string) =>
    ctx.emit({
      kind: "test_run",
      actor: "operator",
      project: s.summary.project,
      summary: `test run ${s.summary.name} ${change}: ${s.summary.counts.passed} passed, ${s.summary.counts.failed + s.summary.counts.errored} failed`,
      data: { change, project: s.summary.project, id: s.summary.id, name: s.summary.name, counts: s.summary.counts, cards: s.summary.cards, labels: s.summary.labels },
    });

  /** The newest older run of the same name in the project. */
  const previous = (s: Stored): Stored | undefined =>
    runs.filter((r) => r.summary.project === s.summary.project && r.summary.name === s.summary.name && r.pk < s.pk).sort((a, b) => b.pk - a.pk)[0];

  const newestFirst = () => [...runs].sort((a, b) => b.pk - a.pk);

  /** `Card.tests`: the newest run linked to the card. */
  const cardTests = (cardId: string): CardTests | undefined => {
    const s = newestFirst().find((r) => r.summary.cards.includes(cardId));
    if (!s) return undefined;
    const { id, name, status, counts, created_at } = s.summary;
    return { id, name, status, counts, created_at };
  };

  const api: Api["testRuns"] = {
    list: (project, q: TestRunListQuery = {}) =>
      ctx.reply(() => {
        ctx.project(project);
        const rows = newestFirst()
          .map((r) => r.summary)
          .filter(
            (r) =>
              r.project === project &&
              (!q.card || r.cards.includes(q.card)) &&
              (!q.label?.trim() || r.labels.includes(q.label.trim())) &&
              (!q.name || r.name === q.name) &&
              (!q.status || r.status === q.status),
          );
        return paginate(rows, q, (r) => r.id);
      }),
    upload: (project, form) =>
      ctx.reply(() => {
        ctx.project(project);
        const file = form.get("file");
        if (!(file instanceof Blob)) return ctx.fail(400, "bad_request", "the multipart upload needs a `file` part");
        if (form.has("version") || form.has("versions")) return ctx.fail(400, "bad_request", "version objects are removed (TER-58): link the run to a version label with `label` (`labels` in JSON), such as `version:1.4.0`");
        // The mock reads no Blob at once; a test passes the text in the `text` part.
        const text = String(form.get("text") ?? "");
        const parsed = crudeParse(text) ?? ctx.fail(422, "invalid", "the mock reads JUnit XML and TAP only (and a DOCTYPE is refused)");
        const s = add({
          project,
          name: String(form.get("name") ?? "") || parsed.format,
          format: parsed.format,
          tool: null,
          cases: parsed.cases,
          at: new Date().toISOString(),
          by: "operator",
          byName: "operator",
          commit: (form.get("commit") as string | null) ?? null,
          branch: (form.get("branch") as string | null) ?? null,
          fileName: (file as File).name || "upload",
          cards: (form.getAll("card") as string[]).map((c) => cardOf(project, c).id),
          labels: [...form.getAll("label"), ...form.getAll("labels")] as string[],
          raw: text,
        });
        event(s, "created");
        return s.summary;
      }),
    get: (id, q = {}) =>
      ctx.reply((): TestRun => {
        const s = byId(id);
        const limit = Math.max(1, Math.min(q.limit ?? DEFAULT_CASE_LIMIT, MAX_CASE_LIMIT));
        const match = s.cases.filter((c) => matchCase(c, q));
        const after = q.cursor ? Number(q.cursor) : 0;
        const rest = match.filter((c) => c.seq > after);
        const cases = rest.slice(0, limit);
        return {
          ...s.summary,
          suites: suitesOf(s),
          cases,
          cases_total: match.length,
          next_cursor: rest.length > limit ? String(cases[cases.length - 1]!.seq) : null,
        };
      }),
    compare: (id, withRun = "previous") =>
      ctx.reply((): TestCompare => {
        const s = byId(id);
        const base = withRun === "previous" ? previous(s) : byId(withRun);
        if (base && base.summary.project !== s.summary.project) ctx.fail(422, "invalid", "compare runs of one project");
        if (!base) {
          return { run: s.summary, base: null, counts: { newly_failing: 0, fixed: 0, still_failing: 0, new_tests: 0, removed_tests: 0 }, newly_failing: [], fixed: [], still_failing: [], new_tests: [], removed_tests: [] };
        }
        return { run: s.summary, base: base.summary, ...diffCases(s.cases, base.cases) };
      }),
    raw: (id) =>
      ctx.reply(() => {
        const s = byId(id);
        return { blob: new Blob([s.raw], { type: s.summary.format.endsWith("json") || s.summary.format === "ctrf" ? "application/json" : s.summary.format === "tap" ? "text/plain" : "application/xml" }), fileName: s.summary.file_name };
      }),
    links: (id, patch: TestRunLinksPatch) =>
      ctx.reply(() => {
        const s = byId(id);
        const p = s.summary.project;
        const add = (patch.add_cards ?? []).map((c) => cardOf(p, c));
        const rm = (patch.remove_cards ?? []).map((c) => cardOf(p, c));
        for (const c of [...add, ...rm]) if (c.archived) ctx.fail(409, "conflict", `${c.id} is archived`);
        const addL = (patch.add_labels ?? []).map(labelOf);
        const rmL = (patch.remove_labels ?? []).map(labelOf);
        const cards = new Set(s.summary.cards);
        for (const c of add) cards.add(c.id);
        for (const c of rm) cards.delete(c.id);
        const labels = new Set(s.summary.labels);
        for (const l of addL) labels.add(l);
        for (const l of rmL) labels.delete(l);
        s.summary.cards = [...cards];
        s.summary.labels = [...labels];
        if (add.length || addL.length) event(s, "linked");
        if (rm.length || rmL.length) event(s, "unlinked");
        return s.summary;
      }),
    remove: (id) =>
      ctx.reply(() => {
        const s = byId(id);
        runs.splice(runs.indexOf(s), 1);
        ctx.audit("test_run.delete", `${s.summary.project}/${s.summary.id}`, s.summary.project, { name: s.summary.name });
        event(s, "deleted");
      }),
  };

  /** One run in each format for project `terrarium`. Two `unit` runs: the newer one has a newly failing, a fixed, a still failing, a new, and a removed case. */
  const seed = () => {
    // The daemon lists by insert order: the plan goes in by time, oldest first.
    const plan: Parameters<typeof add>[0][] = [];
    const ago = (min: number) => new Date(ctx.now - min * 60_000).toISOString();
    const runner = { by: ctx.agentId("runner-dev"), byName: "terrarium/runner-dev" };
    const lead = { by: ctx.agentId("lead"), byName: "terrarium/lead" };
    const panic = (msg: string, at: string) => `thread '${at}' panicked at crates/terrarium-runner/src/pty.rs:214:9:\n${msg}\nnote: run with \`RUST_BACKTRACE=1\` environment variable to display a backtrace`;
    const unitBase = [
      tc("terrarium-runner::pty", "resize_sends_sigwinch", "failed", 412, { classname: "pty::tests", file: "crates/terrarium-runner/src/pty.rs", line: 210, message: "assertion `left == right` failed: rows", trace: panic("assertion `left == right` failed: rows\n  left: 24\n right: 40", "pty::tests::resize_sends_sigwinch"), stdout: "spawned fake TUI pid 4121\nresize 120x40 sent\n" }),
      tc("terrarium-runner::pty", "resize_clamps_to_min", "passed", 3, { classname: "pty::tests" }),
      tc("terrarium-runner::pty", "child_exit_closes_reader", "failed", 1203, { classname: "pty::tests", message: "timed out after 1s waiting for EOF", trace: panic("timed out after 1s waiting for EOF", "pty::tests::child_exit_closes_reader") }),
      tc("terrarium-runner::screen", "detects_prompt", "passed", 18, { classname: "screen::tests" }),
      tc("terrarium-runner::screen", "legacy_prompt_regex", "passed", 2, { classname: "screen::tests" }),
      tc("terrarium-runner::screen", "busy_spinner_is_working", "passed", 25, { classname: "screen::tests" }),
      tc("terrarium-core::event", "parses_hook_payload", "passed", 1, { classname: "event::tests" }),
      tc("terrarium-core::event", "unknown_kind_is_system", "skipped", null, { classname: "event::tests", message: "ignored: needs the v2 hook schema" }),
    ];
    const unitNew = [
      tc("terrarium-runner::pty", "resize_sends_sigwinch", "passed", 380, { classname: "pty::tests", file: "crates/terrarium-runner/src/pty.rs", line: 210 }),
      tc("terrarium-runner::pty", "resize_clamps_to_min", "failed", 4, { classname: "pty::tests", file: "crates/terrarium-runner/src/pty.rs", line: 236, message: "assertion failed: cols >= MIN_COLS", trace: panic("assertion failed: cols >= MIN_COLS", "pty::tests::resize_clamps_to_min"), stderr: `${red("error")}: cols 0 is below the minimum 20\n` }),
      tc("terrarium-runner::pty", "child_exit_closes_reader", "failed", 1201, { classname: "pty::tests", message: "timed out after 1s waiting for EOF", trace: panic("timed out after 1s waiting for EOF", "pty::tests::child_exit_closes_reader") }),
      tc("terrarium-runner::pty", "resize_debounces_bursts", "passed", 151, { classname: "pty::tests" }),
      tc("terrarium-runner::screen", "detects_prompt", "passed", 17, { classname: "screen::tests" }),
      tc("terrarium-runner::screen", "busy_spinner_is_working", "passed", 24, { classname: "screen::tests" }),
      tc("terrarium-core::event", "parses_hook_payload", "passed", 1, { classname: "event::tests" }),
      tc("terrarium-core::event", "unknown_kind_is_system", "skipped", null, { classname: "event::tests", message: "ignored: needs the v2 hook schema" }),
    ];
    plan.push({ project: "terrarium", name: "unit", format: "junit", tool: "cargo-nextest", cases: unitBase, at: ago(180), ...runner, commit: "4f2a9c1e7b3d5a6f8c9e0d1b2a3c4d5e6f7a8b9c", branch: "runner-dev/pty-resize", fileName: "junit.xml", cards: ["TER-12"] });
    plan.push({
      project: "terrarium",
      name: "dashboard",
      format: "vitest-json",
      tool: "vitest",
      cases: [
        tc("src/features/board/lanes.test.ts", "groups cards by version", "passed", 4, { classname: "lanes" }),
        tc("src/features/board/lanes.test.ts", "keeps the backlog first", "passed", 2, { classname: "lanes" }),
        tc("src/features/board/columns.test.ts", "refuses a second working column", "passed", 3, { classname: "columns" }),
        tc("src/lib/eta.test.ts", "counts down each second", "passed", 6, { classname: "eta" }),
        tc("src/lib/eta.test.ts", "marks an overdue ETA", "passed", 1, { classname: "eta" }),
        tc("src/api/mock/parity.test.ts", "answers in the daemon's shapes", "passed", 88, { classname: "mock parity with docs/api.md" }),
      ],
      at: ago(40),
      ...lead,
      commit: "b81d0c2",
      branch: "lead/mail-push",
      fileName: "vitest-report.json",
      cards: ["TER-16"],
      labels: ["version:0.1.0"],
    });
    plan.push({
      project: "terrarium",
      name: "web-e2e",
      format: "ctrf",
      tool: "playwright",
      cases: [
        tc("board.spec.ts", "drags a card to review", "passed", 2310),
        tc("board.spec.ts", "opens the card drawer from ?card=", "passed", 1180),
        tc("mail.spec.ts", "replies from the project inbox", "failed", 30012, {
          file: "e2e/mail.spec.ts",
          line: 41,
          message: `${red("Error")}: Timed out 30000ms waiting for expect(locator).toBeVisible()\n\nLocator: getByRole('button', { name: 'Reply' })`,
          trace: `    at e2e/mail.spec.ts:41:52\n    ${dim("at processTicksAndRejections (node:internal/process/task_queues:105:5)")}`,
          stdout: "[webserver] listening on :5173\n",
        }),
        tc("mail.spec.ts", "marks a thread read", "passed", 940),
        tc("phone.spec.ts", "shows one column at a time", "skipped", null, { message: "skipped on chromium-desktop" }),
      ],
      at: ago(65),
      ...lead,
      commit: "b81d0c2",
      branch: "lead/mail-push",
      fileName: "ctrf-report.json",
      labels: ["version:0.1.0"],
    });
    plan.push({
      project: "terrarium",
      name: "integration-dotnet",
      format: "trx",
      tool: "VSTest",
      cases: [
        tc("Terrarium.Client.Tests.AgentsTests", "ListsAgentsOfAProject", "passed", 120, { classname: "Terrarium.Client.Tests.AgentsTests" }),
        tc("Terrarium.Client.Tests.AgentsTests", "StartsAnAgent", "passed", 340, { classname: "Terrarium.Client.Tests.AgentsTests" }),
        tc("Terrarium.Client.Tests.MailTests", "SendsMail", "passed", 95, { classname: "Terrarium.Client.Tests.MailTests", stdout: "POST /api/mail -> 201\n" }),
        tc("Terrarium.Client.Tests.MailTests", "StreamsEvents", "skipped", null, { classname: "Terrarium.Client.Tests.MailTests", message: "Test is not executed: needs a live daemon." }),
      ],
      at: ago(60 * 26),
      ...lead,
      commit: "a1b2c3d",
      branch: "main",
      fileName: "TestResults.trx",
      cards: ["TER-11"],
    });
    plan.push({
      project: "terrarium",
      name: "doc-tests",
      format: "libtest-json",
      tool: "cargo test",
      cases: [
        tc("terrarium_core", "ids::AgentId::parse", "passed", 0),
        tc("terrarium_core", "eta::parse_duration", "passed", 1),
        tc("terrarium_core", "redact::Redactor::new", "passed", 2),
      ],
      at: ago(60 * 5),
      ...runner,
      commit: "4f2a9c1",
      branch: "main",
      fileName: "libtest.jsonl",
    });
    plan.push({
      project: "terrarium",
      name: "frontend-jest",
      format: "jest-json",
      tool: "jest",
      cases: [
        tc("src/codex/resume.test.js", "resumes by session id", "failed", 54, {
          classname: "codex resume",
          message: `${red("expect(")}received${red(").toBe(")}${green("expected")}${red(")")} // Object.is equality\n\nExpected: ${green('"sess-42"')}\nReceived: ${red("undefined")}`,
          trace: "    at Object.<anonymous> (src/codex/resume.test.js:18:27)",
        }),
        tc("src/codex/resume.test.js", "starts fresh with no session", "passed", 12, { classname: "codex resume" }),
        tc("src/codex/login.test.js", "reads the login status", "errored", 3, { classname: "codex login", message: "TypeError: Cannot read properties of undefined (reading 'status')" }),
      ],
      at: ago(15),
      by: ctx.agentId("adapter-codex"),
      byName: "terrarium/adapter-codex",
      branch: "adapter-codex/resume",
      fileName: "jest-results.json",
      cards: ["TER-15"],
    });
    plan.push({
      project: "terrarium",
      name: "smoke",
      format: "tap",
      tool: null,
      cases: [
        tc("smoke.sh", "daemon answers /api/health", "passed", null),
        tc("smoke.sh", "dashboard serves index.html", "passed", null),
        tc("smoke.sh", "artifact origin answers", "skipped", null, { message: "SKIP no artifact listener in this profile" }),
        tc("smoke.sh", "host agent connects", "passed", null),
      ],
      at: ago(60 * 48),
      by: "operator",
      byName: "operator",
      commit: "9d8e7f6",
      branch: "main",
      fileName: "smoke.tap",
      cards: ["TER-10"],
    });
    plan.push({ project: "terrarium", name: "unit", format: "junit", tool: "cargo-nextest", cases: unitNew, at: ago(20), ...runner, commit: "c0ffee1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b", branch: "runner-dev/pty-resize", fileName: "junit.xml", cards: ["TER-12"], labels: ["version:vnext"], notes: ["1 stdout truncated to 64 KiB"] });
    for (const r of plan.sort((a, b) => Date.parse(a.at) - Date.parse(b.at))) add(r);
  };

  return { api, seed, cardTests, keyOf: caseKey };
}

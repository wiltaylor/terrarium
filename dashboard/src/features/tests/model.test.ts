import { describe, expect, it } from "vitest";
import type { TestCase, TestCompare, TestCounts } from "../../api/types";
import {
  barSegments,
  caseDetails,
  chipText,
  combinedStatus,
  compareSections,
  countsText,
  defaultFailedOnly,
  diffCases,
  groupBySuite,
  latestByName,
  matchCase,
  runStatus,
  shortCommit,
  stripAnsi,
  sumCounts,
  testDuration,
} from "./model";

const counts = (passed: number, failed = 0, errored = 0, skipped = 0): TestCounts => ({ passed, failed, errored, skipped, total: passed + failed + errored + skipped });
const tcase = (seq: number, suite: string, name: string, status: TestCase["status"], extra: Partial<TestCase> = {}): TestCase => ({
  seq,
  suite,
  name,
  classname: null,
  file: null,
  line: null,
  status,
  duration_ms: null,
  message: null,
  trace: null,
  stdout: null,
  stderr: null,
  ...extra,
});

describe("test run counts", () => {
  it("gives bar segments for the statuses with cases, in order, that add to 100", () => {
    const s = barSegments(counts(6, 2, 0, 2));
    expect(s.map((x) => x.status)).toEqual(["passed", "failed", "skipped"]);
    expect(s.map((x) => x.pct)).toEqual([60, 20, 20]);
    expect(barSegments(counts(0))).toEqual([]);
  });

  it("writes the counts as text and the board chip with an icon and a word", () => {
    expect(countsText(counts(120, 2, 1, 3))).toBe("120 passed, 2 failed, 1 errored, 3 skipped");
    expect(countsText(counts(0))).toBe("no tests");
    expect(chipText({ counts: counts(120) })).toBe("✓ 120 passed");
    expect(chipText({ counts: counts(118, 2, 1) })).toBe("✗ 3 failed");
    expect(chipText({ counts: counts(0) })).toBe("– no tests");
  });

  it("gives the run status as the daemon does", () => {
    expect(runStatus(counts(3, 0, 1))).toBe("failed");
    expect(runStatus(counts(3, 0, 0, 2))).toBe("passed");
    expect(runStatus(counts(0))).toBe("empty");
    expect(runStatus(counts(0, 0, 0, 2))).toBe("passed");
  });

  it("combines the runs of a version: failed wins, then passed, then empty; none gives null", () => {
    expect(combinedStatus([])).toBeNull();
    expect(combinedStatus([{ status: "passed" }, { status: "failed" }])).toBe("failed");
    expect(combinedStatus([{ status: "empty" }, { status: "passed" }])).toBe("passed");
    expect(combinedStatus([{ status: "empty" }])).toBe("empty");
    expect(sumCounts([{ counts: counts(2, 1) }, { counts: counts(3, 0, 0, 1) }])).toEqual(counts(5, 1, 0, 1));
  });

  it("keeps the newest run of each name", () => {
    const runs = [
      { name: "unit", created_at: "2026-11-01T10:00:00Z", id: "a" },
      { name: "unit", created_at: "2026-11-01T12:00:00Z", id: "b" },
      { name: "e2e", created_at: "2026-11-01T11:00:00Z", id: "c" },
    ];
    expect(latestByName(runs).map((r) => r.id)).toEqual(["b", "c"]);
  });

  it("formats durations and commits", () => {
    expect(testDuration(null)).toBe("");
    expect(testDuration(850)).toBe("850 ms");
    expect(testDuration(5321)).toBe("5.3 s");
    expect(testDuration(125_000)).toBe("2m 05s");
    expect(testDuration(3_720_000)).toBe("1h 02m");
    expect(shortCommit("4f2a9c1e7b3d5a6f8c9e")).toBe("4f2a9c1e7b");
    expect(shortCommit("main@4f2a")).toBe("main@4f2a");
    expect(shortCommit(null)).toBe("");
  });
});

describe("ANSI stripping", () => {
  it("removes colour codes, OSC links, and control characters, and keeps tabs and line ends", () => {
    expect(stripAnsi("\x1b[31mError\x1b[39m: \x1b[1;32mok\x1b[0m")).toBe("Error: ok");
    expect(stripAnsi("\x1b]8;;https://x.test\x07link\x1b]8;;\x07")).toBe("link");
    expect(stripAnsi("a\tb\r\nc\x07\x08d")).toBe("a\tb\r\ncd");
    expect(stripAnsi("\x1b[2K\x1b[1Gprogress")).toBe("progress");
  });

  it("gives the case details with no ANSI and no trace that repeats the message", () => {
    const d = caseDetails({ message: "\x1b[31mboom\x1b[0m", trace: "boom", stdout: "out\n", stderr: "" });
    expect(d).toEqual([
      { label: "Message", text: "boom" },
      { label: "Stdout", text: "out\n" },
    ]);
    // Markup stays text: the page puts it in a text node.
    expect(caseDetails({ message: "<img src=x onerror=alert(1)>", trace: null, stdout: null, stderr: null })[0]!.text).toBe("<img src=x onerror=alert(1)>");
  });
});

describe("case filter and suites", () => {
  const cases = [
    tcase(1, "pty", "resize", "failed", { classname: "pty::tests" }),
    tcase(2, "pty", "clamp", "passed"),
    tcase(3, "screen", "prompt", "errored"),
    tcase(4, "screen", "spinner", "skipped"),
    tcase(5, "other", "lonely", "passed"),
  ];

  it("matches failed as failed or errored, and searches suite, class, and name with no case", () => {
    expect(cases.filter((c) => matchCase(c, { status: "failed" })).map((c) => c.seq)).toEqual([1, 3]);
    expect(cases.filter((c) => matchCase(c, { status: "skipped" })).map((c) => c.seq)).toEqual([4]);
    expect(cases.filter((c) => matchCase(c, { q: "PTY::" })).map((c) => c.seq)).toEqual([1]);
    expect(cases.filter((c) => matchCase(c, { q: "screen", status: "failed" })).map((c) => c.seq)).toEqual([3]);
  });

  it("groups the loaded cases by suite in the order of the suites; a suite with no loaded case goes", () => {
    const suites = [
      { name: "screen", duration_ms: 10, counts: counts(0, 0, 1, 1) },
      { name: "pty", duration_ms: 5, counts: counts(1, 1) },
      { name: "empty", duration_ms: 0, counts: counts(3) },
    ];
    const g = groupBySuite(suites, cases);
    expect(g.map((x) => x.name)).toEqual(["screen", "pty", "other"]);
    expect(g[0]!.cases.map((c) => c.seq)).toEqual([3, 4]);
    expect(g[0]!.counts).toEqual(counts(0, 0, 1, 1));
    // A suite that the list does not name counts its cases.
    expect(g[2]!.counts).toEqual(counts(1));
  });

  it("turns Failed only on at first when the run has failures", () => {
    expect(defaultFailedOnly({ counts: counts(3, 1) })).toBe(true);
    expect(defaultFailedOnly({ counts: counts(3, 0, 1) })).toBe(true);
    expect(defaultFailedOnly({ counts: counts(3, 0, 0, 2) })).toBe(false);
  });
});

describe("compare", () => {
  const base = [tcase(1, "s", "a", "failed"), tcase(2, "s", "b", "failed"), tcase(3, "s", "c", "passed"), tcase(4, "s", "e", "passed")];
  const run = [tcase(1, "s", "a", "passed"), tcase(2, "s", "b", "errored", { message: "still" }), tcase(3, "s", "c", "failed", { message: "new fail" }), tcase(4, "s", "d", "passed")];

  it("sorts the cases into newly failing, fixed, still failing, new, and removed, by suite + class + name", () => {
    const d = diffCases(run, base);
    expect(d.newly_failing.map((x) => x.name)).toEqual(["c"]);
    expect(d.newly_failing[0]).toMatchObject({ status: "failed", base_status: "passed", message: "new fail" });
    expect(d.fixed.map((x) => x.name)).toEqual(["a"]);
    expect(d.still_failing.map((x) => x.name)).toEqual(["b"]);
    expect(d.new_tests.map((x) => x.name)).toEqual(["d"]);
    expect(d.removed_tests).toEqual([{ suite: "s", classname: null, name: "e", status: null, base_status: "passed", message: null }]);
    expect(d.counts).toEqual({ newly_failing: 1, fixed: 1, still_failing: 1, new_tests: 1, removed_tests: 1 });
    // The class is part of the key.
    expect(diffCases([tcase(1, "s", "a", "passed", { classname: "X" })], [tcase(1, "s", "a", "passed")]).new_tests).toHaveLength(1);
  });

  it("caps each list and keeps the full count", () => {
    const many = Array.from({ length: 5 }, (_, i) => tcase(i, "s", `n${i}`, "failed"));
    const d = diffCases(many, [], 2);
    expect(d.new_tests).toHaveLength(2);
    expect(d.counts.new_tests).toBe(5);
  });

  it("gives the sections in order with newly failing and fixed open; none with no base", () => {
    const summary = {} as TestCompare["run"];
    const c: TestCompare = { run: summary, base: summary, ...diffCases(run, base) };
    const s = compareSections(c);
    expect(s.map((x) => x.key)).toEqual(["newly_failing", "fixed", "still_failing", "new_tests", "removed_tests"]);
    expect(s.filter((x) => x.open).map((x) => x.key)).toEqual(["newly_failing", "fixed"]);
    expect(s[0]!.tone).toBe("bad");
    expect(compareSections({ ...c, base: null })).toEqual([]);
  });
});

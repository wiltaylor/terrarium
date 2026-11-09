// The job queue of the mock (docs/api.md §35.4): the stats of a project's
// jobs over a window, as `GET /api/projects/{name}/jobs/stats` answers
// (TER-135). The seed gives project `terrarium` a month of merge jobs: most
// first attempts pass, one in three fails red and clears on the rerun (the
// flake the first-attempt rate is for), plus a conflict, an error, a split
// batch, and one queued job. `jobStats` folds the rows the way the daemon's
// store does, so any `since` answers, not just the seeded windows.

import type { Api } from "../client";
import type { FirstAttempts, JobStats, StepFails, TargetFails } from "../types";
import { parseDuration } from "../../lib/eta";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface JobsMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  now: () => number;
}

/** One ended or open job of the seed, in the terms the daemon's store counts. */
export interface MockJobRow {
  /** `created_at` of the first try: a rerun keeps it, so both tries stay in one window. */
  at: number;
  kind: "validate" | "merge";
  state: "passed" | "failed" | "queued";
  attempt: number;
  /** `conflict`: a merge conflict. `error`: the job could not run. Both never ran the recipe. */
  outcome?: "conflict" | "error";
  /** A half of a split batch: counts in `splits` only. */
  parent?: boolean;
  /** The failed recipe step, as the tail names it (`timeout` when killed). */
  step?: string;
  /** The failed test targets the tail names. */
  targets?: string[];
}

const emptyFirst = (): FirstAttempts => ({ ended: 0, passed: 0, failed: 0, pass_rate: null });

const add = (f: FirstAttempts, passed: boolean): void => {
  f.ended += 1;
  if (passed) f.passed += 1;
  else f.failed += 1;
  f.pass_rate = f.passed / f.ended;
};

/**
 * The stats of `rows` in the window that starts at `sinceMs` (inclusive), as
 * the daemon folds them: a first attempt is attempt 1 and no split half;
 * `conflict` and `error` count outside `ended`; split halves count in
 * `splits` only; queued jobs count in `open`.
 */
export function jobStats(project: string, rows: readonly MockJobRow[], sinceMs: number): JobStats {
  const s: JobStats = {
    project,
    since: new Date(sinceMs).toISOString(),
    jobs: 0,
    first_attempt: emptyFirst(),
    validate: emptyFirst(),
    merge: emptyFirst(),
    reruns: emptyFirst(),
    conflicts: 0,
    errors: 0,
    cancelled: 0,
    open: 0,
    splits: 0,
    failed_steps: [],
    failed_targets: [],
    untargeted_test_fails: 0,
  };
  const targets = new Map<string, { failed: number; first_attempt: number }>();
  const steps = new Map<string, number>();
  for (const r of rows) {
    if (r.at < sinceMs) continue;
    s.jobs += 1;
    const first = r.attempt <= 1 && !r.parent;
    if (r.state === "queued") {
      s.open += 1;
      continue;
    }
    if (r.state === "failed" && r.step) {
      steps.set(r.step, (steps.get(r.step) ?? 0) + 1);
      if (r.step === "test" && !(r.targets?.length)) s.untargeted_test_fails += 1;
    }
    for (const t of r.targets ?? []) {
      const e = targets.get(t) ?? { failed: 0, first_attempt: 0 };
      e.failed += 1;
      e.first_attempt += first ? 1 : 0;
      targets.set(t, e);
    }
    if (r.parent) {
      s.splits += 1;
      continue;
    }
    if (r.outcome === "error") {
      s.errors += 1;
      continue;
    }
    const passed = r.state === "passed";
    if (!first) {
      add(s.reruns, passed);
      continue;
    }
    if (r.outcome === "conflict") {
      s.conflicts += 1;
      continue;
    }
    add(s.first_attempt, passed);
    add(r.kind === "merge" ? s.merge : s.validate, passed);
  }
  s.failed_targets = [...targets.entries()]
    .map(([target, e]): TargetFails => ({ target, failed: e.failed, first_attempt: e.first_attempt }))
    .sort((a, b) => b.failed - a.failed || a.target.localeCompare(b.target));
  s.failed_steps = [...steps.entries()]
    .map(([step, failed]): StepFails => ({ step, failed }))
    .sort((a, b) => b.failed - a.failed || a.step.localeCompare(b.step));
  return s;
}

/** The start of the window in ms, or the error text the daemon answers with (`400`). */
export function sinceMs(since: string | undefined, now: number): number | string {
  const raw = since?.trim();
  if (!raw) return now - 7 * 86_400_000;
  const at = Date.parse(raw);
  if (Number.isFinite(at)) return at;
  const dur = parseDuration(raw);
  if (dur === null) return `bad since \`${raw}\`: give an RFC 3339 time or a duration such as 7d`;
  const back = now - dur;
  if (!Number.isFinite(back)) return `bad since \`${raw}\`: too far back`;
  return back;
}

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

/** A month of merge jobs for project `terrarium`: see the header. */
export function seedJobs(now: number): MockJobRow[] {
  const rows: MockJobRow[] = [];
  for (let d = 29; d >= 0; d--) {
    const day = now - d * DAY;
    // Two first attempts pass each day.
    rows.push({ at: day - 2 * HOUR, kind: "merge", state: "passed", attempt: 1 });
    rows.push({ at: day - 5 * HOUR, kind: "merge", state: "passed", attempt: 1 });
    // A red first attempt; the rerun clears it, and stays red one day in seven.
    const stays = d % 7 === 3;
    rows.push({ at: day - 8 * HOUR, kind: "merge", state: "failed", attempt: 1, step: "test", targets: ["-p terrariumd --test jobs"] });
    rows.push(stays
      ? { at: day - 8 * HOUR, kind: "merge", state: "failed", attempt: 2, step: "test", targets: ["-p terrariumd --test jobs"] }
      : { at: day - 8 * HOUR, kind: "merge", state: "passed", attempt: 2 });
    // The day-3 job ends in a merge conflict; the day-5 job cannot run.
    if (d === 3) rows.push({ at: day - 11 * HOUR, kind: "merge", state: "failed", attempt: 1, outcome: "conflict" });
    if (d === 5) rows.push({ at: day - 11 * HOUR, kind: "merge", state: "failed", attempt: 1, outcome: "error" });
    // The day-12 batch splits: both halves fail red, a `lint` step once.
    if (d === 12) {
      rows.push({ at: day - 11 * HOUR, kind: "merge", state: "failed", attempt: 1, parent: true, step: "test", targets: ["-p terrarium-core --lib"] });
      rows.push({ at: day - 11 * HOUR, kind: "merge", state: "failed", attempt: 1, parent: true, step: "lint" });
    }
    // The day-21 tail clip: a failed `test` step that names no target.
    if (d === 21) rows.push({ at: day - 11 * HOUR, kind: "merge", state: "failed", attempt: 1, step: "test" });
  }
  // One queued job: the queue is live.
  rows.push({ at: now - 30 * 60_000, kind: "merge", state: "queued", attempt: 1 });
  return rows;
}

export function createJobsMock(ctx: JobsMockContext) {
  // Rows per project: the seed fills `terrarium` only, so a project without a
  // queue answers the empty stats, as the daemon does.
  const rows = new Map<string, MockJobRow[]>();
  const api: Api["jobs"] = {
    stats: (project, since) =>
      ctx.reply(() => {
        ctx.project(project);
        const at = sinceMs(since, ctx.now());
        if (typeof at === "string") ctx.fail(400, "bad_request", at);
        return jobStats(project, rows.get(project) ?? [], at);
      }),
  };
  return {
    api,
    seed(project = "terrarium") {
      rows.set(project, seedJobs(ctx.now()));
    },
  };
}

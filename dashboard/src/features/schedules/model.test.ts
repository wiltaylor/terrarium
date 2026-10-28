import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent, Schedule, ScheduleRun } from "../../api/types";
import { applyRunAnswer, applyScheduleEvent, failedRecently, filterSchedules, groupSchedules, inView, readScheduleEvent, runOutputText, runSeconds, scheduleHref, tabOf, tabParam, touchesRuns, type ScheduleListState } from "./model";
import { createRunHistory } from "./history";
import { fakeClock } from "../../test/wait";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });

const sched = (id: string, project: string | null, over: Partial<Schedule> = {}): Schedule => ({
  id,
  name: id,
  project,
  enabled: true,
  spec: { kind: "interval", every: "15m" },
  action: { type: "probe", subscription: "x" },
  catch_up: "skip",
  created_by: "operator",
  run_as: "operator",
  next_run_at: "2026-10-26T10:15:00Z",
  last_run: null,
  runs_count: 0,
  source: "api",
  created_at: "2026-10-26T00:00:00Z",
  updated_at: "2026-10-26T00:00:00Z",
  ...over,
});
const run = (id: string, schedule: string, over: Partial<ScheduleRun> = {}): ScheduleRun => ({ id, schedule, started_at: "2026-10-26T10:00:00Z", finished_at: "2026-10-26T10:00:05Z", status: "ok", trigger: "manual", summary: "exit 0", output: "", error: null, ...over });
let seq = 0;
const ev = (data: Record<string, unknown>): AgentEvent => ({ seq: ++seq, id: `e${seq}`, at: "2026-10-26T10:00:00Z", kind: "schedule", summary: "", data });

const view = { project: "terrarium" };
const start = (): ScheduleListState => ({ items: [sched("a", "terrarium"), sched("g", null)], running: {} });

describe("applyScheduleEvent", () => {
  it("adds a created schedule of the view, and leaves out one of another project", () => {
    const s1 = applyScheduleEvent(start(), ev({ change: "created", schedule: sched("b", "terrarium") }), view);
    expect(s1.state.items.map((s) => s.id)).toEqual(["a", "b", "g"]);
    expect(s1.refetch).toBe(false);
    const s2 = applyScheduleEvent(start(), ev({ change: "created", schedule: sched("w", "webshop") }), view);
    expect(s2.state.items.map((s) => s.id)).toEqual(["a", "g"]);
    // The global view takes every project.
    expect(applyScheduleEvent(start(), ev({ change: "created", schedule: sched("w", "webshop") }), { project: null }).state.items).toHaveLength(3);
  });

  it("replaces on updated, enabled, and disabled; removes on deleted", () => {
    const off = applyScheduleEvent(start(), ev({ change: "disabled", schedule: sched("a", "terrarium", { enabled: false, next_run_at: null }) }), view);
    expect(off.state.items.find((s) => s.id === "a")).toMatchObject({ enabled: false, next_run_at: null });
    const gone = applyScheduleEvent(start(), ev({ change: "deleted", schedule: "a" }), view);
    expect(gone.state.items.map((s) => s.id)).toEqual(["g"]);
    // A schedule that moved to another project leaves the view.
    const moved = applyScheduleEvent(start(), ev({ change: "updated", schedule: sched("a", "webshop") }), view);
    expect(moved.state.items.map((s) => s.id)).toEqual(["g"]);
  });

  it("marks the row running on run_started, and clears it with the last run on run_finished", () => {
    const s1 = applyScheduleEvent(start(), ev({ change: "run_started", schedule: "a", run: run("r1", "a", { status: "running", finished_at: null }) }), view);
    expect(s1.state.running).toEqual({ a: "r1" });
    const s2 = applyScheduleEvent(s1.state, ev({ change: "run_finished", schedule: "a", run: run("r1", "a", { status: "failed", summary: "exit 1" }), status: "failed" }), view);
    expect(s2.state.running).toEqual({});
    expect(s2.state.items.find((s) => s.id === "a")).toMatchObject({ runs_count: 1, last_run: { run_id: "r1", status: "failed", summary: "exit 1" } });
    // next_run_at may have moved: read the list again.
    expect(s2.refetch).toBe(true);
    // With the whole schedule, no refetch.
    const s3 = applyScheduleEvent(s1.state, ev({ change: "run_finished", schedule: sched("a", "terrarium", { runs_count: 9 }), run: "r1", status: "ok" }), view);
    expect(s3).toMatchObject({ refetch: false, state: { running: {} } });
    expect(s3.state.items.find((s) => s.id === "a")!.runs_count).toBe(9);
  });

  it("asks for a refetch when the event has only an ID, or no ID; ignores other kinds", () => {
    expect(applyScheduleEvent(start(), ev({ change: "updated", schedule: "a" }), view).refetch).toBe(true);
    expect(applyScheduleEvent(start(), ev({ change: "created" }), view).refetch).toBe(true);
    const other = { ...ev({}), kind: "card" as const };
    expect(applyScheduleEvent(start(), other, view)).toEqual({ state: start(), refetch: false });
    expect(readScheduleEvent(ev({ change: "run_started", schedule_id: "a", run_id: "r2" }))).toMatchObject({ id: "a", runId: "r2" });
  });

  it("follows the mock's ID-only events from run now to the end, and the answer sets the last run", async () => {
    // A fake clock: the run takes 20 ms, so "the spinner is on" holds on any machine.
    const tick = fakeClock();
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleRunMs: 20 });
    const list = await api.schedules.list({ project: "terrarium" });
    let state: ScheduleListState = { items: list, running: {} };
    const refetches: string[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => {
      const r = applyScheduleEvent(state, e, view);
      state = r.state;
      if (r.refetch) refetches.push(String(e.data.change));
    });
    await tick();
    const t = list.find((x) => x.name === "tests")!;
    const pending = api.schedules.run(t.id);
    await tick(5);
    // run_started: the spinner, from the run ID.
    expect(state.running[t.id]).toMatch(/^01JBRUN/);
    await tick(15);
    const r = await pending;
    await tick();
    expect(state.running[t.id]).toBeUndefined();
    // run_finished has no schedule body: the view reads the list again.
    expect(refetches).toEqual(["run_finished"]);
    state = applyRunAnswer(state, r, view);
    expect(state.items.find((x) => x.id === t.id)).toMatchObject({ last_run: { run_id: r.id, status: "ok" }, runs_count: t.runs_count + 1 });
    s.close();
  });

  it("applyRunAnswer: a finished run clears the spinner and is the last run; a running one keeps it", () => {
    const st: ScheduleListState = { items: [sched("a", "terrarium")], running: { a: "?" } };
    expect(applyRunAnswer(st, run("r1", "a", { status: "running", finished_at: null }), view).running).toEqual({ a: "r1" });
    const done = applyRunAnswer(st, run("r1", "a", { status: "skipped", summary: "skipped: busy" }), view);
    expect(done.running).toEqual({});
    expect(done.items[0]).toMatchObject({ runs_count: 1, last_run: { status: "skipped", at: "2026-10-26T10:00:05Z" } });
    // The same run twice counts once.
    expect(applyRunAnswer(done, run("r1", "a"), view).items[0]!.runs_count).toBe(1);
  });

  it("touchesRuns: a run event of this schedule", () => {
    expect(touchesRuns(ev({ change: "run_finished", schedule: "a", run: "r" }), "a")).toBe(true);
    expect(touchesRuns(ev({ change: "updated", schedule: "a" }), "a")).toBe(false);
    expect(touchesRuns(ev({ change: "run_started", schedule: "b" }), "a")).toBe(false);
  });
});

describe("scope filter and groups", () => {
  const list = [sched("b", "webshop"), sched("a", "terrarium"), sched("g", null)];
  it("filters project and global", () => {
    expect(filterSchedules(list, "global").map((s) => s.id)).toEqual(["g"]);
    expect(filterSchedules(list, "project").map((s) => s.id)).toEqual(["b", "a"]);
    expect(filterSchedules(list, "all")).toHaveLength(3);
  });
  it("groups global first, then projects by name", () => {
    expect(groupSchedules(list).map((g) => g.label)).toEqual(["Global", "Project terrarium", "Project webshop"]);
  });
});

describe("Home attention: failed runs in the last 24 h", () => {
  const now = Date.parse("2026-10-26T12:00:00Z");
  it("keeps a schedule whose last run failed in the window", () => {
    const failed = (at: string) => ({ at, status: "failed" as const, summary: "x", run_id: "r" });
    const list = [sched("a", "p", { last_run: failed("2026-10-26T11:00:00Z") }), sched("b", "p", { last_run: failed("2026-10-25T11:00:00Z") }), sched("c", "p", { last_run: { ...failed("2026-10-26T11:00:00Z"), status: "ok" } })];
    expect(failedRecently(list, now).map((s) => s.id)).toEqual(["a"]);
  });
  it("links to the row", () => {
    expect(scheduleHref({ id: "S1", project: "terrarium" })).toBe("/p/terrarium/schedules?open=S1");
    expect(scheduleHref({ id: "S1", project: null })).toBe("/schedules?open=S1");
    expect(scheduleHref({ id: "T1", project: "terrarium", spec: { kind: "on", kinds: ["ci.run.failed"] } })).toBe("/p/terrarium/schedules?open=T1&tab=triggers");
  });
});

describe("run history", () => {
  it("pages newest first and loads more until the end", async () => {
    const api = mock();
    const t = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const h = createRunHistory(api, t.id, 10);
    expect(h.initial()).toBe(true);
    await h.reload();
    expect(h.runs()).toHaveLength(10);
    expect(h.hasMore()).toBe(true);
    await h.loadMore();
    await h.loadMore();
    expect(h.runs()).toHaveLength(25);
    expect(h.hasMore()).toBe(false);
    const times = h.runs().map((r) => r.started_at);
    expect(times).toEqual(times.slice().sort().reverse());
    const bad = h.runs().filter((r) => r.status === "failed")[0]!;
    expect(bad.error).toBe("exit code 2");
    expect(runOutputText(bad.output)).toBe("$ make test  (exit 2)\nrunning 214 tests\ntest mail::loop_guard ... FAILED\n--- stderr\nmake: *** [test] Error 2");
  });

  it("puts a live run on top and replaces it when it ends; ignores other schedules", async () => {
    const api = mock();
    const t = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const h = createRunHistory(api, t.id, 5);
    await h.reload();
    h.apply(run("new", t.id, { status: "running", finished_at: null, started_at: "2099-01-01T00:00:00Z" }));
    expect(h.runs()[0]).toMatchObject({ id: "new", status: "running" });
    h.apply(run("new", t.id, { status: "ok" }));
    expect(h.runs()[0]).toMatchObject({ id: "new", status: "ok" });
    expect(h.runs()).toHaveLength(6);
    h.apply(run("x", "other"));
    expect(h.runs()).toHaveLength(6);
    // The cursor still follows the daemon's pages.
    await h.loadMore();
    expect(h.runs()).toHaveLength(11);
  });

  it("syncHead reads the newest page on top and keeps the pages after it", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleRunMs: 1 });
    const t = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const h = createRunHistory(api, t.id, 10);
    await h.reload();
    await h.loadMore();
    expect(h.runs()).toHaveLength(20);
    const r = await api.schedules.run(t.id);
    await h.syncHead();
    expect(h.runs()).toHaveLength(21);
    expect(h.runs()[0]).toMatchObject({ id: r.id, status: "ok" });
    await h.loadMore();
    expect(h.runs()).toHaveLength(26);
    expect(h.hasMore()).toBe(false);
  });

  it("keeps the error of a failed load", async () => {
    const h = createRunHistory(mock(), "nope");
    await h.reload();
    expect(h.error()).toMatchObject({ status: 404 });
  });

  it("shows a run's output as text: an exec's tails, else the JSON", () => {
    expect(runOutputText({})).toBe("");
    expect(runOutputText(null)).toBe("");
    expect(runOutputText("plain")).toBe("plain");
    expect(runOutputText({ message_id: "M1" })).toBe('{\n  "message_id": "M1"\n}');
    expect(runOutputText({ cmd: ["ls", "-l"], stdout: "a\n", stderr: "" })).toBe("$ ls -l\na");
  });

  it("measures a run", () => {
    expect(runSeconds(run("r", "a"))).toBe(5);
    expect(runSeconds(run("r", "a", { finished_at: null }))).toBeNull();
  });
});

describe("the tabs", () => {
  it("read and write ?tab=triggers; anything else is the time schedules", () => {
    expect(tabOf("triggers")).toBe("trigger");
    expect(tabOf(undefined)).toBe("time");
    expect(tabOf("bogus")).toBe("time");
    expect(tabParam("trigger")).toBe("triggers");
    expect(tabParam("time")).toBeUndefined();
  });

  it("keep a trigger out of the time view, and a time schedule out of the trigger view", () => {
    const trig = { project: "p", spec: { kind: "on" as const, kinds: ["git.push"] } };
    const cron = { project: "p", spec: { kind: "cron" as const, expr: "* * * * *", tz: "UTC" } };
    expect(inView(trig, { project: "p", kind: "trigger" })).toBe(true);
    expect(inView(trig, { project: "p", kind: "time" })).toBe(false);
    expect(inView(cron, { project: null, kind: "trigger" })).toBe(false);
    expect(inView(cron, { project: null })).toBe(true);
  });
});

// @vitest-environment jsdom
// The job-queue strip (TER-178): the merge first-attempt rate, one stats call
// per mount, one debounced call per ended merge job, and a failed call hides
// the strip instead of throwing to the page's error boundary.
import { createComponent, createSignal, ErrorBoundary } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentEvent, JobStats } from "../../api/types";

const stats = vi.fn();
let listeners: ((e: AgentEvent) => void)[] = [];
vi.mock("../../stores", () => ({
  api: { jobs: { stats } },
  events: {
    on: (fn: (e: AgentEvent) => void) => {
      listeners.push(fn);
      return () => (listeners = listeners.filter((l) => l !== fn));
    },
  },
}));

const { JobQueuePanel, JOB_REFRESH_MS } = await import("./JobQueuePanel");

const fa = (passed: number, failed: number) => ({
  ended: passed + failed,
  passed,
  failed,
  pass_rate: passed + failed ? passed / (passed + failed) : null,
});
const body = (over: Partial<JobStats> = {}): JobStats => ({
  project: "terra",
  since: "2026-11-02T00:00:00Z",
  jobs: 10,
  first_attempt: fa(7, 3),
  validate: fa(0, 2),
  merge: fa(7, 1),
  reruns: fa(0, 0),
  conflicts: 1,
  errors: 0,
  cancelled: 0,
  open: 0,
  splits: 0,
  failed_steps: [],
  failed_targets: [],
  untargeted_test_fails: 0,
  ...over,
});

const job = (data: Record<string, unknown>, project = "terra"): AgentEvent =>
  ({ seq: 1, id: "e", at: "", kind: "job", project, summary: "", data }) as AgentEvent;
const emit = (e: AgentEvent) => listeners.forEach((l) => l(e));
const flush = async () => {
  for (let i = 0; i < 5; i++) await Promise.resolve();
};

let dispose: (() => void) | undefined;
let crashed = false;
function mount(project: () => string = () => "terra") {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(
    () =>
      createComponent(ErrorBoundary, {
        fallback: () => ((crashed = true), "crashed"),
        get children() {
          return createComponent(JobQueuePanel, { get project() { return project(); } });
        },
      }),
    el,
  );
  return el;
}

beforeEach(() => {
  vi.useFakeTimers();
  crashed = false;
});
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  listeners = [];
  vi.clearAllMocks();
  vi.useRealTimers();
});

describe("JobQueuePanel", () => {
  it("shows the merge first-attempt rate from one stats call", async () => {
    stats.mockResolvedValue(body());
    const el = mount();
    await flush();
    expect(stats).toHaveBeenCalledTimes(1);
    expect(stats).toHaveBeenCalledWith("terra", "7d");
    const strip = el.querySelector(".job-strip")!;
    expect(strip).not.toBeNull();
    // merge 7 of 8 = 87.5%, ties to even like the CLI: 88%. Not first_attempt (7 of 10).
    expect(strip.querySelector("strong")!.textContent).toBe("88%");
    expect(strip.textContent).toContain("7 of 8 first attempts passed");
    expect(strip.textContent).toContain("1 conflict");
  });

  it("hides again when the new project has no jobs", async () => {
    stats.mockImplementation((project: string) => Promise.resolve(body({ project, jobs: project === "terra" ? 10 : 0 })));
    const [project, setProject] = createSignal("terra");
    const el = mount(project);
    await flush();
    expect(el.querySelector(".job-strip")).not.toBeNull();
    setProject("other");
    await flush();
    expect(stats).toHaveBeenLastCalledWith("other", "7d");
    expect(el.querySelector(".job-strip")).toBeNull();
  });

  it("hides with no job in the window", async () => {
    stats.mockResolvedValue(body({ jobs: 0, merge: fa(0, 0), first_attempt: fa(0, 0), validate: fa(0, 0), conflicts: 0 }));
    const el = mount();
    await flush();
    expect(el.querySelector(".job-strip")).toBeNull();
  });

  it.each([
    ["a 404 from an older daemon", Object.assign(new Error("not found"), { status: 404 })],
    ["a 403", Object.assign(new Error("forbidden"), { status: 403 })],
    ["a network error", new TypeError("Failed to fetch")],
  ])("hides on %s and does not crash the page", async (_, err) => {
    stats.mockRejectedValue(err);
    const el = mount();
    await flush();
    expect(crashed).toBe(false);
    expect(el.textContent).not.toContain("crashed");
    expect(el.querySelector(".job-strip")).toBeNull();
  });

  it("reads again once, debounced, after ended merge jobs only", async () => {
    stats.mockResolvedValue(body());
    const el = mount();
    await flush();
    expect(stats).toHaveBeenCalledTimes(1);

    // Changes that do not end a merge job, and jobs of other projects, read nothing.
    emit(job({ change: "queued", kind: "merge", state: "queued" }));
    emit(job({ change: "started", kind: "merge", state: "running" }));
    emit(job({ change: "requeued", kind: "merge", state: "queued" }));
    emit(job({ change: "ended", kind: "validate", state: "passed" }));
    emit(job({ change: "ended", kind: "merge", state: "passed" }, "other"));
    await vi.advanceTimersByTimeAsync(JOB_REFRESH_MS * 2);
    expect(stats).toHaveBeenCalledTimes(1);

    // A burst of ended merge jobs gives one call after the quiet time.
    stats.mockResolvedValue(body({ merge: fa(8, 1) }));
    emit(job({ change: "ended", kind: "merge", state: "passed" }));
    await vi.advanceTimersByTimeAsync(JOB_REFRESH_MS / 2);
    emit(job({ change: "ended", kind: "merge", state: "failed" }));
    await vi.advanceTimersByTimeAsync(JOB_REFRESH_MS - 1);
    expect(stats).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(stats).toHaveBeenCalledTimes(2);
    expect(el.querySelector(".job-strip strong")!.textContent).toBe("89%");
  });

  it("hides when a refresh fails after the strip was shown", async () => {
    stats.mockResolvedValue(body());
    const el = mount();
    await flush();
    expect(el.querySelector(".job-strip")).not.toBeNull();
    stats.mockRejectedValue(new TypeError("Failed to fetch"));
    emit(job({ change: "ended", kind: "merge", state: "passed" }));
    await vi.advanceTimersByTimeAsync(JOB_REFRESH_MS);
    await flush();
    expect(crashed).toBe(false);
    expect(el.querySelector(".job-strip")).toBeNull();
  });

  it("keeps the strip and its select when the chosen window is empty", async () => {
    stats.mockResolvedValue(body());
    const el = mount();
    await flush();
    stats.mockResolvedValue(body({ jobs: 0, merge: fa(0, 0), conflicts: 0 }));
    const select = el.querySelector("select")!;
    select.value = "24h";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(stats).toHaveBeenLastCalledWith("terra", "24h");
    expect(stats).toHaveBeenCalledTimes(2);
    expect(el.querySelector(".job-strip")).not.toBeNull();
    expect(el.textContent).toContain("no ended first merge attempt in this window");
  });
});

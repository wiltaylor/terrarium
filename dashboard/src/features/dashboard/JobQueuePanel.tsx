// The job-queue strip of the project dashboard (TER-178): the first-attempt
// pass rate of the project's merge jobs over a window (docs/api.md §35.4),
// the number the test-stability theme tracks. Merge jobs only: the queue runs
// merge jobs only since 2026-11-07, and older validate jobs would skew a 30-day
// rate. A first attempt is attempt 1 of a job that ran the recipe: conflicts,
// errors, and split halves are outside the rate and show as quiet counts (the
// API gives those for all kinds together), so an outage or a conflict does not
// move the number.
//
// Load on the daemon (TER-394: one locked SQLite connection serves every
// call): one stats call per mount and per change of the window, and one more,
// debounced, when a merge job of the project ends. Queued, started, and
// requeued jobs do not change the rate, so they read nothing.
//
// The strip hides until an answer shows a job in the window, so a project
// without a queue shows none. Once shown it stays, so an empty window keeps
// its select. A failed call (an older daemon without the route, a 403, the
// network) hides the strip and never reaches the page's error boundary.

import { createEffect, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { JobStats } from "../../api/types";
import { api, events } from "../../stores";
import { JOB_WINDOWS, jobStatNotes, passRateText } from "./model";

/** The wait after the last ended merge job before the strip reads the stats again. */
export const JOB_REFRESH_MS = 1500;

export function JobQueuePanel(props: { project: string }) {
  const [win, setWin] = createSignal<(typeof JOB_WINDOWS)[number]["id"]>("7d");
  const [stats, { refetch }] = createResource(
    () => [props.project, win()] as const,
    ([p, w]): Promise<JobStats | null> => api.jobs.stats(p, w).catch(() => null),
  );
  const [seen, setSeen] = createSignal(false);
  createEffect(() => {
    if ((stats.latest?.jobs ?? 0) > 0) setSeen(true);
  });

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.project !== props.project || e.kind !== "job") return;
      if (e.data?.change !== "ended" || e.data?.kind !== "merge") return;
      clearTimeout(t);
      t = setTimeout(() => void refetch(), JOB_REFRESH_MS);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  const merge = () => stats.latest?.merge;
  return (
    <Show when={seen() && stats.latest}>
      <div class="job-strip" aria-label="Job queue">
        <span class="job-rate">
          <strong>{passRateText(merge()?.pass_rate)}</strong> first-attempt pass rate of merge jobs
        </span>
        <Show when={(merge()?.ended ?? 0) > 0} fallback={<span class="muted">no ended first merge attempt in this window</span>}>
          <span class="muted">
            {merge()!.passed} of {merge()!.ended} first attempts passed
          </span>
        </Show>
        <For each={jobStatNotes(stats.latest)}>{(note) => <span class="muted">{note}</span>}</For>
        <label class="job-window">
          window{" "}
          <select
            value={win()}
            onChange={(e) => setWin(e.currentTarget.value as (typeof JOB_WINDOWS)[number]["id"])}
          >
            <For each={JOB_WINDOWS}>{(w) => <option value={w.id}>{w.label}</option>}</For>
          </select>
        </label>
      </div>
    </Show>
  );
}

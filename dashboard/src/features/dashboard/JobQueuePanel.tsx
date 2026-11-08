// The job-queue strip of the project dashboard (TER-178): the first-attempt
// pass rate of the project's job queue over a window (docs/api.md §35.4),
// the number the test-stability theme tracks. A first attempt is attempt 1
// of a job that ran the recipe: conflicts, errors, and split halves are
// outside the rate and show as quiet counts, so an outage or a conflict does
// not move the number. The strip hides while the queue ran no job in the
// last 30 days, so a project without a queue shows none. A `job` event of
// the project reads the stats again.

import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { api, events } from "../../stores";
import { JOB_WINDOWS, jobStatNotes, passRateText } from "./model";

export function JobQueuePanel(props: { project: string }) {
  const [win, setWin] = createSignal<(typeof JOB_WINDOWS)[number]["id"]>("7d");
  // The 30-day probe decides whether the project has a queue at all; the
  // strip keeps its own window even when that window is empty.
  const [probe, { refetch: refetchProbe }] = createResource(() => props.project, (p) => api.jobs.stats(p, "30d"));
  const [stats, { refetch }] = createResource(() => [props.project, win()] as const, ([p, w]) => api.jobs.stats(p, w));

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.project !== props.project || e.kind !== "job") return;
      clearTimeout(t);
      t = setTimeout(() => (void refetch(), void refetchProbe()), 400);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  const first = () => stats.latest?.first_attempt;
  return (
    <Show when={probe.latest && probe.latest.jobs > 0}>
      <div class="job-strip" aria-label="Job queue">
        <span class="job-rate">
          <strong>{passRateText(first()?.pass_rate)}</strong> first-attempt pass rate
        </span>
        <Show when={(first()?.ended ?? 0) > 0} fallback={<span class="muted">no ended first attempt in this window</span>}>
          <span class="muted">
            {first()!.passed} of {first()!.ended} first attempts passed
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

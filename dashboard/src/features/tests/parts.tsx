// Small parts of the test run views: the stacked counts bar, the status chip,
// the run row, the chip of a linked label, and the list of the runs linked to a
// card (the card drawer). Every status shows as an icon and a word, not
// as a colour only.

import { A } from "@solidjs/router";
import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { CardTests, TestCounts, TestRunStatus, TestRunSummary } from "../../api/types";
import { ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { api, events } from "../../stores";
import { labelHref } from "../board/versionLabels";
import { barSegments, CASE_ICON, chipText, combinedStatus, countsText, latestByName, STATUS_TONE, sumCounts, testDuration } from "./model";

/** The page of a run. */
export const runHref = (project: string, id: string) => `/p/${encodeURIComponent(project)}/board/tests/${encodeURIComponent(id)}`;

/** The chip of a label that a run links to. A version label opens the board filtered by that version. */
export function LabelChip(props: { project: string; label: string }) {
  return (
    <Show when={labelHref(props.project, props.label)} fallback={<span class="chip info">{props.label}</span>}>
      {(href) => (
        <A class="chip info" href={href()} title={`Open the cards of ${props.label}`}>
          {props.label}
        </A>
      )}
    </Show>
  );
}

const RUN_ICON: Record<TestRunStatus, string> = { passed: CASE_ICON.passed, failed: CASE_ICON.failed, empty: CASE_ICON.skipped };

/** The stacked bar of the counts, with the counts as text beside it (or under it with `stacked`). */
export function CountsBar(props: { counts: TestCounts; showText?: boolean; class?: string }) {
  return (
    <span class={`test-counts ${props.class ?? ""}`}>
      <span class="test-bar" role="img" aria-label={countsText(props.counts)} title={countsText(props.counts)}>
        <For each={barSegments(props.counts)}>{(s) => <span class={`seg ${s.status}`} style={{ width: `${s.pct}%` }} />}</For>
      </span>
      <Show when={props.showText !== false}>
        <span class="small">{countsText(props.counts)}</span>
      </Show>
    </span>
  );
}

/** The status of a run as a chip: an icon and the word. */
export function RunStatusChip(props: { status: TestRunStatus; label?: string }) {
  return (
    <span class={`chip ${STATUS_TONE[props.status]}`}>
      {RUN_ICON[props.status]} {props.label ?? props.status}
    </span>
  );
}

/** The chip of the latest test run on a board card. A click opens the run, not the card. */
export function CardTestsChip(props: { project: string; tests: CardTests }) {
  return (
    <A
      href={runHref(props.project, props.tests.id)}
      class={`chip test-chip ${STATUS_TONE[props.tests.status]}`}
      title={`Latest test run "${props.tests.name}": ${countsText(props.tests.counts)}, ${ago(props.tests.created_at)}. Open the run.`}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      {chipText(props.tests)}
    </A>
  );
}

/** One linked run: status, name, counts, time. */
export function RunLine(props: { run: TestRunSummary }) {
  return (
    <div class="row small test-run-line">
      <RunStatusChip status={props.run.status} />
      <A href={runHref(props.run.project, props.run.id)}>{props.run.name}</A>
      <CountsBar counts={props.run.counts} />
      <span class="muted">
        {props.run.format}
        {props.run.duration_ms !== null ? ` · ${testDuration(props.run.duration_ms)}` : ""} · {ago(props.run.created_at)}
      </span>
    </div>
  );
}

/**
 * The test runs linked to a card, newest first, with the
 * combined status of the newest run of each name. It loads again on a
 * `test_run` event of the project.
 */
export function LinkedRuns(props: { project: string; card?: string; title?: string }) {
  const [runs, setRuns] = createSignal<TestRunSummary[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    try {
      const page = await api.testRuns.list(props.project, { card: props.card, limit: 50 });
      if (mine !== gen) return;
      setRuns(page.items);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoaded(true);
    }
  };
  createEffect(
    on(
      () => [props.project, props.card],
      () => {
        setRuns([]);
        setLoaded(false);
        void reload();
      },
    ),
  );
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind !== "test_run" || e.project !== props.project) return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });
  const latest = () => latestByName(runs());
  const status = () => combinedStatus(latest());
  return (
    <section class="stack linked-runs" style={{ gap: "6px" }}>
      <div class="row" style={{ gap: "8px" }}>
        <h3 style={{ margin: 0 }}>{props.title ?? "Tests"}</h3>
        <Show when={status()}>
          {(s) => <RunStatusChip status={s()} label={`${s()} (${countsText(sumCounts(latest()))}${latest().length > 1 ? `, latest of ${latest().length} names` : ""})`} />}
        </Show>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <For each={runs()} fallback={<p class="muted small" style={{ margin: 0 }}>{loaded() ? "No test runs are linked. An agent links one with `terrarium test push`." : "Loading…"}</p>}>
        {(r) => <RunLine run={r} />}
      </For>
    </section>
  );
}

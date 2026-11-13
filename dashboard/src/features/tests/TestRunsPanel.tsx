import { useInstance } from "../../instance";
// The Tests tab of the project (app/nav.ts PROJECT_TABS): the test runs of
// the project, newest first (docs/api.md, test runs). Agents push them with
// `terrarium test push`. The filters are in the URL (`?name=&status=&card=&label=`),
// so a filtered list has a link. A row opens the run (/p/:project/tests/:id).
// A run links to labels, such as the version label `version:1.4.0` (TER-58).

import { A } from "../../instance/navigation";
import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { TestRunListQuery, TestRunStatus, TestRunSummary } from "../../api/types";
import { Empty, ErrorBanner, LoadMore, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";

import { shortCommit, testDuration } from "./model";
import { CountsBar, LabelChip, runHref, RunStatusChip } from "./parts";

const STATUSES: TestRunStatus[] = ["failed", "passed", "empty"];

export type TestRunsQuery = { name?: string; status?: string; card?: string; label?: string };
type TestRunsPanelProps = { project: string; query: TestRunsQuery; onQueryChange: (patch: TestRunsQuery) => void };
export function TestRunsPanel(props: TestRunsPanelProps) {
  return <Show when={props.project} keyed>{(project) => <TestRunsContent {...props} project={project} />}</Show>;
}

function TestRunsContent(props: TestRunsPanelProps) {
  const { api, events } = useInstance();
  const setSearch = (patch: TestRunsQuery) => props.onQueryChange(patch);
  const [runs, setRuns] = createSignal<TestRunSummary[]>([]);
  const [next, setNext] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // The name filter is a text box: it applies on Enter or on blur, not on each key.
  const [nameDraft, setNameDraft] = createSignal(props.query.name ?? "");

  const query = (): TestRunListQuery => ({
    name: props.query.name || undefined,
    status: STATUSES.includes(props.query.status as TestRunStatus) ? (props.query.status as TestRunStatus) : undefined,
    card: props.query.card || undefined,
    label: props.query.label || undefined,
    limit: 50,
  });

  let gen = 0;
  const load = async (more: boolean) => {
    if (more && loading()) return;
    const mine = ++gen;
    setLoading(true);
    try {
      const page = await api.testRuns.list(props.project, { ...query(), cursor: more ? (next() ?? undefined) : undefined });
      if (mine !== gen) return;
      setRuns((xs) => (more ? [...xs, ...page.items.filter((r) => !xs.some((x) => x.id === r.id))] : page.items));
      setNext(page.next);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) {
        setLoading(false);
        setLoaded(true);
      }
    }
  };
  createEffect(
    on(
      () => [props.project, props.query.name, props.query.status, props.query.card, props.query.label],
      () => {
        setNameDraft(props.query.name ?? "");
        void load(false);
      },
    ),
  );
  onCleanup(() => { ++gen; });
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind !== "test_run" || e.project !== props.project) return;
      clearTimeout(t);
      t = setTimeout(() => void load(false), 300);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  const filtered = () => !!(props.query.name || props.query.status || props.query.card || props.query.label);

  return (
    <>
      <PageHead title="Tests" level={2}>
        <span class="muted small">
          Agents push standard test result files with <code>terrarium test push</code>.
        </span>
      </PageHead>
      <form
        class="row test-filters"
        onSubmit={(e) => {
          e.preventDefault();
          setSearch({ name: nameDraft().trim() || undefined });
        }}
      >
        <label class="small">
          Name
          <input value={nameDraft()} onInput={(e) => setNameDraft(e.currentTarget.value)} onBlur={() => setSearch({ name: nameDraft().trim() || undefined })} placeholder="unit" aria-label="Run name" />
        </label>
        <label class="small">
          Status
          <select value={props.query.status ?? ""} onChange={(e) => setSearch({ status: e.currentTarget.value || undefined })} aria-label="Run status">
            <option value="">Any</option>
            <For each={STATUSES}>{(s) => <option value={s}>{s}</option>}</For>
          </select>
        </label>
        <label class="small">
          Card
          <input value={props.query.card ?? ""} onChange={(e) => setSearch({ card: e.currentTarget.value.trim().toUpperCase() || undefined })} placeholder="TER-12" aria-label="Card ID" />
        </label>
        <label class="small">
          Label
          <input value={props.query.label ?? ""} onChange={(e) => setSearch({ label: e.currentTarget.value.trim() || undefined })} placeholder="version:vnext" aria-label="Label" />
        </label>
        <Show when={filtered()}>
          <button type="button" class="small" onClick={() => setSearch({ name: undefined, status: undefined, card: undefined, label: undefined })}>
            Clear filters
          </button>
        </Show>
      </form>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="test-runs" role="list" aria-label="Test runs">
        <For each={runs()} fallback={<Empty>{loaded() ? (filtered() ? "No test run matches the filters." : "No test runs yet. An agent pushes one with `terrarium test push <file> --card <id>`.") : "Loading…"}</Empty>}>
          {(r) => <RunRow run={r} />}
        </For>
      </div>
      <LoadMore hasMore={!!next()} loading={loading()} onMore={() => void load(true)} />
    </>
  );
}

function RunRow(props: { run: TestRunSummary }) {
  const r = () => props.run;
  const p = () => encodeURIComponent(r().project);
  return (
    <div class="test-run-row card-box" role="listitem">
      <div class="test-run-main">
        <div class="row" style={{ gap: "8px" }}>
          <RunStatusChip status={r().status} />
          <A href={runHref(r().project, r().id)} class="test-run-name">
            {r().name}
          </A>
          <span class="muted small">
            {r().format}
            {r().tool ? ` · ${r().tool}` : ""}
          </span>
        </div>
        <div class="muted small">
          {r().created_by_name || r().created_by}, <span title={r().created_at}>{ago(r().created_at)}</span>
          <Show when={r().commit || r().branch}>
            {" · "}
            <span class="mono">{[r().branch, shortCommit(r().commit)].filter(Boolean).join(" @ ")}</span>
          </Show>
          <Show when={r().duration_ms !== null}> · {testDuration(r().duration_ms)}</Show>
        </div>
      </div>
      <CountsBar counts={r().counts} class="test-run-counts" />
      <div class="row small test-run-links">
        <For each={r().cards}>
          {(c) => (
            <A class="chip" href={`/p/${p()}/board?card=${encodeURIComponent(c)}`} title={`Open card ${c}`}>
              {c}
            </A>
          )}
        </For>
        <For each={r().labels}>{(l) => <LabelChip project={r().project} label={l} />}</For>
      </div>
    </div>
  );
}

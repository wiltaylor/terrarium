import { InstanceAnchor } from "../../instance/navigation";
import { useInstance } from "../../instance";
// The Pipelines page of a project (/p/:project/pipelines, docs/api.md §31):
// each CI/CD pipeline with the status of its newest run, its last poll, an
// on/off switch, Refresh, Edit, and Delete. A row opens the recent runs; a
// run opens its detail with the steps (the jobs of a GitHub run, the steps
// that a script gives). The dialog makes or changes a pipeline: GitHub
// Actions, or a script on a target that prints the runs as JSON, with a
// Test poll before a save. The page loads again on each `pipeline.*` event
// of the project.

import { useParams } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { Pipeline, PipelineRun, PipelineRunDetail, PipelineTestReport } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, Modal, PageHead, useNow } from "../../components/ui";
import { ago } from "../../lib/format";
import { everyWords, parseEvery } from "../../lib/schedule";

import { defOf, formOf, parseEnvSecrets, pipelineState, runDuration, sourceText, STATUS_META, stateCounts, type PipelineForm, type PipelineState } from "./model";

/** A link to the CI system: http and https only. */
const safeHref = (u: string | null | undefined) => (u && /^https?:\/\//i.test(u) ? u : undefined);

export function StatusBadge(props: { state: PipelineState }) {
  const meta = () => STATUS_META[props.state];
  return (
    <span class={`mon-badge ${meta().cls}`}>
      <span aria-hidden="true" class="mon-glyph">
        {meta().glyph}
      </span>
      {meta().label}
    </span>
  );
}

function ExtLink(props: { href: string | null | undefined; children: string }) {
  return (
    <Show when={safeHref(props.href)} fallback={<span>{props.children}</span>}>
      {(h) => (
        <InstanceAnchor href={h()} target="_blank" rel="noopener noreferrer">
          {props.children}
        </InstanceAnchor
      )}
    </Show>
  );
}

/** The place of a run: its environment, else its branch and commit. */
const runWhere = (r: Pick<PipelineRun, "environment" | "branch" | "commit">) => r.environment ?? [r.branch, r.commit?.slice(0, 8)].filter(Boolean).join(" · ");

export function RunDetail(props: { pipeline: string; run: string; onClose: () => void }) {
  const { api } = useInstance();
  const now = useNow(5_000);
  const [d] = createResource(
    () => [props.pipeline, props.run] as const,
    ([p, r]) => api.pipelines.run(p, r),
  );
  return (
    <Modal title={`Run ${d()?.name ?? props.run}`} onClose={() => props.onClose()} class="modal-wide">
      <ErrorBanner error={d.error} />
      <Show when={d()} fallback={<p class="muted">Loading…</p>}>
        {(r) => <RunDetailBody run={r()} now={now()} />}
      </Show>
    </Modal>
  );
}

function RunDetailBody(props: { run: PipelineRunDetail; now: number }) {
  const r = () => props.run;
  const facts = () =>
    (
      [
        ["Run ID", r().id],
        ["Environment", r().environment],
        ["Branch", r().branch],
        ["Commit", r().commit],
        ["Actor", r().actor],
        ["Event", r().event],
        ["Started", r().started_at ? new Date(r().started_at!).toLocaleString() : null],
        ["Finished", r().finished_at ? new Date(r().finished_at!).toLocaleString() : null],
        ["Duration", runDuration(r(), props.now)],
      ] as const
    ).filter(([, v]) => v);
  return (
    <div class="stack">
      <div class="row" style={{ "flex-wrap": "wrap", gap: "8px" }}>
        <StatusBadge state={r().status} />
        <Show when={safeHref(r().url)}>
          {(h) => (
            <InstanceAnchor href={h()} target="_blank" rel="noopener noreferrer">
              Open in {r().adapter === "github" ? "GitHub" : "the CI system"} ↗
            </InstanceAnchor
          )}
        </Show>
      </div>
      <Show when={r().summary}>{(s) => <p style={{ margin: 0 }}>{s()}</p>}</Show>
      <dl class="pl-facts">
        <For each={facts()}>
          {([k, v]) => (
            <>
              <dt>{k}</dt>
              <dd class={k === "Commit" || k === "Run ID" ? "mono" : ""}>{v}</dd>
            </>
          )}
        </For>
      </dl>
      <h3 class="small" style={{ margin: "8px 0 0" }}>
        Steps
      </h3>
      <Show when={r().steps_error}>{(e) => <div class="small mon-err">The steps did not load: {e()}</div>}</Show>
      <Show when={r().steps?.length} fallback={<p class="muted small">{r().steps_error ? "" : "This run has no steps."}</p>}>
        <div class="table-wrap">
          <table class="mon-table">
            <thead>
              <tr>
                <th scope="col">Step</th>
                <th scope="col">Status</th>
                <th scope="col">Duration</th>
              </tr>
            </thead>
            <tbody>
              <For each={r().steps!}>
                {(s) => (
                  <tr>
                    <td>
                      <ExtLink href={s.url}>{s.name}</ExtLink>
                      <Show when={s.summary}>
                        <div class="muted small">{s.summary}</div>
                      </Show>
                    </td>
                    <td>
                      <StatusBadge state={s.status} />
                    </td>
                    <td class="small">{runDuration({ started_at: s.started_at ?? null, finished_at: s.finished_at ?? null, status: s.status }, props.now)}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
    </div>
  );
}

function RunsTable(props: { pipeline: Pipeline; version: string | null; now: number; onOpen: (run: string) => void }) {
  const { api } = useInstance();
  const [runs] = createResource(
    () => [props.pipeline.id, props.version] as const,
    ([id]) => api.pipelines.runs(id, 50),
  );
  return (
    <section aria-label={`Runs of ${props.pipeline.name}`} class="pl-runs">
      <ErrorBanner error={runs.error} />
      <Show when={runs()?.length} fallback={<p class="muted small">{runs.loading ? "Loading…" : "No runs yet."}</p>}>
        <table class="mon-table">
          <thead>
            <tr>
              <th scope="col">Run</th>
              <th scope="col">Status</th>
              <th scope="col">Where</th>
              <th scope="col">Started</th>
              <th scope="col">Duration</th>
            </tr>
          </thead>
          <tbody>
            <For each={runs()!}>
              {(r) => (
                <tr>
                  <td>
                    <button class="link" onClick={() => props.onOpen(r.id)}>
                      {r.name ?? r.id}
                    </button>
                    <Show when={r.summary}>
                      <div class="muted small">{r.summary}</div>
                    </Show>
                  </td>
                  <td>
                    <StatusBadge state={r.status} />
                  </td>
                  <td class="small">{runWhere(r) || "–"}</td>
                  <td class="small">{r.started_at ? ago(r.started_at, props.now) : "not started"}</td>
                  <td class="small">{runDuration(r, props.now)}</td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </Show>
    </section>
  );
}

export function PipelineDialog(props: { project: string; pipeline?: Pipeline; onClose: () => void; onSaved: (p: Pipeline) => void }) {
  const { api } = useInstance();
  const [f, setF] = createSignal<PipelineForm>(formOf(props.pipeline));
  const set = <K extends keyof PipelineForm>(k: K, v: PipelineForm[K]) => setF((x) => ({ ...x, [k]: v }));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const [test, setTest] = createSignal<PipelineTestReport | null>(null);
  const [targets] = createResource(async () => (await api.targets.list()).filter((t) => (!t.project || t.project === props.project) && t.kind !== "vnc"));
  const envError = () => {
    const r = parseEnvSecrets(f().envSecrets);
    return "error" in r ? r.error : null;
  };
  const when = () => {
    const s = parseEvery(f().every);
    if (s === null) return "Not an interval: use 30s, 1m, 5m, or 1h.";
    if (s < 30) return "The shortest interval is 30 s.";
    return `Polls every ${everyWords(s)}.`;
  };
  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };
  const runTest = () =>
    run(async () => {
      setTest(null);
      const { name: _n, ...d } = defOf(f());
      setTest(await api.pipelines.test(props.project, d));
    });
  const save = (e: Event) => {
    e.preventDefault();
    if (envError()) return;
    void run(async () => {
      const d = defOf(f());
      props.onSaved(props.pipeline ? await api.pipelines.update(props.pipeline.id, d) : await api.pipelines.create(props.project, d));
    });
  };
  return (
    <Modal title={props.pipeline ? `Edit pipeline ${props.pipeline.name}` : "New pipeline"} onClose={() => props.onClose()} class="modal-wide modal-full">
      <form class="stack mon-form" onSubmit={save}>
        <div class="mon-form-grid">
          <label>
            Name
            <input required value={f().name} onInput={(e) => set("name", e.currentTarget.value)} placeholder="ci" pattern="[a-z0-9][a-z0-9_\-]{0,62}" title="1-63 of a-z, 0-9, - and _" />
          </label>
          <label>
            Source
            <select value={f().kind} onChange={(e) => set("kind", e.currentTarget.value as PipelineForm["kind"])}>
              <option value="github">GitHub Actions</option>
              <option value="script">Script (any other system)</option>
            </select>
          </label>
          <label class="span-2">
            Description
            <input value={f().description} onInput={(e) => set("description", e.currentTarget.value)} placeholder="What this pipeline builds or deploys" />
          </label>
        </div>
        <Show
          when={f().kind === "github"}
          fallback={
            <>
              <div class="mon-form-grid">
                <label>
                  Target
                  <select required value={f().target} onChange={(e) => set("target", e.currentTarget.value)}>
                    <option value="">Pick a target</option>
                    <For each={targets() ?? []}>
                      {(t) => (
                        <option value={t.name} selected={t.name === f().target}>
                          {t.name} ({t.kind}
                          {t.project ? `, ${t.project}` : ", global"})
                        </option>
                      )}
                    </For>
                  </select>
                </label>
                <label>
                  Timeout (s)
                  <input type="number" min="1" max="600" value={f().timeout} onInput={(e) => set("timeout", e.currentTarget.value)} />
                </label>
              </div>
              <label>
                Script <span class="muted small">(runs with sh -c on the target)</span>
                <textarea class="mono mon-script" rows={6} spellcheck={false} required value={f().script} onInput={(e) => set("script", e.currentTarget.value)} placeholder="python3 ~/bin/octopus-runs.py" />
              </label>
              <p class="muted small" style={{ margin: 0 }}>
                The script prints the runs as JSON, newest first: <code>{'{"runs": [{"id", "status", "name", "url", "environment", "started_at", "steps": [{"name", "status"}]}]}'}</code>. Status: queued, running, waiting, success, failed, cancelled,
                skipped, or unknown (Octopus words such as Executing and Canceled are fine).
              </p>
              <label>
                Secrets in the environment <span class="muted small">(VAR=secret, one per line)</span>
                <textarea class="mono" rows={2} spellcheck={false} value={f().envSecrets} onInput={(e) => set("envSecrets", e.currentTarget.value)} placeholder="OCTOPUS_API_KEY=octopus-key" />
              </label>
              <Show when={envError()}>{(e) => <div class="small mon-err">{e()}</div>}</Show>
            </>
          }
        >
          <div class="mon-form-grid">
            <label>
              Repo
              <input required value={f().repo} onInput={(e) => set("repo", e.currentTarget.value)} placeholder="owner/name" />
            </label>
            <label>
              Workflow <span class="muted small">(optional)</span>
              <input value={f().workflow} onInput={(e) => set("workflow", e.currentTarget.value)} placeholder="ci.yml" />
            </label>
            <label>
              Branch <span class="muted small">(optional)</span>
              <input value={f().branch} onInput={(e) => set("branch", e.currentTarget.value)} placeholder="main" />
            </label>
            <label>
              Token secret <span class="muted small">(optional)</span>
              <input value={f().tokenSecret} onInput={(e) => set("tokenSecret", e.currentTarget.value)} placeholder="github-token" />
            </label>
            <label class="span-2">
              API URL <span class="muted small">(GitHub Enterprise Server only)</span>
              <input value={f().apiUrl} onInput={(e) => set("apiUrl", e.currentTarget.value)} placeholder="https://api.github.com" />
            </label>
          </div>
          <p class="muted small" style={{ margin: 0 }}>
            Without a token secret, GitHub gives public repos only, and 60 requests an hour.
          </p>
        </Show>
        <div class="mon-form-grid">
          <label>
            Poll every
            <input value={f().every} onInput={(e) => set("every", e.currentTarget.value)} placeholder="5m" />
          </label>
          <label>
            Keep runs
            <input type="number" min="1" max="100" value={f().maxRuns} onInput={(e) => set("maxRuns", e.currentTarget.value)} />
          </label>
        </div>
        <p class="muted small">{when()}</p>
        <label class="row small">
          <input type="checkbox" checked={f().enabled} onChange={(e) => set("enabled", e.currentTarget.checked)} /> On: poll on the schedule
        </label>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={test()}>{(t) => <TestReport report={t()} />}</Show>
        <div class="row" style={{ "justify-content": "flex-end", "flex-wrap": "wrap" }}>
          <button type="button" disabled={busy() || !!envError()} onClick={() => void runTest()}>
            {busy() ? "Working…" : "Test poll"}
          </button>
          <button type="button" class="ghost" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button type="submit" class="primary" disabled={busy() || !!envError()}>
            {props.pipeline ? "Save" : "Create pipeline"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** The runs or the error of a test poll, and the raw output of a script. */
function TestReport(props: { report: PipelineTestReport }) {
  const r = () => props.report;
  return (
    <section class="mon-report" aria-label="Test poll (not saved)">
      <div class="spread">
        <strong class="small">Test poll (not saved)</strong>
        <span class="muted small">{r().duration_ms} ms</span>
      </div>
      <Show when={r().error} fallback={<div class="small">{r().runs.length} runs</div>}>
        {(e) => <div class="small mon-err">Error: {e()}</div>}
      </Show>
      <For each={r().runs.slice(0, 10)}>
        {(x) => (
          <div class="row small" style={{ gap: "8px" }}>
            <StatusBadge state={x.status} />
            <span>{x.name ?? x.id}</span>
            <span class="muted">{runWhere(x)}</span>
          </div>
        )}
      </For>
      <Show when={r().output}>
        {(o) => (
          <>
            <div class="small">Exit code {o().exit_code ?? "none"}</div>
            <div class="small muted">stdout</div>
            <pre class="dash-text">{o().stdout || "(empty)"}</pre>
            <Show when={o().stderr}>
              <div class="small muted">stderr</div>
              <pre class="dash-text">{o().stderr}</pre>
            </Show>
          </>
        )}
      </Show>
    </section>
  );
}

export default function PipelinesPage() {
  const { api, events } = useInstance();
  const params = useParams<{ project: string }>();
  const now = useNow(5_000);
  const [error, setError] = createSignal<unknown>(null);
  const [list, { refetch }] = createResource(
    () => params.project,
    (p) => api.pipelines.list(p),
  );
  const [open, setOpen] = createSignal<string | null>(null);
  const [detail, setDetail] = createSignal<{ pipeline: string; run: string } | null>(null);
  const [editing, setEditing] = createSignal<Pipeline | "new" | null>(null);
  const [refreshing, setRefreshing] = createSignal<string | null>(null);

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.project !== params.project || !e.kind.startsWith("pipeline.")) return;
      clearTimeout(t);
      t = setTimeout(() => void refetch(), 400);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      await fn();
      await refetch();
    } catch (e) {
      setError(e);
    }
  };
  const refresh = (p: Pipeline) =>
    act(async () => {
      setRefreshing(p.id);
      try {
        await api.pipelines.refresh(p.id);
      } finally {
        setRefreshing(null);
      }
    });

  const summary = createMemo(() => {
    const c = stateCounts(list.latest ?? []);
    const order: PipelineState[] = ["failed", "error", "waiting", "running", "queued", "success", "cancelled", "skipped", "unknown", "pending", "off"];
    return order.filter((s) => (c[s] ?? 0) > 0).map((s) => ({ s, n: c[s]! }));
  });

  return (
    <>
      <PageHead title="Pipelines" level={2}>
        <button class="primary" onClick={() => setEditing("new")}>
          New pipeline
        </button>
      </PageHead>
      <p class="muted small">
        The CI/CD runs of the project, polled on a schedule: GitHub Actions, or a script on a target for any other system (Octopus Deploy, Jenkins, GitLab). Agents add them with <code>terrarium pipeline new</code>.
      </p>
      <Show when={summary().length}>
        <div class="row" style={{ "flex-wrap": "wrap", gap: "8px", "margin-bottom": "8px" }} aria-label="Pipelines by state">
          <For each={summary()}>
            {(x) => (
              <span class="row small" style={{ gap: "4px" }}>
                <StatusBadge state={x.s} /> × {x.n}
              </span>
            )}
          </For>
        </div>
      </Show>
      <ErrorBanner error={error() ?? list.error} onDismiss={() => setError(null)} />
      <Show when={list.latest?.length} fallback={<Empty>{list.loading ? "Loading…" : "No pipelines yet. Make one to see the runs of a CI/CD system here."}</Empty>}>
        <div class="table-wrap">
          <table class="mon-table pl-table">
            <thead>
              <tr>
                <th scope="col">Pipeline</th>
                <th scope="col">State</th>
                <th scope="col">Newest run</th>
                <th scope="col">Last poll</th>
                <th scope="col">On</th>
                <th scope="col">
                  <span class="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              <For each={list.latest!}>
                {(p) => (
                  <>
                    <tr>
                      <td>
                        <button class="link" aria-expanded={open() === p.id} onClick={() => setOpen(open() === p.id ? null : p.id)} style={{ "font-weight": 600 }}>
                          {open() === p.id ? "▾" : "▸"} {p.name}
                        </button>
                        <div class="muted small">
                          {sourceText(p.source)}
                          <Show when={p.description}> · {p.description}</Show>
                        </div>
                      </td>
                      <td>
                        <StatusBadge state={pipelineState(p)} />
                        <Show when={p.error}>{(e) => <div class="small mon-err pl-err">{e()}</div>}</Show>
                      </td>
                      <td>
                        <Show when={p.latest} fallback={<span class="muted small">–</span>}>
                          {(r) => (
                            <>
                              <button class="link" onClick={() => setDetail({ pipeline: p.id, run: r().id })}>
                                {r().name ?? r().id}
                              </button>
                              <div class="muted small">
                                {[runWhere(r()), r().started_at ? ago(r().started_at!, now()) : null, runDuration(r(), now()) !== "–" ? runDuration(r(), now()) : null].filter(Boolean).join(" · ")}
                              </div>
                            </>
                          )}
                        </Show>
                      </td>
                      <td class="small" data-label="Last poll">
                        {p.running ? "polling now" : p.polled_at ? ago(p.polled_at, now()) : "never"}
                      </td>
                      <td data-label="On">
                        <label class="switch">
                          <input type="checkbox" checked={p.enabled} aria-label={`Poll ${p.name} on its schedule`} onChange={(e) => void act(() => api.pipelines.update(p.id, { enabled: e.currentTarget.checked }))} />
                        </label>
                      </td>
                      <td>
                        <div class="row mon-actions">
                          <button class="small" disabled={refreshing() === p.id || p.running} onClick={() => void refresh(p)}>
                            {refreshing() === p.id ? "Polling…" : "Refresh"}
                          </button>
                          <button class="small" onClick={() => setEditing(p)}>
                            Edit
                          </button>
                          <ConfirmButton class="small danger" label="Delete" onConfirm={() => act(() => api.pipelines.remove(p.id))} />
                        </div>
                      </td>
                    </tr>
                    <Show when={open() === p.id}>
                      <tr>
                        <td colSpan={6}>
                          <RunsTable pipeline={p} version={p.polled_at} now={now()} onOpen={(run) => setDetail({ pipeline: p.id, run })} />
                        </td>
                      </tr>
                    </Show>
                  </>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={detail()}>{(d) => <RunDetail pipeline={d().pipeline} run={d().run} onClose={() => setDetail(null)} />}</Show>
      <Show when={editing()}>
        {(e) => (
          <PipelineDialog
            project={params.project}
            pipeline={e() === "new" ? undefined : (e() as Pipeline)}
            onClose={() => setEditing(null)}
            onSaved={() => {
              setEditing(null);
              void refetch();
            }}
          />
        )}
      </Show>
    </>
  );
}

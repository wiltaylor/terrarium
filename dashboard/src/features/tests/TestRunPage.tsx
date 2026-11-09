// The page of one test run (/p/:project/board/tests/:id, docs/api.md, test
// runs): the summary, the compare with the previous run of the same name, the
// suites with their cases, and the output of a case. The output is plain text in
// <pre> text nodes, with no ANSI codes (model.ts stripAnsi); it never renders as
// HTML. "Failed only" is on at first when the run has failures. The daemon
// filters and pages the cases (`?status=failed&q=&cursor=`).

import { A, useNavigate, useParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import type { TestCase, TestRun } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, LoadMore, PageHead } from "../../components/ui";
import { ago, bytes } from "../../lib/format";
import { projectBase } from "../../lib/url";
import { api } from "../../stores";
import { CASE_ICON, caseDetails, compareSections, countsText, defaultFailedOnly, failing, groupBySuite, shortCommit, STATUS_TONE, testDuration } from "./model";
import { CountsBar, runHref, RunStatusChip } from "./parts";
import { labelHref } from "../board/versionLabels";

export default function TestRunPage() {
  const params = useParams<{ project: string; id: string }>();
  const navigate = useNavigate();
  const [run, setRun] = createSignal<TestRun | null>(null);
  const [cases, setCases] = createSignal<TestCase[]>([]);
  const [next, setNext] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // null: not known yet; the first answer decides (defaultFailedOnly).
  const [failedOnly, setFailedOnly] = createSignal<boolean | null>(null);
  const [q, setQ] = createSignal("");
  const [qApplied, setQApplied] = createSignal("");
  const [comparing, setComparing] = createSignal(false);
  const [open, setOpen] = createSignal<Set<number>>(new Set());
  const [linkCard, setLinkCard] = createSignal("");

  let gen = 0;
  const load = async (more: boolean) => {
    const mine = ++gen;
    setLoading(true);
    try {
      const first = failedOnly() === null;
      let r = await api.testRuns.get(params.id, { status: first || failedOnly() ? "failed" : undefined, q: qApplied() || undefined, cursor: more ? (next() ?? undefined) : undefined });
      if (mine !== gen) return;
      if (first) {
        const on = defaultFailedOnly(r);
        setFailedOnly(on);
        if (!on) {
          r = await api.testRuns.get(params.id, { q: qApplied() || undefined });
          if (mine !== gen) return;
        }
      }
      setRun(r);
      setCases((xs) => (more ? [...xs, ...r.cases.filter((c) => !xs.some((x) => x.seq === c.seq))] : r.cases));
      setNext(r.next_cursor);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(
    on(
      () => params.id,
      () => {
        setRun(null);
        setCases([]);
        setFailedOnly(null);
        setComparing(false);
        setOpen(new Set<number>());
        setQ("");
        setQApplied("");
        void load(false);
      },
    ),
  );
  // A change of the filter loads the first page again.
  createEffect(on([failedOnly, qApplied], (_v, prev) => prev && prev[0] !== null && run() && void load(false), { defer: true }));
  // The search applies 300 ms after the last key.
  createEffect(
    on(q, (v) => {
      const t = setTimeout(() => setQApplied(v.trim()), 300);
      onCleanup(() => clearTimeout(t));
    }, { defer: true }),
  );

  const [compare] = createResource(
    () => (comparing() ? params.id : null),
    (id) => api.testRuns.compare(id, "previous"),
  );

  const toggle = (seq: number) =>
    setOpen((s) => {
      const n = new Set(s);
      if (n.has(seq)) n.delete(seq);
      else n.add(seq);
      return n;
    });

  const download = async () => {
    try {
      const { blob, fileName } = await api.testRuns.raw(params.id);
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = fileName;
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 10_000);
    } catch (e) {
      setError(e);
    }
  };

  const links = async (patch: Parameters<typeof api.testRuns.links>[1]) => {
    try {
      const s = await api.testRuns.links(params.id, patch);
      setRun((r) => (r ? { ...r, cards: s.cards, labels: s.labels } : r));
      return true;
    } catch (e) {
      setError(e);
      return false;
    }
  };

  const p = () => projectBase(params.project);

  return (
    <>
      <div class="small">
        <A href={`${p()}/board/tests`}>← All test runs</A>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={run()} fallback={<Show when={!error()}><Empty>Loading…</Empty></Show>}>
        {(r) => (
          <div class="stack test-run-page">
            <PageHead
              level={2}
              title={
                <span class="row" style={{ gap: "8px" }}>
                  {r().name} <RunStatusChip status={r().status} />
                </span>
              }
            >
              <button onClick={() => void download()} title={`${r().file_name}, ${bytes(r().raw_size)}`}>
                Download original
              </button>
              <ConfirmButton label="Delete" onConfirm={async () => {
                try {
                  await api.testRuns.remove(r().id);
                  navigate(`/p/${p()}/board/tests`);
                } catch (e) {
                  setError(e);
                }
              }} />
            </PageHead>

            <div class="test-totals" aria-label={countsText(r().counts)}>
              <Total n={r().counts.passed} label="passed" status="passed" />
              <Total n={r().counts.failed} label="failed" status="failed" />
              <Total n={r().counts.errored} label="errored" status="errored" />
              <Total n={r().counts.skipped} label="skipped" status="skipped" />
              <div class="test-total">
                <div class="test-total-n">{r().duration_ms !== null ? testDuration(r().duration_ms) : "–"}</div>
                <div class="small muted">duration</div>
              </div>
            </div>
            <CountsBar counts={r().counts} showText={false} class="wide" />

            <dl class="kv small test-meta">
              <dt>Format</dt>
              <dd>
                {r().format}
                {r().tool ? ` · ${r().tool}` : ""} · <span class="mono">{r().file_name}</span> ({bytes(r().raw_size)})
              </dd>
              <Show when={r().commit || r().branch}>
                <dt>Commit</dt>
                <dd class="mono">{[r().branch, shortCommit(r().commit)].filter(Boolean).join(" @ ")}</dd>
              </Show>
              <dt>Pushed</dt>
              <dd>
                {r().created_by_name || r().created_by} · <span title={r().created_at}>{ago(r().created_at)}</span>
              </dd>
              <dt>Linked</dt>
              <dd>
                <div class="row" style={{ gap: "4px", "flex-wrap": "wrap" }}>
                  <For each={r().cards}>
                    {(c) => (
                      <span class="chip">
                        <A href={`${p()}/board?card=${encodeURIComponent(c)}`}>{c}</A>
                        <button type="button" class="ghost small" aria-label={`Unlink ${c}`} title={`Unlink ${c}`} onClick={() => void links({ remove_cards: [c] })}>
                          ×
                        </button>
                      </span>
                    )}
                  </For>
                  <For each={r().labels}>
                    {(l) => (
                      <span class="chip info">
                        <Show when={labelHref(r().project, l)} fallback={l}>
                          {(href) => <A href={href()}>{l}</A>}
                        </Show>
                        <button type="button" class="ghost small" aria-label={`Unlink ${l}`} title={`Unlink ${l}`} onClick={() => void links({ remove_labels: [l] })}>
                          ×
                        </button>
                      </span>
                    )}
                  </For>
                  <form
                    class="row"
                    style={{ gap: "4px" }}
                    onSubmit={async (e) => {
                      e.preventDefault();
                      const id = linkCard().trim().toUpperCase();
                      if (id && (await links({ add_cards: [id] }))) setLinkCard("");
                    }}
                  >
                    <input class="small" style={{ width: "8em" }} value={linkCard()} onInput={(e) => setLinkCard(e.currentTarget.value)} placeholder="TER-12" aria-label="Card to link" />
                    <button class="small" type="submit" disabled={!linkCard().trim()}>
                      Link card
                    </button>
                  </form>
                </div>
              </dd>
              <Show when={r().notes.length}>
                <dt>Notes</dt>
                <dd>
                  <For each={r().notes}>{(n) => <div>{n}</div>}</For>
                </dd>
              </Show>
            </dl>

            <section class="card-box stack" style={{ gap: "6px" }}>
              <label class="row small">
                <input type="checkbox" checked={comparing()} onChange={(e) => setComparing(e.currentTarget.checked)} /> Compare with the previous run of "{r().name}"
              </label>
              <Show when={comparing()}>
                <Show when={compare()} fallback={<span class="muted small">{compare.error ? "The compare failed." : "Loading…"}</span>}>
                  {(c) => (
                    <Show when={c().base} fallback={<span class="muted small">There is no older run named "{r().name}" in this project.</span>}>
                      {(b) => (
                        <div class="stack" style={{ gap: "6px" }}>
                          <div class="small muted">
                            Against <A href={runHref(b().project, b().id)}>{ago(b().created_at)}</A> ({countsText(b().counts)}
                            {b().commit ? `, ${shortCommit(b().commit)}` : ""})
                          </div>
                          <For each={compareSections(c())}>
                            {(s) => (
                              <details class="test-compare" open={s.open && s.count > 0}>
                                <summary>
                                  <span class={`chip ${s.tone}`}>
                                    {s.title}: {s.count}
                                  </span>
                                </summary>
                                <ul class="small">
                                  <For each={s.items} fallback={<li class="muted">None.</li>}>
                                    {(d) => (
                                      <li>
                                        <span class="mono">{d.classname ? `${d.classname}::` : ""}{d.name}</span> <span class="muted">({d.suite})</span>
                                        <span class="muted">
                                          {" "}
                                          {d.base_status ?? "new"} → {d.status ?? "removed"}
                                        </span>
                                      </li>
                                    )}
                                  </For>
                                  <Show when={s.count > s.items.length}>
                                    <li class="muted">and {s.count - s.items.length} more</li>
                                  </Show>
                                </ul>
                              </details>
                            )}
                          </For>
                        </div>
                      )}
                    </Show>
                  )}
                </Show>
              </Show>
            </section>

            <div class="row test-case-filters">
              <label class="row small">
                <input type="checkbox" checked={!!failedOnly()} onChange={(e) => setFailedOnly(e.currentTarget.checked)} /> Failed only ({failing(r().counts)})
              </label>
              <input type="search" value={q()} onInput={(e) => setQ(e.currentTarget.value)} placeholder="Search cases" aria-label="Search cases" />
              <span class="muted small">
                {r().cases_total} {r().cases_total === 1 ? "case" : "cases"}
                {failedOnly() || qApplied() ? " match" : ""}
              </span>
            </div>

            <For each={groupBySuite(r().suites, cases())} fallback={<Empty>{failedOnly() && !qApplied() ? "No case failed." : "No case matches."}</Empty>}>
              {(g) => (
                <details class="test-suite card-box" open={g.counts.failed + g.counts.errored > 0 || r().suites.length <= 12}>
                  <summary>
                    <span class="mono test-suite-name">{g.name}</span>
                    <CountsBar counts={g.counts} />
                    <Show when={g.duration_ms !== null}>
                      <span class="muted small">{testDuration(g.duration_ms)}</span>
                    </Show>
                  </summary>
                  <ul class="test-cases">
                    <For each={g.cases}>
                      {(c) => (
                        <li class={`test-case ${c.status}`}>
                          <button type="button" class="test-case-head" aria-expanded={open().has(c.seq)} onClick={() => toggle(c.seq)}>
                            <span class={`chip ${STATUS_TONE[c.status]} test-case-status`}>
                              {CASE_ICON[c.status]} {c.status}
                            </span>
                            <span class="test-case-name">
                              <Show when={c.classname && c.classname !== g.name}>
                                <span class="muted">{c.classname}::</span>
                              </Show>
                              {c.name}
                            </span>
                            <span class="muted small">{testDuration(c.duration_ms)}</span>
                          </button>
                          <Show when={open().has(c.seq)}>
                            <div class="test-case-body stack">
                              <Show when={c.file}>
                                <div class="small muted mono">
                                  {c.file}
                                  {c.line !== null ? `:${c.line}` : ""}
                                </div>
                              </Show>
                              <For each={caseDetails(c)} fallback={<span class="muted small">No output.</span>}>
                                {(d) => (
                                  <div>
                                    <div class="small muted">{d.label}</div>
                                    <pre class="test-output">{d.text}</pre>
                                  </div>
                                )}
                              </For>
                            </div>
                          </Show>
                        </li>
                      )}
                    </For>
                  </ul>
                </details>
              )}
            </For>
            <LoadMore hasMore={!!next()} loading={loading()} onMore={() => void load(true)} count={cases().length} />
          </div>
        )}
      </Show>
    </>
  );
}

function Total(props: { n: number; label: string; status: TestCase["status"] }) {
  return (
    <div class={`test-total ${props.n > 0 ? props.status : ""}`}>
      <div class="test-total-n">
        <span aria-hidden="true">{CASE_ICON[props.status]}</span> {props.n}
      </div>
      <div class="small muted">{props.label}</div>
    </div>
  );
}

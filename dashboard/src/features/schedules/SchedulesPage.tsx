// Schedules (docs/api.md §19). Two views: a project view
// (`/p/:project/board/schedules`, the Schedules tab of the board screen;
// `GET /api/schedules?project=`: the project's schedules and the global
// ones, with a scope filter), and the view of all
// schedules (`/schedules`, grouped by scope). Two tabs, kept in the URL
// (`?tab=triggers`): Schedules (the time specs, `?kind=time`) and Triggers
// (the spec `on`, `?kind=trigger`, §19.4), with the event sources (§20)
// under the triggers. A row opens to its run history; a trigger run shows
// the event that started it. `schedule` events keep the list live:
// `run_started` shows a spinner on the row, `run_finished` sets the last run.
// A webhook action (§19.5) also opens to its Deliveries panel, with Send
// test and Redeliver.

import { A, useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createSignal, For, Index, on, onCleanup, onMount, Show } from "solid-js";
import { MockOnlyNote } from "../../components/MockOnly";
import { ConfirmButton, Empty, ErrorBanner, LoadMore, PageHead, Tabs, useNow } from "../../components/ui";
import type { Schedule } from "../../api/types";
import { api, events } from "../../stores";
import { SCOPE_FILTERS, type ScopeFilter } from "../../lib/target-scope";
import { actionWords, guardWords, matchesWords, nextRunWords, specWords } from "../../lib/schedule";
import { ago } from "../../lib/format";
import { EventSourcesSection } from "./EventSources";
import { createRunHistory } from "./history";
import { applyRunAnswer, applyScheduleEvent, eventRun, filterSchedules, groupSchedules, RUN_CHIP, runOutputText, runSeconds, SCHEDULE_TABS, scheduleScope, scheduleScopeLabel, sortSchedules, tabOf, tabParam, touchesRuns, upsertSchedule, type ScheduleKind, type ScheduleListState } from "./model";
import { ScheduleDialog } from "./ScheduleDialog";
import { DeliveriesPanel } from "./Deliveries";
import { ownerText } from "../dashboard/model";

const COLS = 8;

export default function SchedulesPage() {
  const params = useParams<{ project?: string }>();
  const [search, setSearch] = useSearchParams<{ scope?: string; open?: string; tab?: string }>();
  const project = () => params.project ?? null;
  const kind = (): ScheduleKind => tabOf(search.tab);
  const triggers = () => kind() === "trigger";
  const actions = () => kind() === "action";
  const setTab = (k: ScheduleKind) => setSearch({ tab: tabParam(k), open: undefined }, { replace: true });
  const view = () => ({ project: project(), kind: kind() });
  const filter = (): ScopeFilter => (search.scope === "project" || search.scope === "global" ? search.scope : "all");
  const setFilter = (f: ScopeFilter) => setSearch({ scope: f === "all" ? undefined : f }, { replace: true });

  const [state, setState] = createSignal<ScheduleListState>({ items: [], running: {} });
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<unknown>(null);
  const [open, setOpen] = createSignal<ReadonlySet<string>>(new Set(search.open ? [search.open] : []));
  const [dialog, setDialog] = createSignal<{ schedule?: Schedule } | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [rowError, setRowError] = createSignal<{ id: string; error: unknown } | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);
  const now = useNow(15_000);
  let generation = 0;

  const load = async () => {
    const gen = ++generation;
    const p = project();
    try {
      const items = await api.schedules.list({ ...(p ? { project: p } : {}), kind: kind() });
      if (gen !== generation) return;
      setState((s) => ({ items: sortSchedules(items), running: s.running }));
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  createEffect(
    on([project, kind], () => {
      setLoading(true);
      setState({ items: [], running: {} });
      void load();
    }),
  );

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind !== "schedule") return;
      const r = applyScheduleEvent(state(), e, view());
      setState(r.state);
      if (r.refetch) {
        clearTimeout(t);
        t = setTimeout(() => void load(), 400);
      }
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });

  const put = (s: Schedule) => setState((st) => ({ ...st, items: upsertSchedule(st.items, s, view()) }));
  const toggleOpen = (id: string) =>
    setOpen((cur) => {
      const next = new Set(cur);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const guard = async (s: Schedule, what: string, fn: () => Promise<void>) => {
    setBusy(`${s.id}:${what}`);
    try {
      await fn();
      setRowError(null);
    } catch (e) {
      setRowError({ id: s.id, error: e });
    } finally {
      setBusy(null);
    }
  };
  // The daemon answers when the run ends; the spinner shows until then (and `run_started` sets it too).
  const runNow = (s: Schedule) =>
    guard(s, "run", async () => {
      setState((st) => ({ ...st, running: { [s.id]: st.running[s.id] ?? "?", ...st.running } }));
      try {
        const r = await api.schedules.run(s.id);
        setState((st) => applyRunAnswer(st, r, view()));
        setNotice(r.status === "running" ? `Started ${s.name}.` : `${s.name}: ${r.status}. ${r.summary}`);
        void load();
      } catch (e) {
        setState((st) => {
          const running = { ...st.running };
          delete running[s.id];
          return { ...st, running };
        });
        throw e;
      }
    });
  const toggle = (s: Schedule, el: HTMLInputElement) =>
    guard(s, "toggle", async () => {
      try {
        put(await (el.checked ? api.schedules.enable(s.id) : api.schedules.disable(s.id)));
      } catch (e) {
        el.checked = s.enabled;
        throw e;
      }
    });
  const remove = (s: Schedule) =>
    guard(s, "delete", async () => {
      await api.schedules.remove(s.id);
      setState((st) => ({ ...st, items: st.items.filter((x) => x.id !== s.id) }));
      setNotice(`Deleted ${s.name}.`);
    });

  const groups = () => {
    const list = state().items;
    if (project()) return [{ key: project()!, label: "", items: filterSchedules(list, filter()) }];
    return groupSchedules(list);
  };
  const count = () => groups().reduce((n, g) => n + g.items.length, 0);
  const noun = () => (triggers() ? "triggers" : actions() ? "actions" : "schedules");
  const empty = () => {
    if (loading()) return "Loading…";
    if (project() && filter() === "project") return `Project ${project()} has no ${noun()} of its own.`;
    if (project() && filter() === "global") return `No global ${noun()}.`;
    if (actions()) return "No actions. Make one with New action: it runs only when you press its button on the project dashboard or on Home.";
    return triggers() ? "No triggers. Make one with New trigger, or start from the preset CI failure → mail an agent." : "No schedules. Make one with New schedule.";
  };

  return (
    <>
      {/* In a project it is a tab of the board screen, under the screen title. */}
      <PageHead title={project() ? "Schedules" : "Schedules · all"} level={project() ? 2 : 1}>
        <button class="primary" onClick={() => setDialog({})}>
          {triggers() ? "New trigger" : actions() ? "New action" : "New schedule"}
        </button>
        <button onClick={() => void load()}>Refresh</button>
      </PageHead>
      <div style={{ "margin-bottom": "12px" }}>
        <Tabs tabs={SCHEDULE_TABS} active={kind()} onChange={setTab} />
      </div>
      <div class="spread" style={{ "margin-bottom": "12px", "flex-wrap": "wrap", gap: "8px" }}>
        <div class="row" style={{ gap: "8px", "flex-wrap": "wrap" }}>
          <Show when={project()} fallback={<span class="small muted">All {noun()}, by scope.</span>}>
            <span class="small muted">
              The {noun()} of {project()} and the global ones.
            </span>
            <Tabs tabs={SCOPE_FILTERS} active={filter()} onChange={setFilter} />
          </Show>
        </div>
        <Show when={project()} fallback={<span class="small muted">Open a project for its view.</span>}>
          <A class="small" href="/schedules">
            all projects
          </A>
        </Show>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={notice()}>
        <p class="notice small spread" role="status">
          {notice()}
          <button class="ghost small" onClick={() => setNotice(null)} aria-label="Dismiss">
            ✕
          </button>
        </p>
      </Show>
      <section class="card-box" style={{ "margin-bottom": "16px" }}>
        <div class="table-wrap">
          <table aria-label={triggers() ? "Triggers" : actions() ? "Actions" : "Schedules"}>
            <thead>
              <tr>
                <th>Name</th>
                <th>Scope</th>
                <th>{triggers() ? "On" : "When"}</th>
                <th>Action</th>
                <th>{triggers() ? "Guards" : "Next run"}</th>
                <th>Last run</th>
                <th>Enabled</th>
                <th />
              </tr>
            </thead>
            <Show when={count()} fallback={<tbody><tr><td colSpan={COLS}><Empty>{empty()}</Empty></td></tr></tbody>}>
              <Index each={groups()}>
                {(g) => (
                  <tbody aria-label={g().label || undefined}>
                    <Show when={g().label}>
                      <tr class="group-row">
                        <th colSpan={COLS} scope="colgroup">
                          {g().label} <span class="muted small">· {g().items.length}</span>
                        </th>
                      </tr>
                    </Show>
                    <For each={g().items}>
                      {(s) => {
                        const running = () => !!state().running[s.id];
                        const isOpen = () => open().has(s.id);
                        return (
                          <>
                            <tr aria-label={`Schedule ${s.name}`} classList={{ "row-open": isOpen() }}>
                              <td>
                                <button class="ghost small disclosure" aria-expanded={isOpen()} aria-label={`${isOpen() ? "Close" : "Open"} the runs of ${s.name}`} onClick={() => toggleOpen(s.id)}>
                                  {isOpen() ? "▾" : "▸"}
                                </button>{" "}
                                <strong style={{ "white-space": "nowrap" }}>{s.name}</strong>
                                <Show when={s.source === "file"}>
                                  {" "}
                                  <span class="chip" title="From a file: edit the file">
                                    file
                                  </span>
                                </Show>
                                <Show when={s.disabled_reason}>
                                  {(why) => (
                                    <div>
                                      <span class="chip warn" title="The daemon disabled this schedule">
                                        disabled: {why()}
                                      </span>
                                    </div>
                                  )}
                                </Show>
                                <div class="small muted">{ownerText(s)}</div>
                              </td>
                              <td>
                                <span class={`chip ${scheduleScope(s) === "global" ? "info" : ""}`} title={s.project ? `A schedule of project ${s.project}` : "A global schedule: sysadmin only"}>
                                  {scheduleScopeLabel(s)}
                                </span>
                              </td>
                              <td title={JSON.stringify(s.spec)}>
                                <div class={s.spec.kind === "on" ? "mono small" : undefined}>{specWords(s.spec)}</div>
                                <Show when={s.spec.kind === "on" && matchesWords(s.spec)}>{(m) => <div class="small muted">where {m()}</div>}</Show>
                              </td>
                              <td class="ellipsis" style={{ "max-width": "18rem" }} title={actionWords(s.action, s.project)}>
                                {actionWords(s.action, s.project)}
                              </td>
                              <td class="small">
                                <Show when={s.spec.kind === "on" ? s.spec : null} fallback={<div>{nextRunWords(s, now())}</div>}>{(spec) => <div>{guardWords(spec())}</div>}</Show>
                                <Show when={s.enabled && s.next_run_at}>{(at) => <div class="muted">{new Date(at()).toLocaleString()}</div>}</Show>
                              </td>
                              <td class="small">
                                <Show when={running()}>
                                  <span class="chip info" aria-label={`${s.name} is running`}>
                                    <span class="spinner" aria-hidden="true" /> running
                                  </span>{" "}
                                </Show>
                                <Show when={s.last_run} fallback={<Show when={!running()}><span class="muted">never ran</span></Show>}>
                                  {(l) => (
                                    <>
                                      <Show when={!running()}>
                                        <span class={`chip ${RUN_CHIP[l().status] ?? ""}`}>{l().status}</span>{" "}
                                      </Show>
                                      <span title={new Date(l().at).toLocaleString()}>{ago(l().at, now())}</span>
                                      <div class="muted ellipsis" style={{ "max-width": "16rem" }} title={l().summary}>
                                        {l().summary}
                                      </div>
                                    </>
                                  )}
                                </Show>
                                <button class="link small" onClick={() => toggleOpen(s.id)}>
                                  {isOpen() ? "hide runs" : s.action.type === "webhook" ? `runs (${s.runs_count}) and deliveries` : `runs (${s.runs_count})`}
                                </button>
                              </td>
                              <td>
                                <input
                                  type="checkbox"
                                  aria-label={`Enable ${s.name}`}
                                  title={s.source === "file" ? "From a file: edit the file" : undefined}
                                  checked={s.enabled}
                                  disabled={s.source === "file" || busy() === `${s.id}:toggle`}
                                  onChange={(e) => void toggle(s, e.currentTarget)}
                                />
                              </td>
                              <td>
                                <span class="row" style={{ "flex-wrap": "wrap" }}>
                                  <button class="small" disabled={running() || busy() === `${s.id}:run`} onClick={() => void runNow(s)}>
                                    {running() || busy() === `${s.id}:run` ? "Running…" : "Run now"}
                                  </button>
                                  <button class="small" disabled={s.source === "file"} title={s.source === "file" ? "From a file: edit the file" : undefined} onClick={() => setDialog({ schedule: s })}>
                                    Edit
                                  </button>
                                  <ConfirmButton class="small danger" label="Delete" disabled={s.source === "file"} onConfirm={() => remove(s)} />
                                </span>
                              </td>
                            </tr>
                            <Show when={rowError()?.id === s.id ? rowError()!.error : null}>
                              {(err) => (
                                <tr class="cap-row">
                                  <td colSpan={COLS}>
                                    <ErrorBanner error={err()} onDismiss={() => setRowError(null)} />
                                  </td>
                                </tr>
                              )}
                            </Show>
                            <Show when={isOpen()}>
                              <tr class="lease-row">
                                <td colSpan={COLS}>
                                  <Show when={s.action.type === "webhook"}>
                                    <DeliveriesPanel schedule={s} />
                                  </Show>
                                  <RunHistoryPanel schedule={s} />
                                </td>
                              </tr>
                            </Show>
                          </>
                        );
                      }}
                    </For>
                  </tbody>
                )}
              </Index>
            </Show>
          </table>
        </div>
      </section>
      <Show when={dialog()}>
        {(d) => (
          <ScheduleDialog
            project={project()}
            schedule={d().schedule}
            trigger={triggers()}
            action={actions()}
            onClose={() => setDialog(null)}
            onSaved={(s) => {
              // Read the dialog before it closes: its accessor goes stale after.
              const verb = d().schedule ? "Saved" : "Created";
              setDialog(null);
              put(s);
              setNotice(`${verb} ${s.name} (${scheduleScopeLabel(s)}): ${specWords(s.spec)}.`);
            }}
          />
        )}
      </Show>
      <Show when={triggers()}>
        <EventSourcesSection project={project()} />
      </Show>
      <MockOnlyNote label="new API">
        <code>GET</code>, <code>POST /api/schedules</code>, <code>GET</code>, <code>PATCH</code>, <code>DELETE /api/schedules/{"{id}"}</code>, <code>POST .../run|enable|disable</code>, <code>GET .../runs</code>, and the webhook <code>GET .../deliveries</code>, <code>POST .../deliveries/{"{id}"}/redeliver</code>, <code>POST .../test</code> are docs/api.md §19; <code>/api/event-sources</code> is §20.
      </MockOnlyNote>
    </>
  );
}

/** The runs of one schedule, newest first, with Load more; a live run comes in from the events. */
function RunHistoryPanel(props: { schedule: Schedule }) {
  // The panel belongs to one schedule; a new schedule mounts a new panel.
  const h = createRunHistory(api, props.schedule.id);
  const trigger = props.schedule.spec.kind === "on";
  onMount(() => {
    void h.reload();
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      const r = eventRun(e, props.schedule.id);
      if (r) h.apply(r);
      else if (touchesRuns(e, props.schedule.id)) {
        // The daemon's event has only the run ID: read the newest page again.
        clearTimeout(t);
        t = setTimeout(() => void h.syncHead(), 200);
      }
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });
  return (
    <div class="stack lease-section" style={{ gap: "6px" }} aria-label={`Runs of ${props.schedule.name}`}>
      <span class="small">
        <strong>Runs</strong> <span class="muted">· {props.schedule.runs_count} in all</span>
      </span>
      <ErrorBanner error={h.error()} />
      <Show when={h.runs().length} fallback={<span class="small muted">{h.initial() || h.loading() ? "Loading…" : "No runs yet."}</span>}>
        <table class="small" aria-label={`Run history of ${props.schedule.name}`}>
          <thead>
            <tr>
              <th>Started</th>
              <th>Trigger</th>
              <Show when={trigger}>
                <th>Event</th>
              </Show>
              <th>Status</th>
              <th>Took</th>
              <th>Summary and output</th>
            </tr>
          </thead>
          <tbody>
            <For each={h.runs()}>
              {(r) => (
                <tr>
                  <td title={r.started_at}>{new Date(r.started_at).toLocaleString()}</td>
                  <td>{r.trigger.replace("_", " ")}</td>
                  <Show when={trigger}>
                    <td>
                      <Show when={r.event} fallback={<span class="muted">{r.event_seq ? `#${r.event_seq} (pruned)` : "—"}</span>}>
                        {(ev) => (
                          <>
                            <span class="mono">#{ev().seq}</span> <span class="chip mono">{ev().kind}</span>
                            <div class="muted ellipsis" style={{ "max-width": "18rem" }} title={ev().summary}>
                              {ev().summary}
                            </div>
                          </>
                        )}
                      </Show>
                    </td>
                  </Show>
                  <td>
                    <span class={`chip ${RUN_CHIP[r.status] ?? ""}`}>
                      <Show when={r.status === "running"}>
                        <span class="spinner" aria-hidden="true" />{" "}
                      </Show>
                      {r.status}
                    </span>
                  </td>
                  <td>{runSeconds(r) === null ? "—" : `${runSeconds(r)} s`}</td>
                  <td>
                    <div>{r.summary}</div>
                    <Show when={r.error}>{(e) => <pre class="error-detail run-output">{e()}</pre>}</Show>
                    <Show when={runOutputText(r.output)}>
                      {(out) => (
                        <details>
                          <summary class="muted">output</summary>
                          <pre class="run-output">{out()}</pre>
                        </details>
                      )}
                    </Show>
                  </td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </Show>
      <LoadMore hasMore={h.hasMore()} loading={h.loading()} onMore={() => void h.loadMore()} count={h.runs().length || undefined} />
    </div>
  );
}

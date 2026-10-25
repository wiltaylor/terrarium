// Targets (docs/api.md §17): kind, scope, host, state, the lifecycle verbs,
// the leases, and an exec console. A target is global, or one project owns
// it (§17.2). `POST .../up|down|snapshot|restore|status` (§17.3), `POST
// .../exec` with `{shell, pty}` and the session WebSocket (§17.4).

import { useSearchParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, Index, onCleanup, Show } from "solid-js";
import { GrantDialog, grantNotice, LeaseTable, useLeaseList } from "../../components/Leases";
import { MockOnlyNote } from "../../components/MockOnly";
import { Terminal } from "../../components/Terminal";
import { Empty, ErrorBanner, PageHead, Tabs } from "../../components/ui";
import type { Target } from "../../api/types";
import type { TargetAction } from "../../api/client";
import { api, leases as holds, projects } from "../../stores";
import { filterByScope, groupByScope, SCOPE_FILTERS, scopeLabel, type ScopeFilter } from "../../lib/target-scope";
import { STATE_CHIP, targetActions } from "./actions";
import { CreateTargetDialog } from "./CreateTargetDialog";
import { ScopeChip } from "./ScopeChip";

const COLS = 7;

/**
 * Two views. A project view (the project of the sidebar): `GET
 * /api/projects/{name}/targets`, the project's targets and the global ones,
 * with a scope filter. The view of all targets (`?all=1`, or no project):
 * `GET /api/targets`, grouped by scope. `?project=<name>` selects that
 * project in the sidebar; `?scope=project|global` sets the filter.
 */
export default function TargetsPage() {
  const [search, setSearch] = useSearchParams<{ project?: string; all?: string; scope?: string }>();
  // A link from a project page: select the project, then keep the URL short.
  createEffect(() => {
    const p = search.project;
    if (p) {
      if (projects.selected() !== p) projects.select(p);
      setSearch({ project: undefined, all: undefined }, { replace: true });
    }
  });
  const project = () => (search.all === "1" || search.project ? (search.project ?? null) : projects.selected());
  const filter = (): ScopeFilter => (search.scope === "project" || search.scope === "global" ? search.scope : "all");
  const setFilter = (f: ScopeFilter) => setSearch({ scope: f === "all" ? undefined : f }, { replace: true });
  const setAll = (all: boolean) => setSearch({ all: all ? "1" : undefined, scope: undefined }, { replace: true });

  const [targets, { refetch }] = createResource(
    () => ({ project: project() }),
    (v) => (v.project ? api.projects.targets(v.project) : api.targets.list()),
  );
  const [hosts] = createResource(() => api.hosts.list().catch(() => []));
  const [error, setError] = createSignal<unknown>(null);
  const [consoleFor, setConsole] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [creating, setCreating] = createSignal(false);
  // `target:verb` of the verb that runs now. A verb runs to its end before the daemon answers.
  const [busy, setBusy] = createSignal<string | null>(null);

  const groups = () => {
    const list = targets.latest ?? [];
    const p = project();
    if (p) return [{ key: p, label: "", targets: filterByScope(list, filter(), p) }];
    return groupByScope(list);
  };
  const count = () => groups().reduce((n, g) => n + g.targets.length, 0);

  const act = async (id: string, action: TargetAction) => {
    setBusy(`${id}:${action}`);
    try {
      const r = await api.targets.action(id, action);
      setError(null);
      setNotice(`${r.target}: ${r.verb} → ${r.state}${r.snapshot ? ` (snapshot ${r.snapshot})` : ""}`);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
      void refetch();
    }
  };
  const hostStatus = (t: Target) => {
    if (t.host_online === true) return "online";
    if (t.host_online === false) return "offline";
    return t.host ? hosts.latest?.find((h) => h.name === t.host)?.status : undefined;
  };
  const empty = () => {
    if (targets.loading) return "Loading…";
    if (project() && filter() === "project") return `Project ${project()} has no targets of its own.`;
    if (project() && filter() === "global") return "No global targets.";
    return "No targets. Define one with New target, or in targets/<name>.toml.";
  };

  return (
    <>
      <PageHead title={project() ? `Targets · ${project()}` : "Targets · all"}>
        <button class="primary" onClick={() => setCreating(true)}>
          New target
        </button>
        <button onClick={() => void refetch()}>Refresh</button>
      </PageHead>
      <div class="spread" style={{ "margin-bottom": "12px", "flex-wrap": "wrap", gap: "8px" }}>
        <div class="row" style={{ gap: "8px", "flex-wrap": "wrap" }}>
          <Show when={project()} fallback={<span class="small muted">All targets, by scope.</span>}>
            <span class="small muted">The targets of {project()} and the global ones.</span>
            <Tabs tabs={SCOPE_FILTERS} active={filter()} onChange={setFilter} />
          </Show>
        </div>
        <label class="row small">
          <input type="checkbox" checked={!project()} disabled={!projects.selected() && !project()} onChange={(e) => setAll(e.currentTarget.checked)} />
          all projects
        </label>
      </div>
      <ErrorBanner error={error() ?? targets.error} onDismiss={() => setError(null)} />
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
          <table aria-label="Targets">
            <thead>
              <tr>
                <th>Name</th>
                <th>Kind</th>
                <th>Scope</th>
                <th>Host</th>
                <th>State</th>
                <th>Snapshots</th>
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
                          {g().label} <span class="muted small">· {g().targets.length}</span>
                        </th>
                      </tr>
                    </Show>
                    <Index each={g().targets}>
                      {(t) => (
                        <>
                          <tr>
                            <td>
                              <strong>{t().name}</strong>
                              <Show when={t().ephemeral}>
                                {" "}
                                <span class="chip">ephemeral</span>
                              </Show>
                              <Show when={t().snapshot_before_task}>
                                {" "}
                                <span class="chip" title="Snapshot before each task">
                                  snapshot per task
                                </span>
                              </Show>
                            </td>
                            <td>{t().kind}</td>
                            <td>
                              <ScopeChip target={t()} />
                            </td>
                            <td class="small">
                              <Show when={t().host} fallback={<span class="mono">{t().address ?? "—"}</span>}>
                                <span class="mono">{t().host}</span>{" "}
                                <Show when={hostStatus(t())}>{(s) => <span class={`chip ${s() === "online" ? "ok" : "bad"}`}>{s()}</span>}</Show>
                              </Show>
                            </td>
                            <td>
                              <span class={`chip ${STATE_CHIP[t().state] ?? ""}`} title={t().state_at ? `since ${new Date(t().state_at!).toLocaleString()}` : undefined}>
                                {t().state}
                              </span>
                            </td>
                            <td class="small">{t().snapshots.join(", ") || "—"}</td>
                            <td>
                              <span class="row" style={{ "flex-wrap": "wrap" }}>
                                <For each={targetActions(t())}>
                                  {(a) => (
                                    <button class="small" disabled={busy() !== null} onClick={() => act(t().id, a)}>
                                      {busy() === `${t().id}:${a}` ? `${a}…` : a[0]!.toUpperCase() + a.slice(1)}
                                    </button>
                                  )}
                                </For>
                                <button class="small" onClick={() => setConsole(t().name)}>
                                  Console
                                </button>
                              </span>
                            </td>
                          </tr>
                          <tr class="lease-row">
                            <td colSpan={COLS}>
                              <TargetLeases target={t()} onNotice={setNotice} />
                            </td>
                          </tr>
                        </>
                      )}
                    </Index>
                  </tbody>
                )}
              </Index>
            </Show>
          </table>
        </div>
      </section>
      <Show when={consoleFor()}>{(name) => <ExecConsole target={name()} onClose={() => setConsole(null)} />}</Show>
      <Show when={creating()}>
        <CreateTargetDialog
          project={project() ?? projects.selected()}
          onClose={() => setCreating(false)}
          onCreated={(t) => {
            setCreating(false);
            setNotice(`Created ${t.name} (${scopeLabel(t)}).`);
            void refetch();
          }}
        />
      </Show>
      <MockOnlyNote label="new API">
        <code>GET /api/targets</code>, <code>GET /api/projects/{"{name}"}/targets</code>, and <code>POST /api/targets</code> are in docs/api.md §17.2. <code>POST /api/targets/{"{name}"}/up|down|snapshot|restore|status</code> is §17.3, exec sessions §17.4. Leases: <code>GET</code>, <code>POST /api/targets/{"{name}"}/leases</code>, <code>PATCH</code>, <code>DELETE .../leases/{"{agent}"}</code> (docs/api.md §17.7).
      </MockOnlyNote>
    </>
  );
}

/** The leases on one target (docs/api.md §17.7): the active ones, or the history; Grant, Extend, Revoke. */
function TargetLeases(props: { target: Target; onNotice: (s: string) => void }) {
  const list = useLeaseList({
    key: () => props.target.name,
    fetch: (name, all) => api.targets.leases(name, { all }),
    match: (d) => d.target === props.target.name,
    onLoaded: (name, rows) => holds.seedTarget(name, rows),
  });
  const [granting, setGranting] = createSignal(false);
  const active = () => list.list().filter((l) => l.state === "active").length;
  return (
    <div class="stack lease-section" style={{ gap: "6px" }} aria-label={`Leases on ${props.target.name}`}>
      <div class="spread">
        <span class="small">
          <strong>Leases</strong> <span class="muted">· {active()} active</span>
        </span>
        <span class="row" style={{ gap: "8px" }}>
          <label class="row small">
            <input type="checkbox" checked={list.all()} onChange={(e) => list.setAll(e.currentTarget.checked)} />
            history
          </label>
          <button class="small" onClick={() => setGranting(true)}>
            Grant…
          </button>
        </span>
      </div>
      <ErrorBanner error={list.error()} />
      <Show when={!list.error()}>
        <LeaseTable leases={list.list()} show="agent" history={list.all()} loading={list.loading()} onChange={list.put} />
      </Show>
      <Show when={granting()}>
        <GrantDialog
          target={props.target}
          onClose={() => setGranting(false)}
          onGranted={(l) => {
            setGranting(false);
            list.put(l);
            props.onNotice(grantNotice(l));
          }}
        />
      </Show>
    </div>
  );
}

/** A login shell on the target: `POST .../exec {shell, pty}`, then the session WebSocket. Closing kills the run. */
function ExecConsole(props: { target: string; onClose: () => void }) {
  const [error, setError] = createSignal<unknown>(null);
  const [session] = createResource(
    () => props.target,
    (name) =>
      api.targets.exec(name, { shell: true, pty: { rows: 24, cols: 80 } }).catch((e: unknown) => {
        setError(e);
        return null;
      }),
  );
  onCleanup(() => {
    const s = session.latest;
    if (s) void api.targets.closeSession(props.target, s.session_id).catch(() => undefined);
  });
  return (
    <section class="card-box stack" aria-label="Exec console">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Console · {props.target}</h3>
        <button class="small" onClick={() => props.onClose()}>
          Close and end the shell
        </button>
      </div>
      <ErrorBanner error={error()} />
      <Show when={session()} fallback={<Show when={!error()}><p class="muted">Opening a shell…</p></Show>}>
        {(s) => <Terminal connect={() => api.targets.sessionPty(props.target, s().session_id)} title={`${s().kind} on ${s().target}`} interactive reconnect={false} />}
      </Show>
    </section>
  );
}

import { useInstance } from "../../instance";
// Targets (docs/api.md §17): kind, scope, host, state, the lifecycle verbs,
// the leases, and an exec console. A target is global, or one project owns
// it (§17.2). `POST .../up|down|snapshot|restore|status` (§17.3), `POST
// .../exec` with `{shell, pty}` and the session WebSocket (§17.4). Mode 1
// (§17.8): the lab source of a vmlab target, the attached workspaces, the
// lab instance of each holder of a `lab_from_workspace` target with its own
// verbs, and the two caps (`409` project cap, `429` host cap) inline. A
// vmlab target has a snapshot panel: `GET .../snapshots`, take, restore,
// and `DELETE .../snapshots/{snap}` (§17.3).

import { useSearchParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, Index, onCleanup, Show } from "solid-js";
import { GrantDialog, grantNotice, LeaseTable, useLeaseList } from "../../components/Leases";
import { MockOnlyNote } from "../../components/MockOnly";
import { Terminal } from "../../components/Terminal";
import { ConfirmButton, Empty, ErrorBanner, PageHead, Tabs } from "../../components/ui";
import type { SnapshotScope, Target, TargetReport, TargetSnapshotList, TargetVerbBody } from "../../api/types";
import type { TargetAction } from "../../api/client";

import { filterByScope, groupByScope, SCOPE_FILTERS, scopeLabel, type ScopeFilter } from "../../lib/target-scope";
import { capError, capSentence, holderRows, instanceActions, labSource, sourceLabel, workspacePath, type CapError } from "../../lib/target-runtime";
import { STATE_CHIP, targetActions } from "./actions";
import { deleteNeedsForce, deleteNotice, hasSnapshots, listSummary, snapshotNameError, snapshotScopes, verbNotice } from "./snapshots";
import { CreateTargetDialog } from "./CreateTargetDialog";
import { ScopeChip } from "./ScopeChip";
import { ScreenPanel } from "./ScreenPanel";
import { CredentialCell, takesCredential, TargetCredential } from "./TargetCredential";
import { targetHasDisplay } from "./display";
import { limitsChip, limitsTitle } from "../../lib/limits";

const COLS = 7;

/**
 * Two views. A project view (the project of the sidebar): `GET
 * /api/projects/{name}/targets`, the project's targets and the global ones,
 * with a scope filter. The view of all targets (`?all=1`, or no project):
 * `GET /api/targets`, grouped by scope. `?project=<name>` selects that
 * project in the sidebar; `?scope=project|global` sets the filter.
 */
export default function TargetsPage() {
  const { projects, api } = useInstance();
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
  // docs/api.md §17.10: the target whose live screen shows.
  const [screenFor, setScreenFor] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [creating, setCreating] = createSignal(false);
  // `target:verb[:agent]` of the verb that runs now. A verb runs to its end before the daemon answers.
  const [busy, setBusy] = createSignal<string | null>(null);
  // A cap refusal of a verb, shown on the row of its target.
  const [capFor, setCapFor] = createSignal<{ target: string; cap: CapError } | null>(null);
  // The open snapshot panel: one target, and the holder of a `lab_from_workspace` instance.
  const [snapsFor, setSnapsFor] = createSignal<{ target: string; agent?: string; who?: string } | null>(null);
  const toggleSnaps = (target: string, agent?: string, who?: string) => {
    const cur = snapsFor();
    setSnapsFor(cur?.target === target && cur.agent === agent ? null : { target, agent, who });
  };

  const groups = () => {
    const list = targets.latest ?? [];
    const p = project();
    if (p) return [{ key: p, label: "", targets: filterByScope(list, filter(), p) }];
    return groupByScope(list);
  };
  const count = () => groups().reduce((n, g) => n + g.targets.length, 0);

  const act = async (id: string, action: TargetAction, body?: TargetVerbBody, who?: string) => {
    setBusy(`${id}:${action}${body?.agent ? `:${body.agent}` : ""}`);
    try {
      const r = await api.targets.action(id, action, body);
      setError(null);
      setCapFor(null);
      setNotice(reportNotice(r, who));
    } catch (e) {
      const cap = capError(e);
      if (cap) {
        setCapFor({ target: id, cap });
        setError(null);
      } else setError(e);
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
      <PageHead title={project() ? `Targets · ${project()}` : "Targets · all"} level={2}>
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
                              <Show when={limitsChip(t())}>
                                {(l) => (
                                  <>
                                    {" "}
                                    <span class={`chip${t().limits_info?.problem ? " bad" : ""}`} aria-label="Resource limits" title={limitsTitle(t())}>
                                      {l()}
                                    </span>
                                  </>
                                )}
                              </Show>
                              <Show when={sourceLabel(t())}>
                                {(src) => (
                                  <div class="small muted" aria-label="Lab source">
                                    <span class="chip">{SOURCE_CHIP[labSource(t())!]}</span> <span class="mono">{src()}</span>
                                    <Show when={t().runtime?.head}>{(h) => <span class="mono" title={h()}> · {h().slice(0, 10)}</span>}</Show>
                                  </div>
                                )}
                              </Show>
                              <CredentialCell target={t()} />
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
                                      {busy() === `${t().id}:${a}` ? `${a}…` : verbLabel(a)}
                                    </button>
                                  )}
                                </For>
                                <Show when={hasSnapshots(t()) && !t().lab_from_workspace}>
                                  <button class="small" aria-expanded={snapsFor()?.target === t().id} onClick={() => toggleSnaps(t().id)}>
                                    Snapshots
                                  </button>
                                </Show>
                                <Show when={t().lab_from_workspace}>
                                  <span class="small muted">verbs per holder, below</span>
                                </Show>
                                <Show when={!["vnc", "rdp", "desktop"].includes(t().kind)}>
                                  <button class="small" onClick={() => setConsole(t().name)}>
                                    Console
                                  </button>
                                </Show>
                                <Show when={targetHasDisplay(t())}>
                                  <button class="small" aria-expanded={screenFor() === t().id} onClick={() => setScreenFor(screenFor() === t().id ? null : t().id)}>
                                    Screen
                                  </button>
                                </Show>
                              </span>
                            </td>
                          </tr>
                          <Show when={capFor()?.target === t().id ? capFor()!.cap : null}>
                            {(c) => (
                              <tr class="cap-row">
                                <td colSpan={COLS}>
                                  <div class="error-banner spread" role="alert" aria-label={`Cap on ${t().name}`}>
                                    <div class="stack" style={{ gap: "2px" }}>
                                      <strong>Refused ({c().cap === "host" ? "429, host cap" : "409, project cap"}): {capSentence(c())}</strong>
                                      <span class="small">{c().message}</span>
                                    </div>
                                    <button class="ghost small" onClick={() => setCapFor(null)} aria-label="Dismiss">
                                      ✕
                                    </button>
                                  </div>
                                </td>
                              </tr>
                            )}
                          </Show>
                          <Show when={hasRuntime(t())}>
                            <tr class="runtime-row">
                              <td colSpan={COLS}>
                                <TargetRuntimePanel target={t()} busy={busy()} act={act} onSnapshots={(agent, who) => toggleSnaps(t().id, agent, who)} />
                              </td>
                            </tr>
                          </Show>
                          <Show when={snapsFor()?.target === t().id ? snapsFor() : null}>
                            {(s) => (
                              <tr class="snapshot-row">
                                <td colSpan={COLS}>
                                  <TargetSnapshots
                                    target={t()}
                                    agent={s().agent}
                                    who={s().who}
                                    onNotice={setNotice}
                                    onChanged={() => void refetch()}
                                    onClose={() => setSnapsFor(null)}
                                  />
                                </td>
                              </tr>
                            )}
                          </Show>
                          <Show when={screenFor() === t().id}>
                            <tr class="screen-row">
                              <td colSpan={COLS}>
                                <ScreenPanel target={t()} onClose={() => setScreenFor(null)} />
                              </td>
                            </tr>
                          </Show>
                          <Show when={takesCredential(t())}>
                            <tr class="credential-row">
                              <td colSpan={COLS}>
                                <TargetCredential target={t()} onNotice={setNotice} onChanged={() => void refetch()} />
                              </td>
                            </tr>
                          </Show>
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
        <code>GET /api/targets</code>, <code>GET /api/projects/{"{name}"}/targets</code>, and <code>POST /api/targets</code> are in docs/api.md §17.2. <code>POST /api/targets/{"{name}"}/up|down|snapshot|restore|status</code>, <code>GET .../snapshots</code>, and <code>DELETE .../snapshots/{"{snap}"}</code> are §17.3, exec sessions §17.4. Leases: <code>GET</code>, <code>POST /api/targets/{"{name}"}/leases</code>, <code>PATCH</code>, <code>DELETE .../leases/{"{agent}"}</code> (docs/api.md §17.7).
      </MockOnlyNote>
    </>
  );
}

const SOURCE_CHIP = { dir: "lab dir", repo: "lab repo", workspace: "lab from workspace" } as const;

const verbLabel = (a: string) => a[0]!.toUpperCase() + a.slice(1);

/** The notice of a verb report, with the holder of an instance and an `attach_error`. */
const reportNotice = (r: TargetReport, who?: string) =>
  `${r.target}${who ? ` (${who})` : ""}: ${r.verb} → ${r.state}${r.snapshot ? ` (snapshot ${r.snapshot})` : ""}${r.attach_error ? `. ${r.attach_error}` : ""}`;

/** A target with something to show in the runtime panel: attached workspaces, a lab instance, or holders. */
const hasRuntime = (t: Target) => !!t.lab_from_workspace || !!t.runtime?.attached?.length || !!t.runtime?.lab;

/**
 * What runs on the target (docs/api.md §17.8): the attached workspaces and
 * their paths in the target, the lab dir of a `lab_repo` instance, and for a
 * `lab_from_workspace` target each holder with its instance and its verbs.
 */
function TargetRuntimePanel(props: {
  target: Target;
  busy: string | null;
  act: (id: string, a: TargetAction, body?: TargetVerbBody, who?: string) => void;
  onSnapshots: (agent: string, who: string) => void;
}) {
  const { leases: holds, agents } = useInstance();
  const holderIds = () =>
    Object.entries(holds.holds())
      .filter(([, byTarget]) => byTarget[props.target.name])
      .map(([agent]) => agent);
  const nameOf = (id: string) => {
    const a = agents.byId(id);
    return a ? `${a.project}/${a.name}` : undefined;
  };
  const rows = () => holderRows(props.target, holderIds(), nameOf);
  return (
    <div class="stack" style={{ gap: "4px" }} aria-label={`Runtime of ${props.target.name}`}>
      <Show when={props.target.runtime?.attached?.length}>
        <span class="small">
          <strong>Workspaces</strong>{" "}
          <For each={props.target.runtime!.attached}>
            {(w) => (
              <span class="chip mono" title={`The workspace of ${w}, at ${workspacePath(w)} in the target`}>
                {workspacePath(w)}
              </span>
            )}
          </For>
        </span>
      </Show>
      <Show when={props.target.runtime?.lab}>
        {(lab) => (
          <span class="small">
            <strong>Instance</strong> <span class="mono">{lab()}</span>
          </span>
        )}
      </Show>
      <Show when={props.target.lab_from_workspace}>
        <span class="small">
          <strong>Instances</strong> <span class="muted">· one per holder, from the lab in the holder's workspace</span>
        </span>
        <Show when={rows().length} fallback={<span class="small muted">No holder has an instance. A holder's instance comes up when the agent starts or gets its lease.</span>}>
          <table class="small" aria-label={`Instances of ${props.target.name}`}>
            <thead>
              <tr>
                <th>Holder</th>
                <th>State</th>
                <th>Workspace</th>
                <th>Lab dir</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={rows()}>
                {(r) => (
                  <tr>
                    <td class="mono">{r.agent_name ?? r.agent}</td>
                    <td>
                      <span class={`chip ${STATE_CHIP[r.instance?.state ?? "absent"] ?? ""}`}>{r.instance?.state ?? "no instance"}</span>
                    </td>
                    <td class="mono">{(r.instance?.attached ?? []).map(workspacePath).join(", ") || "—"}</td>
                    <td class="mono">{r.instance?.lab ?? "—"}</td>
                    <td>
                      <span class="row" style={{ "flex-wrap": "wrap" }}>
                        <For each={instanceActions(r.instance?.state)}>
                          {(a) => (
                            <button class="small" disabled={props.busy !== null} onClick={() => props.act(props.target.id, a, { agent: r.agent }, r.agent_name ?? r.agent)}>
                              {props.busy === `${props.target.id}:${a}:${r.agent}` ? `${a}…` : verbLabel(a)}
                            </button>
                          )}
                        </For>
                        <Show when={r.instance?.lab}>
                          <button class="small" onClick={() => props.onSnapshots(r.agent, r.agent_name ?? r.agent)}>
                            Snapshots
                          </button>
                        </Show>
                      </span>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Show>
      </Show>
    </div>
  );
}

/**
 * The snapshots of a vmlab target, or of one holder's instance (docs/api.md
 * §17.3): what the host has (`managed` marks the ones Terrarium took), with
 * a refresh, a take with a name, a restore (in-page confirm, with the option
 * to discard guest changes), and a delete (in-page confirm; `terrarium-clean`
 * with `snapshot_before_task` sends `force`).
 */
function TargetSnapshots(props: { target: Target; agent?: string; who?: string; onNotice: (s: string) => void; onChanged: () => void; onClose: () => void }) {
  const { api } = useInstance();
  const scopes = () => snapshotScopes(props.target);
  const [scope, setScope] = createSignal<SnapshotScope>(scopes()[0]!);
  const [error, setError] = createSignal<unknown>(null);
  const [list, { refetch }] = createResource(
    () => ({ name: props.target.name, agent: props.agent, scope: scope() }),
    (k): Promise<TargetSnapshotList | null> =>
      api.targets.snapshots(k.name, { scope: k.scope, agent: k.agent }).catch((e: unknown) => {
        setError(e);
        return null;
      }),
  );
  const [name, setName] = createSignal("");
  const [busy, setBusy] = createSignal<string | null>(null);
  // The snapshot whose restore waits for the in-page confirm.
  const [restoring, setRestoring] = createSignal<string | null>(null);
  const [discard, setDiscard] = createSignal(false);
  const nameError = () => snapshotNameError(name().trim());
  const run = async (key: string, f: () => Promise<string>) => {
    setBusy(key);
    try {
      props.onNotice(await f());
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
      void refetch();
      props.onChanged();
    }
  };
  const body = (extra: TargetVerbBody = {}): TargetVerbBody => ({ scope: scope(), ...(props.agent ? { agent: props.agent } : {}), ...extra });
  const take = () =>
    run("take", async () => {
      const n = name().trim();
      const r = await api.targets.action(props.target.id, "snapshot", body(n ? { name: n } : {}));
      setName("");
      return verbNotice(r, props.who);
    });
  const restore = (snap: string) =>
    run(`restore:${snap}`, async () => {
      const r = await api.targets.action(props.target.id, "restore", body({ name: snap, ...(discard() ? { discard_guest_changes: true } : {}) }));
      setRestoring(null);
      setDiscard(false);
      return verbNotice(r, props.who);
    });
  const remove = (snap: string) =>
    run(`delete:${snap}`, async () => {
      const force = deleteNeedsForce(props.target, snap);
      const d = await api.targets.deleteSnapshot(props.target.id, snap, { scope: scope(), ...(props.agent ? { agent: props.agent } : {}), ...(force ? { force: true } : {}) });
      return deleteNotice(d, props.who);
    });
  return (
    <div class="stack" style={{ gap: "6px" }} aria-label={`Snapshots of ${props.target.name}`}>
      <div class="spread" style={{ "flex-wrap": "wrap", gap: "8px" }}>
        <span class="small">
          <strong>Snapshots</strong>
          <Show when={props.who}>{(w) => <span class="muted"> · instance of {w()}</span>}</Show>
          <Show when={list()}>{(l) => <span class="muted"> · {listSummary(l())}</span>}</Show>
        </span>
        <span class="row" style={{ gap: "8px", "flex-wrap": "wrap" }}>
          <Show when={scopes().length > 1}>
            <Tabs tabs={scopes().map((s) => ({ id: s, label: s === "lab" ? "whole lab" : `machine ${props.target.machine}` }))} active={scope()} onChange={setScope} />
          </Show>
          <button class="small" disabled={list.loading} onClick={() => void refetch()}>
            {list.loading ? "Loading…" : "Refresh"}
          </button>
          <button class="ghost small" onClick={() => props.onClose()} aria-label="Close snapshots">
            ✕
          </button>
        </span>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form
        class="row small"
        style={{ gap: "6px", "flex-wrap": "wrap" }}
        onSubmit={(e) => {
          e.preventDefault();
          if (!nameError()) void take();
        }}
      >
        <label class="row small">
          Name <input value={name()} placeholder="terrarium" aria-label="Snapshot name" onInput={(e) => setName(e.currentTarget.value)} />
        </label>
        <button type="submit" class="small" disabled={busy() !== null || !!nameError()}>
          {busy() === "take" ? "Taking…" : scope() === "lab" ? "Take (whole lab)" : "Take"}
        </button>
        <Show when={nameError()}>{(m) => <span class="small" role="alert" style={{ color: "var(--danger)" }}>{m()}</span>}</Show>
      </form>
      <Show when={list()?.snapshots.length} fallback={<span class="small muted">{list.loading ? "Loading…" : list() ? "No snapshots." : ""}</span>}>
        <table class="small" aria-label={`Snapshot list of ${props.target.name}`}>
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Taken</th>
              <th>Machines</th>
              <th>Terrarium</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <For each={list()!.snapshots}>
              {(s) => (
                <tr>
                  <td class="mono">{s.name}</td>
                  <td>{s.kind ?? "—"}</td>
                  <td>{s.taken ? new Date(s.taken).toLocaleString() : "—"}</td>
                  <td class="mono">{s.machines.join(", ")}</td>
                  <td>{s.managed ? <span class="chip ok">managed</span> : <span class="muted">—</span>}</td>
                  <td>
                    <Show
                      when={restoring() === s.name}
                      fallback={
                        <span class="row" style={{ gap: "6px", "flex-wrap": "wrap" }}>
                          <button class="small" disabled={busy() !== null} onClick={() => setRestoring(s.name)}>
                            {busy() === `restore:${s.name}` ? "Restoring…" : "Restore"}
                          </button>
                          <ConfirmButton
                            class="small danger"
                            label={busy() === `delete:${s.name}` ? "Deleting…" : "Delete"}
                            confirmLabel={deleteNeedsForce(props.target, s.name) ? "Yes, delete (force: snapshot per task is on)" : "Yes, delete"}
                            disabled={busy() !== null}
                            onConfirm={() => remove(s.name)}
                          />
                        </span>
                      }
                    >
                      <span class="row confirm" style={{ gap: "6px", "flex-wrap": "wrap" }} aria-label={`Restore ${s.name}`}>
                        <span class="small">Roll back to {s.name}?</span>
                        <label class="row small">
                          <input type="checkbox" checked={discard()} onChange={(e) => setDiscard(e.currentTarget.checked)} />
                          discard guest changes
                        </label>
                        <button class="danger small" disabled={busy() !== null} onClick={() => void restore(s.name)}>
                          Yes, restore
                        </button>
                        <button
                          class="small"
                          onClick={() => {
                            setRestoring(null);
                            setDiscard(false);
                          }}
                        >
                          Cancel
                        </button>
                      </span>
                    </Show>
                  </td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </Show>
    </div>
  );
}

/** The leases on one target (docs/api.md §17.7): the active ones, or the history; Grant, Extend, Revoke. */
function TargetLeases(props: { target: Target; onNotice: (s: string) => void }) {
  const { api, leases: holds } = useInstance();
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
  const { api } = useInstance();
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

import { InstanceAnchor } from "../../instance/navigation";
import { useInstance } from "../../instance";
// Settings, Software tab: the build of the running daemon
// (`GET /api/system/version`, docs/api.md §4): the version, the git ref,
// the build time, and the host agents built from another ref
// (`stale` in `GET /api/hosts`). The self-update (docs/api.md §34,
// `GET /api/system/update`): the newest release with its notes, Check
// now, Update, Roll back (the operator and sysadmins; each asks in the
// page), the policy and the channel, the updater, and the update log.

import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { A } from "../../instance/navigation";
import { ConfirmButton, ErrorBanner, PageHead } from "../../components/ui";
import { SafeHtml } from "../../components/SafeHtml";
import { staleHosts } from "../../lib/build-version";
import { renderMarkdown } from "../../lib/markdown";
import { blockedReason, canRollback, imageText, outcomeChip, runningRun, updateTarget } from "../../lib/self-update";

import type { UpdateRun } from "../../api/types";

/** How often the page reads the state while a run runs. */
const POLL_MS = 3000;

export default function SoftwarePage() {
  const { api } = useInstance();
  const [version, { refetch }] = createResource(() => api.systemVersion());
  // A caller that cannot list the hosts sees no host line.
  const [hosts] = createResource(() => api.hosts.list().catch(() => null));
  const stale = () => staleHosts(hosts());
  const [update, { refetch: refetchUpdate, mutate }] = createResource(() => api.selfUpdate.status());
  const [actionError, setActionError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  // Poll while a run runs, and for a while after a request: the daemon
  // restarts in the middle of an update, so a read can fail for a moment.
  const [pollUntil, setPollUntil] = createSignal(0);
  const timer = setInterval(() => {
    if (runningRun(update()) || Date.now() < pollUntil()) {
      void refetchUpdate();
      void refetch();
    }
  }, POLL_MS);
  onCleanup(() => clearInterval(timer));

  const act = async (what: string, f: () => Promise<{ run: string }>) => {
    setBusy(true);
    setActionError(null);
    try {
      const r = await f();
      setNotice(`${what} started (run ${r.run}). The daemon drains, backs up, and restarts; this page follows it.`);
      setPollUntil(Date.now() + 15 * 60_000);
      await refetchUpdate();
    } catch (e) {
      setActionError(e);
    } finally {
      setBusy(false);
    }
  };

  const check = async () => {
    setBusy(true);
    setActionError(null);
    try {
      mutate(await api.selfUpdate.check());
    } catch (e) {
      setActionError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageHead title="Software" level={2}>
        <button
          onClick={() => {
            void refetch();
            void refetchUpdate();
          }}
        >
          Refresh
        </button>
      </PageHead>
      <ErrorBanner error={version.error} />
      <section class="card-box" aria-label="Daemon version">
        <Show when={version()} fallback={<p class="muted">{version.loading ? "Loading…" : "The daemon did not report its version."}</p>}>
          {(v) => (
            <dl class="kv">
              <dt>Version</dt>
              <dd class="mono">{v().version}</dd>
              <dt>Git ref</dt>
              <dd class="mono">
                {v().git_ref}
                <Show when={v().dirty}>
                  {" "}
                  <span class="chip warn" title="The build tree had changes to tracked files">dirty</span>
                </Show>
                <Show when={v().git_ref === "unknown"}>
                  {" "}
                  <span class="muted small">the build did not know its commit</span>
                </Show>
              </dd>
              <dt>Built</dt>
              <dd class="mono">{v().build_time}</dd>
              <dt>Host agents</dt>
              <dd>
                <Show when={hosts()} fallback={<span class="muted">—</span>}>
                  <Show when={stale().length} fallback={<span class="chip ok">all on the daemon's build</span>}>
                    <span class="chip warn" aria-label="Stale host agents">
                      {stale().length} stale
                    </span>{" "}
                    <For each={stale()}>{(n, i) => <><span class="mono">{n}</span>{i() < stale().length - 1 ? ", " : ""}</>}</For>
                    {" · "}
                    <A href="/settings/hosts">Hosts</A>
                  </Show>
                </Show>
              </dd>
            </dl>
          )}
        </Show>
      </section>

      <PageHead title="Updates" level={2}>
        <button disabled={busy()} onClick={() => void check()}>
          Check now
        </button>
      </PageHead>
      <ErrorBanner error={update.error} />
      <ErrorBanner error={actionError()} onDismiss={() => setActionError(null)} />
      <Show when={notice()}>
        <p class="muted small" role="status">
          {notice()}
        </p>
      </Show>
      <Show when={update()} fallback={<p class="muted">{update.loading ? "Loading…" : "No self-update state."}</p>}>
        {(r) => (
          <>
            <section class="card-box" aria-label="Newest release">
              <dl class="kv">
                <dt>Newest release</dt>
                <dd>
                  <Show when={r().latest} fallback={<span class="muted">unknown{r().check_error ? ` (${r().check_error})` : ""}</span>}>
                    {(l) => (
                      <>
                        <InstanceAnchor class="mono" href={l().url} target="_blank" rel="noreferrer">
                          {l().version}
                        </InstanceAnchor{" "}
                        <Show when={l().prerelease}>
                          <span class="chip info">pre-release</span>{" "}
                        </Show>
                        <Show when={r().update_available} fallback={<span class="chip ok">up to date</span>}>
                          <span class="chip warn">update available</span>
                        </Show>
                      </>
                    )}
                  </Show>
                </dd>
                <dt>Checked</dt>
                <dd class="mono">{r().checked_at ?? "—"}</dd>
                <dt>Policy</dt>
                <dd>
                  <span class="mono">{r().policy}</span> <span class="muted small">(TERRARIUM_SELF_UPDATE)</span>, channel{" "}
                  <span class="mono">{r().channel}</span> <span class="muted small">(TERRARIUM_UPDATE_CHANNEL)</span>
                  <Show when={r().policy === "auto"}>
                    <div class="muted small">An update runs only inside a global calendar event of kind maintenance, once for each.</div>
                  </Show>
                </dd>
                <Show when={r().maintenance}>
                  {(w) => (
                    <>
                      <dt>Maintenance</dt>
                      <dd>
                        {w().title} <span class="muted small mono">until {w().end}</span>
                      </dd>
                    </>
                  )}
                </Show>
                <dt>Updater</dt>
                <dd>
                  <Show
                    when={r().updater.status}
                    fallback={<span class="chip warn" title={r().updater.error ?? ""}>not reachable</span>}
                  >
                    {(s) => (
                      <>
                        <span class="chip ok">reachable</span> <span class="mono small">{s().updater_version}</span>, signature check{" "}
                        <span class={s().verify === "off" ? "chip warn" : "mono"}>{s().verify}</span>
                        <div class="small">
                          Image: <span class="mono">{imageText(s().current)}</span>
                        </div>
                        <Show when={s().previous}>
                          <div class="small">
                            Roll back to: <span class="mono">{imageText(s().previous)}</span>
                          </div>
                        </Show>
                      </>
                    )}
                  </Show>
                </dd>
                <Show when={r().drain.on}>
                  <dt>Drain</dt>
                  <dd>
                    <span class="chip warn">no new turns</span>{" "}
                    <span class="small">{r().drain.busy.length ? `still running: ${r().drain.busy.join(", ")}` : "no turn runs"}</span>
                  </dd>
                </Show>
              </dl>
              <div class="row">
                <ConfirmButton
                  class="primary"
                  label={`Update to ${updateTarget(r())}`}
                  confirmLabel="Yes, update"
                  disabled={busy() || blockedReason(r()) !== null}
                  onConfirm={() => act("The update", () => api.selfUpdate.apply(updateTarget(r())))}
                />
                <ConfirmButton
                  label="Roll back"
                  confirmLabel="Yes, roll back and restore the backup"
                  disabled={busy() || !canRollback(r())}
                  onConfirm={() => act("The rollback", () => api.selfUpdate.rollback())}
                />
                <Show when={blockedReason(r())}>
                  <span class="muted small">{blockedReason(r())}</span>
                </Show>
              </div>
              <p class="muted small">
                An update checks the image signature, drains the daemon (no new turns), makes a full backup, and starts the new image. When it does not
                become healthy, the updater puts the old image and the backup back. A rollback also restores the backup of the last update: the data
                since then is lost.
              </p>
              <Show when={r().latest?.notes}>
                <details>
                  <summary>Release notes of {r().latest!.version}</summary>
                  <SafeHtml class="prose" html={renderMarkdown(r().latest!.notes)} />
                </details>
              </Show>
            </section>

            <PageHead title="Update log" level={2} />
            <Show
              when={runs(r()).length}
              fallback={<p class="muted">No update or rollback yet.</p>}
            >
              <div class="table-wrap">
                <table aria-label="Update log">
                  <thead>
                    <tr>
                      <th>Started</th>
                      <th>Kind</th>
                      <th>From</th>
                      <th>To</th>
                      <th>Outcome</th>
                      <th>By</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={runs(r())}>
                      {(run) => (
                        <>
                          <tr>
                            <td class="mono small">{run.started_at}</td>
                            <td>{run.kind}</td>
                            <td class="mono small">{imageText(run.from)}</td>
                            <td class="mono small">{imageText(run.to) === "—" ? run.target : imageText(run.to)}</td>
                            <td>
                              <span class={outcomeChip(run.outcome).cls}>{outcomeChip(run.outcome).label}</span>
                            </td>
                            <td class="mono small">{run.actor}</td>
                          </tr>
                          <tr>
                            <td colSpan={6}>
                              <details>
                                <summary class="small">
                                  {run.steps.length} steps{run.error ? `: ${run.error}` : ""}
                                </summary>
                                <ol class="small">
                                  <For each={run.steps}>
                                    {(s) => (
                                      <li>
                                        <span class="mono">{s.name}</span>{" "}
                                        <span class={s.state === "failed" ? "chip bad" : s.state === "skipped" ? "chip" : s.state === "ok" ? "chip ok" : "chip info"}>
                                          {s.state}
                                        </span>{" "}
                                        {s.detail}
                                      </li>
                                    )}
                                  </For>
                                </ol>
                              </details>
                            </td>
                          </tr>
                        </>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </Show>
          </>
        )}
      </Show>
    </>
  );
}

/** The run in progress, then the history, newest first. */
function runs(r: { updater: { status: { running: UpdateRun | null; history: UpdateRun[] } | null } }): UpdateRun[] {
  const s = r.updater.status;
  if (!s) return [];
  return s.running ? [s.running, ...s.history.filter((h) => h.id !== s.running!.id)] : s.history;
}

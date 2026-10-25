// Targets (PLAN §17, §19): kind, host, state, the lifecycle verbs, and an
// exec console. Coded to the daemon's `api/targets.rs`: `POST .../up|down|
// snapshot|restore`, `POST .../exec` with `{shell, pty}`, and the session
// WebSocket `.../sessions/{id}/ws` with the agent PTY protocol. These are
// new and not in docs/api.md yet; the mock serves the same shapes.

import { createResource, createSignal, For, Index, onCleanup, Show } from "solid-js";
import { GrantDialog, grantNotice, LeaseTable, useLeaseList } from "../../components/Leases";
import { MockOnlyNote } from "../../components/MockOnly";
import { Terminal } from "../../components/Terminal";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { Target } from "../../api/types";
import type { TargetAction } from "../../api/client";
import { api, leases as holds } from "../../stores";
import { STATE_CHIP, targetActions } from "./actions";

export default function TargetsPage() {
  const [targets, { refetch }] = createResource(() => api.targets.list());
  const [hosts] = createResource(() => api.hosts.list().catch(() => []));
  const [error, setError] = createSignal<unknown>(null);
  const [consoleFor, setConsole] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  // `target:verb` of the verb that runs now. A verb runs to its end before the daemon answers.
  const [busy, setBusy] = createSignal<string | null>(null);

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

  return (
    <>
      <PageHead title="Targets">
        <button onClick={() => void refetch()}>Refresh</button>
      </PageHead>
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
          <table>
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
            <tbody>
              <Index each={targets.latest} fallback={<tr><td colSpan={7}><Empty>{targets.loading ? "Loading…" : "No targets. Define them in targets/<name>.toml."}</Empty></td></tr>}>
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
                    <td>{t().project ?? "global"}</td>
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
                    <td colSpan={7}>
                      <TargetLeases target={t()} onNotice={setNotice} />
                    </td>
                  </tr>
                  </>
                )}
              </Index>
            </tbody>
          </table>
        </div>
      </section>
      <Show when={consoleFor()}>{(name) => <ExecConsole target={name()} onClose={() => setConsole(null)} />}</Show>
      <MockOnlyNote label="new API">
        <code>GET /api/targets</code> is in docs/api.md. <code>POST /api/targets/{"{name}"}/up|down|snapshot|restore|status</code>, <code>POST .../exec</code>, and the session WebSocket are new in the daemon and not in docs/api.md yet (§14 still says 404). Leases: <code>GET</code>, <code>POST /api/targets/{"{name}"}/leases</code>, <code>PATCH</code>, <code>DELETE .../leases/{"{agent}"}</code> (docs/api.md §17.7).
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

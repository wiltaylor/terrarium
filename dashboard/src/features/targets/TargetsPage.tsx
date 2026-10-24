// Targets (PLAN §17, §19): kind, host, state, the lifecycle verbs, and an
// exec console. Coded to the daemon's `api/targets.rs`: `POST .../up|down|
// snapshot|restore`, `POST .../exec` with `{shell, pty}`, and the session
// WebSocket `.../sessions/{id}/ws` with the agent PTY protocol. These are
// new and not in docs/api.md yet; the mock serves the same shapes.

import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { MockOnlyNote } from "../../components/MockOnly";
import { Terminal } from "../../components/Terminal";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { Target, TargetState } from "../../api/types";
import type { TargetAction } from "../../api/client";
import { api } from "../../stores";

const STATE_CHIP: Record<TargetState, string> = { up: "ok", down: "", starting: "info", stopping: "info", unknown: "" };

/** The verbs a target kind has (PLAN §19): `ssh` and `host` have no lifecycle; only vmlab snapshots. */
export function targetActions(t: Pick<Target, "kind" | "state" | "snapshots">): TargetAction[] {
  if (t.kind === "ssh" || t.kind === "host") return [];
  const out: TargetAction[] = [];
  if (t.state !== "up" && t.state !== "starting") out.push("up");
  if (t.state !== "down" && t.state !== "stopping") out.push("down");
  if (t.kind === "vmlab") {
    out.push("snapshot");
    if (t.snapshots.length) out.push("restore");
  }
  return out;
}

export default function TargetsPage() {
  const [targets, { refetch }] = createResource(() => api.targets.list());
  const [hosts] = createResource(() => api.hosts.list().catch(() => []));
  const [error, setError] = createSignal<unknown>(null);
  const [consoleFor, setConsole] = createSignal<string | null>(null);
  // Follow `starting` and `stopping` to the end.
  const t = setInterval(() => {
    if ((targets.latest ?? []).some((x) => x.state === "starting" || x.state === "stopping")) void refetch();
  }, 1500);
  onCleanup(() => clearInterval(t));

  const act = async (id: string, action: TargetAction) => {
    try {
      await api.targets.action(id, action);
      setError(null);
      void refetch();
    } catch (e) {
      setError(e);
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
              <For each={targets.latest} fallback={<tr><td colSpan={7}><Empty>{targets.loading ? "Loading…" : "No targets. Define them in targets/<name>.toml."}</Empty></td></tr>}>
                {(t) => (
                  <tr>
                    <td>
                      <strong>{t.name}</strong>
                      <Show when={t.ephemeral}>
                        {" "}
                        <span class="chip">ephemeral</span>
                      </Show>
                      <Show when={t.snapshot_before_task}>
                        {" "}
                        <span class="chip" title="Snapshot before each task">
                          snapshot per task
                        </span>
                      </Show>
                    </td>
                    <td>{t.kind}</td>
                    <td>{t.project ?? "global"}</td>
                    <td class="small">
                      <Show when={t.host} fallback={<span class="mono">{t.address ?? "—"}</span>}>
                        <span class="mono">{t.host}</span>{" "}
                        <Show when={hostStatus(t)}>{(s) => <span class={`chip ${s() === "online" ? "ok" : "bad"}`}>{s()}</span>}</Show>
                      </Show>
                    </td>
                    <td>
                      <span class={`chip ${STATE_CHIP[t.state] ?? ""}`}>{t.state}</span>
                    </td>
                    <td class="small">{t.snapshots.join(", ") || "—"}</td>
                    <td>
                      <span class="row" style={{ "flex-wrap": "wrap" }}>
                        <For each={targetActions(t)}>
                          {(a) => (
                            <button class="small" onClick={() => act(t.id, a)}>
                              {a[0]!.toUpperCase() + a.slice(1)}
                            </button>
                          )}
                        </For>
                        <button class="small" onClick={() => setConsole(t.name)}>
                          Console
                        </button>
                      </span>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </section>
      <Show when={consoleFor()}>{(name) => <ExecConsole target={name()} onClose={() => setConsole(null)} />}</Show>
      <MockOnlyNote label="new API">
        <code>GET /api/targets</code> is in docs/api.md. <code>POST /api/targets/{"{name}"}/up|down|snapshot|restore</code>, <code>POST .../exec</code>, and the session WebSocket are new in the daemon and not in docs/api.md yet (§14 still says 404).
      </MockOnlyNote>
    </>
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

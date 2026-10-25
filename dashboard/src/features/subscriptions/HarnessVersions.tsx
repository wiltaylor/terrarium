// The "Harness versions" table: the installed version, the tested range, the
// latest release on npm, and a status chip (GET /api/harnesses?latest=true).
// When npm has a newer release, the box under the table gives the command to
// run on the Docker host, and an "Update on host" button for each online host
// agent with `terrarium_repo_dir` (POST /api/hosts/{name}/harness-update).

import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { CopyButton, ErrorBanner } from "../../components/ui";
import { api } from "../../stores";
import type { HarnessReport } from "../../api/types";
import { harnessStatus, offPin, outOfRange, policyNote, publishedDay, updateCommand, updateHosts, updateNames, withUpdate } from "./harnesses";

const decoder = new TextDecoder();
const decode = (b64: string) => decoder.decode(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)));

/** One run of `harness.update` on a host, as the box shows it. */
interface HostRun {
  host: string;
  log: string;
  done: boolean;
  ok: boolean | null;
  rollout: string | null;
  error: unknown;
}

export function HarnessVersions() {
  const [report, { refetch }] = createResource((_: unknown, info: { refetching?: unknown }) => api.harnesses(info.refetching === true, true));
  const [hosts] = createResource(() => api.hosts.list().catch(() => []));
  const [run, setRun] = createSignal<HostRun | null>(null);
  let stopped = false;
  onCleanup(() => (stopped = true));

  const startOnHost = async (host: string, r: HarnessReport) => {
    const names = updateNames(r);
    setRun({ host, log: "", done: false, ok: null, rollout: null, error: null });
    try {
      const s = await api.hosts.harnessUpdate(host, names);
      let since = 0;
      while (!stopped) {
        const o = await api.hosts.sessionOutput(host, s.session_id, since, 20);
        const text = o.chunks.map((c) => decode(c.data)).join("");
        since = o.next;
        const ok = o.done ? o.end?.type === "exit" && o.end.code === 0 : null;
        setRun((x) => x && { ...x, log: x.log + text, done: o.done, ok, rollout: o.rollout ?? (ok ? s.rollout : null) });
        if (o.done) break;
      }
    } catch (e) {
      setRun((x) => x && { ...x, done: true, ok: false, error: e });
    }
  };

  return (
    <section class="card-box" aria-label="Harness versions" style={{ "margin-bottom": "14px" }}>
      <div class="spread">
        <h3 style={{ margin: 0 }}>
          Harness versions
          <Show when={outOfRange(report.latest).length}>
            {(n) => <span class="chip bad" style={{ "margin-left": "8px" }}>{n()} unsupported</span>}
          </Show>
          <Show when={withUpdate(report.latest).length}>
            {(n) => <span class="chip warn" style={{ "margin-left": "8px" }}>{n()} update{n() > 1 ? "s" : ""} available</span>}
          </Show>
        </h3>
        <button class="small" onClick={() => void refetch(true)} disabled={report.loading}>
          {report.loading ? "Checking…" : "Refresh"}
        </button>
      </div>
      <ErrorBanner error={report.error} />
      <Show when={report.latest}>
        {(r) => (
          <>
            <div class="table-wrap">
              <table aria-label="Harness versions">
                <thead>
                  <tr>
                    <th>Harness</th>
                    <th>Installed</th>
                    <th>Tested range</th>
                    <th>Latest</th>
                    <th>Status</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={r().harnesses}>
                    {(h) => {
                      const s = harnessStatus(h);
                      return (
                        <tr>
                          <td>
                            <strong>{h.harness}</strong>
                            <div class="small muted mono">{h.package}</div>
                          </td>
                          <td class="mono" title={offPin(h) ? `Not the pinned version (${h.pinned})` : `The pinned version`}>
                            {h.installed ?? "—"}
                            <Show when={offPin(h)}>
                              <div class="small muted">pin {h.pinned}</div>
                            </Show>
                          </td>
                          <td class="mono">{h.tested}</td>
                          <td class="mono" title={h.latest_error}>
                            <Show when={h.latest} fallback={<span class="muted">{h.latest_error ? "npm not reachable" : "—"}</span>}>
                              {(l) => (
                                <>
                                  {l().version}
                                  <Show when={publishedDay(l().published_at)}>{(d) => <div class="small muted">published {d()}</div>}</Show>
                                </>
                              )}
                            </Show>
                          </td>
                          <td>
                            <span class={`chip ${s.cls}`} title={s.title} data-status={s.status}>
                              {s.status}
                            </span>
                          </td>
                        </tr>
                      );
                    }}
                  </For>
                </tbody>
              </table>
            </div>
            <Show when={updateCommand(r())}>
              {(cmd) => (
                <div class="card-box" aria-label="Update the harnesses" style={{ margin: "10px 0 0" }}>
                  <div class="small">Run this on the Docker host, in the Terrarium checkout:</div>
                  <div class="row" style={{ gap: "8px", "align-items": "center", margin: "6px 0", "flex-wrap": "wrap" }}>
                    <code class="mono">{cmd()}</code>
                    <CopyButton text={cmd()} label="Copy command" />
                  </div>
                  <div class="small muted">The update rebuilds the image and restarts the stack; the running agents stop gracefully and resume.</div>
                  <Show when={updateHosts(hosts())}>
                    {(hs) => (
                      <Show when={hs().length > 0}>
                        <div class="row" style={{ gap: "8px", "margin-top": "8px", "flex-wrap": "wrap" }}>
                          <For each={hs()}>
                            {(h) => (
                              <button class="small" disabled={!!run() && !run()!.done} onClick={() => void startOnHost(h.name, r())}>
                                Update on host {h.name}
                              </button>
                            )}
                          </For>
                        </div>
                      </Show>
                    )}
                  </Show>
                  <Show when={run()}>
                    {(x) => (
                      <div style={{ "margin-top": "8px" }} aria-label="Update output">
                        <div class="small">
                          {x().done ? (x().ok ? `The update on ${x().host} finished.` : `The update on ${x().host} failed.`) : `Updating on ${x().host}…`}
                        </div>
                        <ErrorBanner error={x().error} />
                        <Show when={x().log}>
                          <pre class="mono small" style={{ "max-height": "260px", overflow: "auto" }}>{x().log}</pre>
                        </Show>
                        <Show when={x().rollout}>
                          {(msg) => (
                            <div class="row" style={{ gap: "8px", "align-items": "center", "flex-wrap": "wrap" }}>
                              <span class="chip warn">{msg()}</span>
                              <CopyButton text="just up" label="Copy just up" />
                            </div>
                          )}
                        </Show>
                      </div>
                    )}
                  </Show>
                </div>
              )}
            </Show>
            <p class="muted small" style={{ margin: "6px 0 0" }}>
              {policyNote(r())} Pins live in harnesses.toml; see docs/getting-started.md, "Updating a harness".
            </p>
          </>
        )}
      </Show>
    </section>
  );
}

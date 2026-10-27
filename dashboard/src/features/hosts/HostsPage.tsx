// Host agents (PLAN §19, docs/host-protocol.md): online or offline, the
// capabilities each host reported in `hello`, last seen, add with a token
// that shows once, and revoke. The running targets of each host against
// its `max_running_targets` (docs/api.md §17.3), counted as the daemon
// counts them, and `work_dir` and `instances_dir` when the API reports them.
// "Add host" makes an enrol token and shows the one line that installs the
// host agent on the machine (`GET /install/host.sh`, docs/api.md §17.9).
//
// `GET`, `POST /api/hosts` and `DELETE /api/hosts/{name}` (the daemon's
// `api/hosts.rs`). They are new and not in docs/api.md yet.

import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { MockOnlyNote } from "../../components/MockOnly";
import { ConfirmButton, CopyButton, Empty, ErrorBanner, Modal, PageHead, useNow } from "../../components/ui";
import type { Host, NewHostToken } from "../../api/types";
import { ago } from "../../lib/format";
import { INSTALL_NOTES, installLine } from "../../lib/host-install";
import { runningLabel, runningOnHost } from "../../lib/target-runtime";
import { api } from "../../stores";
import { hostDisplayLabel, hostDisplayOn } from "../targets/display";

/** The chips of a host's capabilities: `docker 29.8.0`, `vmlab 0.30.0-alpha`. */
export function capabilityChips(h: Pick<Host, "capabilities">): { label: string; present: boolean }[] {
  const c = h.capabilities ?? {};
  const out: { label: string; present: boolean }[] = [];
  for (const tool of ["docker", "vmlab"] as const) {
    const t = c[tool];
    if (t) out.push({ label: t.present ? `${tool}${t.version ? ` ${t.version}` : ""}` : `no ${tool}`, present: t.present });
  }
  return out;
}

export default function HostsPage() {
  const now = useNow(15_000);
  const [hosts, { refetch: refetchHosts }] = createResource(() => api.hosts.list());
  // The targets, for the running count of each host. A caller that cannot list them sees no count.
  const [targets, { refetch: refetchTargets }] = createResource(() => api.targets.list().catch(() => null));
  const refetch = () => {
    void refetchHosts();
    void refetchTargets();
  };
  const [error, setError] = createSignal<unknown>(null);
  const [adding, setAdding] = createSignal(false);
  const t = setInterval(() => refetch(), 15_000);
  onCleanup(() => clearInterval(t));

  const revoke = async (name: string) => {
    try {
      await api.hosts.revoke(name);
      setError(null);
      refetch();
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Hosts">
        <button onClick={() => refetch()}>Refresh</button>
        <button class="primary" onClick={() => setAdding(true)}>
          Add host
        </button>
      </PageHead>
      <ErrorBanner error={error() ?? hosts.error} onDismiss={() => setError(null)} />
      <section class="card-box">
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Host</th>
                <th>Status</th>
                <th>Platform</th>
                <th>Capabilities</th>
                <th title="Docker and vmlab targets (and lab instances) that run now, against max_running_targets">Running targets</th>
                <th>Agent version</th>
                <th>Last seen</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={hosts.latest} fallback={<tr><td colSpan={8}><Empty>{hosts.loading ? "Loading…" : "No host agents. Add one, then install terrarium-host on the machine."}</Empty></td></tr>}>
                {(h) => (
                  <tr>
                    <td>
                      <strong>{h.name}</strong>
                      <Show when={h.work_dir}>
                        <div class="small muted">
                          work <span class="mono">{h.work_dir}</span>
                        </div>
                      </Show>
                      <Show when={h.instances_dir}>
                        <div class="small muted">
                          instances <span class="mono">{h.instances_dir}</span>
                        </div>
                      </Show>
                    </td>
                    <td>
                      <span class={`chip ${h.status === "online" ? "ok" : "bad"}`}>{h.status}</span>
                    </td>
                    <td class="small">{h.os && h.arch ? `${h.os}/${h.arch}` : "—"}</td>
                    <td>
                      <span class="row" style={{ gap: "4px", "flex-wrap": "wrap" }}>
                        <For each={capabilityChips(h)} fallback={<span class="muted small">not reported</span>}>
                          {(c) => <span class={`chip ${c.present ? "ok" : ""}`}>{c.label}</span>}
                        </For>
                        <Show when={h.capabilities?.shell}>
                          <span class="chip mono">{h.capabilities!.shell}</span>
                        </Show>
                        <span
                          class={`chip ${hostDisplayOn(h) ? "warn" : ""}`}
                          aria-label={`Desktop display of ${h.name}`}
                          title={
                            hostDisplayOn(h)
                              ? "The host agent's config opens its own desktop to desktop targets ([display] enabled = true)"
                              : "The desktop of this host is closed. Only the host agent's own config can open it"
                          }
                        >
                          {hostDisplayLabel(h)}
                        </span>
                        <Show when={h.capabilities?.display?.tesseract}>
                          <span class="chip ok" title="tesseract on the host: OCR for the displays that have none">ocr</span>
                        </Show>
                        <Show when={h.capabilities?.display?.tools?.length}>
                          <span class="chip mono small" title="The desktop tools on the host">
                            {h.capabilities!.display!.tools!.join(" ")}
                          </span>
                        </Show>
                      </span>
                    </td>
                    <td class="small" aria-label={`Running targets on ${h.name}`}>
                      <Show when={targets()} fallback={<span class="muted">{h.max_running_targets !== undefined ? `max ${h.max_running_targets}` : "—"}</span>}>
                        {(list) => {
                          const n = () => runningOnHost(list(), h.name);
                          const full = () => h.max_running_targets !== undefined && n() >= h.max_running_targets;
                          return (
                            <span class={`chip ${full() ? "warn" : ""}`} title={full() ? "At the cap: an up of another target is 429" : undefined}>
                              {runningLabel(n(), h)}
                            </span>
                          );
                        }}
                      </Show>
                    </td>
                    <td class="mono small">{h.version ?? "—"}</td>
                    <td>
                      {h.status === "online" ? "now" : h.last_seen ? ago(h.last_seen, now()) : "never"}
                      <Show when={h.sessions}>
                        {" "}
                        <span class="muted small">· {h.sessions} sessions</span>
                      </Show>
                    </td>
                    <td>
                      <ConfirmButton class="small danger" label="Revoke" confirmLabel="Yes, revoke the token" onConfirm={() => revoke(h.name)} />
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </section>
      <MockOnlyNote label="new API">
        <code>GET</code>, <code>POST /api/hosts</code> and <code>DELETE /api/hosts/{"{name}"}</code> are new in the daemon (<code>api/hosts.rs</code>) and not in docs/api.md yet. The mock serves the same shapes.
      </MockOnlyNote>
      <Show when={adding()}>
        <AddHostDialog
          onClose={() => {
            setAdding(false);
            refetch();
          }}
        />
      </Show>
    </>
  );
}

function AddHostDialog(props: { onClose: () => void }) {
  const [name, setName] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  // The token lives in this dialog only. Closing it drops the token for good.
  const [made, setMade] = createSignal<NewHostToken | null>(null);

  const submit = async (e: Event) => {
    e.preventDefault();
    try {
      // An enrol token: one use, 10 minutes. The host agent swaps it for its
      // long-lived token, so that token never passes through the clipboard.
      setMade(await api.hosts.create(name().trim(), { enrol: true }));
      setError(null);
    } catch (err) {
      setError(err);
    }
  };

  return (
    <Modal title="Add host" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show
        when={made()}
        fallback={
          <form class="stack" onSubmit={submit}>
            <label>
              Host name (the <code>host</code> of target definitions)
              <input value={name()} onInput={(e) => setName(e.currentTarget.value)} placeholder="lab1" required autocomplete="off" />
            </label>
            <p class="small muted" style={{ margin: 0 }}>
              The next step shows one command. Run it on the machine to install <code>terrarium-host</code>.
            </p>
            <div>
              <button class="primary" type="submit" disabled={!name().trim()}>
                Make install command
              </button>
            </div>
          </form>
        }
      >
        {(m) => {
          const line = () => installLine(m(), window.location.origin);
          return (
            <div class="stack">
              <p class="notice" style={{ margin: 0 }}>
                Run this on <strong>{m().name}</strong>. It shows <strong>once</strong>: the daemon keeps only a hash of the token.
                <Show when={m().token_expires_at}>{(at) => <> The token works one time, until {new Date(at()).toLocaleTimeString()}.</>}</Show>
              </p>
              <div class="copy-row">
                <code class="token-once" aria-label="Install command">
                  {line()}
                </code>
                <CopyButton text={line()} label="Copy command" />
              </div>
              <ul class="small muted" style={{ margin: 0, "padding-left": "1.2em" }}>
                <For each={INSTALL_NOTES}>{(n) => <li>{n}</li>}</For>
              </ul>
              <details class="small">
                <summary>Other ways to install</summary>
                <p style={{ margin: "0.5em 0" }}>
                  From the repo, over ssh: <code>just host::install user@{m().name} {m().name}</code> (it gets its own token). By hand: store the token with
                  mode 0600, then set <code>name = "{m().name}"</code> and the daemon URL in <code>/etc/terrarium-host/config.toml</code>, and start the unit.
                </p>
                <div class="copy-row">
                  <code class="token-once" aria-label="Enrol token">
                    {m().token}
                  </code>
                  <CopyButton text={m().token} label="Copy token" />
                </div>
                <pre class="small">{`echo '<token>' | sudo -u <user> terrarium-host token-file /etc/terrarium-host/token`}</pre>
              </details>
              <p class="small muted" style={{ margin: 0 }}>
                The host shows online in this list when it connects.
              </p>
              <div>
                <button class="primary" onClick={() => props.onClose()}>
                  Done
                </button>
              </div>
            </div>
          );
        }}
      </Show>
    </Modal>
  );
}

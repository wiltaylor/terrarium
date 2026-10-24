// Host agents (PLAN §19, docs/host-protocol.md): online or offline, the
// capabilities each host reported in `hello`, last seen, add with a token
// that shows once, and revoke.
//
// `GET`, `POST /api/hosts` and `DELETE /api/hosts/{name}` (the daemon's
// `api/hosts.rs`). They are new and not in docs/api.md yet.

import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { MockOnlyNote } from "../../components/MockOnly";
import { ConfirmButton, CopyButton, Empty, ErrorBanner, Modal, PageHead, useNow } from "../../components/ui";
import type { Host, NewHostToken } from "../../api/types";
import { ago } from "../../lib/format";
import { api } from "../../stores";

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
  const [hosts, { refetch }] = createResource(() => api.hosts.list());
  const [error, setError] = createSignal<unknown>(null);
  const [adding, setAdding] = createSignal(false);
  const t = setInterval(() => void refetch(), 15_000);
  onCleanup(() => clearInterval(t));

  const revoke = async (name: string) => {
    try {
      await api.hosts.revoke(name);
      setError(null);
      void refetch();
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Hosts">
        <button onClick={() => void refetch()}>Refresh</button>
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
                <th>Agent version</th>
                <th>Last seen</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={hosts.latest} fallback={<tr><td colSpan={7}><Empty>{hosts.loading ? "Loading…" : "No host agents. Add one, then install terrarium-host on the machine."}</Empty></td></tr>}>
                {(h) => (
                  <tr>
                    <td>
                      <strong>{h.name}</strong>
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
                      </span>
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
            void refetch();
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
      setMade(await api.hosts.create(name().trim()));
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
            <div>
              <button class="primary" type="submit" disabled={!name().trim()}>
                Make token
              </button>
            </div>
          </form>
        }
      >
        {(m) => (
          <div class="stack">
            <p class="notice" style={{ margin: 0 }}>
              Copy the token now. It shows <strong>once</strong>: the daemon keeps only a hash of it.
            </p>
            <div class="copy-row">
              <code class="token-once" aria-label="Host token">
                {m().token}
              </code>
              <CopyButton text={m().token} label="Copy token" />
            </div>
            <p class="small muted" style={{ margin: 0 }}>
              On <strong>{m().name}</strong>, store it with mode 0600:
            </p>
            <pre class="small">{`echo '<token>' | sudo terrarium-host token-file /etc/terrarium-host/token`}</pre>
            <p class="small muted" style={{ margin: 0 }}>
              Then set <code>name = "{m().name}"</code> and the daemon URL in <code>/etc/terrarium-host/config.toml</code>, and start the unit. The host shows online when it connects.
            </p>
            <div>
              <button class="primary" onClick={() => props.onClose()}>
                Done
              </button>
            </div>
          </div>
        )}
      </Show>
    </Modal>
  );
}

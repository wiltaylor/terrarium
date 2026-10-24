import { createResource, createSignal, For, Show } from "solid-js";
import { ErrorBanner, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";

export default function TargetsPage() {
  const [targets, { refetch }] = createResource(() => api.targets.list());
  const [hosts] = createResource(() => api.hosts.list());
  const [error, setError] = createSignal<unknown>(null);

  const act = async (id: string, action: "up" | "down" | "snapshot" | "restore") => {
    try {
      await api.targets.action(id, action);
      setError(null);
      void refetch();
    } catch (e) {
      // Target actions are not in the daemon yet (docs/api.md §10); the banner says so.
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Targets and hosts" />
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <section class="card-box" style={{ "margin-bottom": "16px" }}>
        <h2>Targets</h2>
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
              <For each={targets()}>
                {(t) => (
                  <tr>
                    <td>
                      <strong>{t.name}</strong>
                      <Show when={t.ephemeral}>
                        {" "}
                        <span class="chip">ephemeral</span>
                      </Show>
                    </td>
                    <td>{t.kind}</td>
                    <td>{t.project ?? "global"}</td>
                    <td class="mono small">{t.host ?? t.address ?? "—"}</td>
                    <td>
                      <span class={`chip ${t.state === "up" ? "ok" : t.state === "down" ? "" : "info"}`}>{t.state}</span>
                    </td>
                    <td class="small">{t.snapshots.join(", ") || "—"}</td>
                    <td class="row">
                      <Show when={t.kind !== "ssh"}>
                        <button class="small" disabled={t.state === "up"} onClick={() => act(t.id, "up")}>
                          Up
                        </button>
                        <button class="small" disabled={t.state === "down"} onClick={() => act(t.id, "down")}>
                          Down
                        </button>
                      </Show>
                      <Show when={t.kind === "vmlab"}>
                        <button class="small" onClick={() => act(t.id, "snapshot")}>
                          Snapshot
                        </button>
                      </Show>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </section>
      <section class="card-box">
        <h2>Host agents</h2>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Host</th>
                <th>State</th>
                <th>Platform</th>
                <th>Capabilities</th>
                <th>Version</th>
                <th>Last seen</th>
              </tr>
            </thead>
            <tbody>
              <For each={hosts()}>
                {(h) => (
                  <tr>
                    <td>
                      <strong>{h.name}</strong>
                    </td>
                    <td>
                      <span class={`chip ${h.connected ? "ok" : "bad"}`}>{h.connected ? "connected" : "offline"}</span>
                    </td>
                    <td>
                      {h.os}/{h.arch}
                    </td>
                    <td>{[h.capabilities.docker && "docker", h.capabilities.vmlab && "vmlab"].filter(Boolean).join(", ") || "exec only"}</td>
                    <td class="mono small">{h.version}</td>
                    <td>{ago(h.last_seen)}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </section>
    </>
  );
}

// The grants of one secret (PLAN §16b.3, docs/api.md §5.2): the list, a
// new grant, and a revoke. The grantee picker offers what the scope allows:
// any project or agent for a global secret; the project and its agents for
// a project secret. The level starts at the kind's default; an env var
// needs the level read.

import { createResource, createSignal, For, onMount, Show } from "solid-js";
import type { GrantLevel, Secret } from "../../api/types";
import { ConfirmButton, ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { agents, api, projects } from "../../stores";
import { defaultLevel, emptyGrant, envAllowed, grantBody, granteeOptions, type GrantForm } from "./model";

export function GrantsPanel(props: { secret: Secret; onClose: () => void }) {
  const s = () => props.secret;
  const [list, { refetch }] = createResource(
    () => ({ scope: s().scope, name: s().name }),
    (k) => api.secrets.grants(k.scope, k.name),
  );
  const [form, setForm] = createSignal<GrantForm>(emptyGrant(s()));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  onMount(() => {
    if (!agents.state.agents.length) void agents.refresh();
  });
  const options = () => granteeOptions(s(), projects.projects(), agents.state.agents);
  const set = (patch: Partial<GrantForm>) => setForm((g) => ({ ...g, ...patch }));

  const add = async (e: Event) => {
    e.preventDefault();
    const b = grantBody(form());
    if ("error" in b) return setError(new Error(b.error));
    setBusy(true);
    try {
      await api.secrets.grant(s().scope, s().name, b.body);
      setForm(emptyGrant(s()));
      setError(null);
      void refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  const revoke = async (grantee: string) => {
    try {
      await api.secrets.revoke(s().scope, s().name, grantee);
      setError(null);
      void refetch();
    } catch (err) {
      setError(err);
    }
  };

  return (
    <div class="stack" style={{ gap: "8px" }} role="region" aria-label={`Grant panel of ${s().scope}/${s().name}`}>
      <div class="spread">
        <strong class="small">Grants of {s().scope}/{s().name}</strong>
        <button class="small ghost" type="button" aria-label="Close grants" onClick={() => props.onClose()}>
          ✕
        </button>
      </div>
      <p class="small muted" style={{ margin: 0 }}>
        use: the brokers only (the git credential helper and the ssh-agent); the agent cannot see the value. read: also an env var at start and <code>terrarium secret get</code>. A change applies at the next start of the agent; a revoke stops the brokers at once.
      </p>
      <ErrorBanner error={error() ?? list.error} onDismiss={() => setError(null)} />
      <Show when={list()?.length} fallback={<p class="small muted" style={{ margin: 0 }}>{list.loading ? "Loading…" : "No grants. No agent gets this secret."}</p>}>
        <div class="table-wrap">
          <table aria-label="Grants">
            <thead>
              <tr>
                <th>Grantee</th>
                <th>Level</th>
                <th>Env var</th>
                <th>By</th>
                <th>Since</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={list()}>
                {(g) => (
                  <tr>
                    <td class="mono small">{g.grantee_label}</td>
                    <td>
                      <span class={`chip ${g.level === "read" ? "warn" : ""}`}>{g.level}</span>
                    </td>
                    <td class="mono small">{g.env ?? <span class="muted">—</span>}</td>
                    <td class="small">{g.created_by}</td>
                    <td class="small" title={g.created_at}>
                      {ago(g.created_at, Date.now())}
                    </td>
                    <td>
                      <ConfirmButton class="small danger" label="Revoke" onConfirm={() => revoke(g.grantee_label)} />
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <form class="row" style={{ gap: "8px", "flex-wrap": "wrap", "align-items": "flex-end" }} aria-label="Add grant" onSubmit={add}>
        <label class="small">
          Grantee
          <select aria-label="Grantee" value={form().grantee} onChange={(e) => set({ grantee: e.currentTarget.value })}>
            <option value="" disabled selected={!form().grantee}>
              Pick an agent or a project
            </option>
            <For each={["Projects", "Agents"] as const}>
              {(group) => (
                <optgroup label={group}>
                  <For each={options().filter((o) => o.group === group)}>
                    {(o) => (
                      <option value={o.value} selected={o.value === form().grantee}>
                        {o.label}
                      </option>
                    )}
                  </For>
                </optgroup>
              )}
            </For>
          </select>
        </label>
        <label class="small">
          Level
          <select aria-label="Level" value={form().level} onChange={(e) => set({ level: e.currentTarget.value as GrantLevel })}>
            <For each={["use", "read"] as const}>
              {(l) => (
                <option value={l} selected={l === form().level}>
                  {l}
                  {l === defaultLevel(s().kind) ? " (default)" : ""}
                </option>
              )}
            </For>
          </select>
        </label>
        <label class="small" title={envAllowed(form().level) ? "" : "An env var needs the level read: the agent can print it"}>
          Env var
          <input aria-label="Env var" class="mono" value={envAllowed(form().level) ? form().env : ""} disabled={!envAllowed(form().level)} placeholder={envAllowed(form().level) ? "DATABASE_URL" : "needs read"} onInput={(e) => set({ env: e.currentTarget.value })} />
        </label>
        <button class="primary small" type="submit" disabled={busy() || !form().grantee}>
          {busy() ? "Granting…" : "Grant"}
        </button>
      </form>
    </div>
  );
}

// Settings > Tools (GitHub #21, design D7, docs/api.md §17.11): the tool
// proxies with their definitions and grants, grant and revoke, the delete
// of an API tool, and the recent runs (the `tool.run` audit rows). The
// operator and sysadmins manage tools; a definition comes from
// `tools/<name>.toml` in the config repo or `POST /api/tools`. Secret
// values never reach the page: `env_secrets` holds names.

import { createResource, createSignal, For, onMount, Show } from "solid-js";
import type { Tool } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { agents, api, projects } from "../../stores";
import { approvalText, argsText, commandLine, granteeOptions, runRows } from "./model";

export default function ToolsPage() {
  const [tools, { refetch }] = createResource(() => api.tools.list());
  const [runs] = createResource(() => api.audit({ action: "tool.run", order: "desc", limit: 20 }).then((p) => runRows(p.items)).catch(() => []));
  const [error, setError] = createSignal<unknown>(null);
  onMount(() => {
    if (!agents.state.agents.length) void agents.refresh();
  });
  const changed = () => {
    setError(null);
    void refetch();
  };
  return (
    <div class="stack">
      <p class="muted small" style={{ margin: 0 }}>
        A tool is one command on a target that a granted agent runs as a CLI in its sandbox, without the target. Define tools in <code>tools/&lt;name&gt;.toml</code> of the config
        repo or with <code>POST /api/tools</code>. A tool is only as narrow as the command it runs.
      </p>
      <ErrorBanner error={error() ?? tools.error} onDismiss={() => setError(null)} />
      <Show when={tools()} fallback={<Empty>{tools.loading ? "Loading…" : "No tools."}</Empty>}>
        {(list) => (
          <Show when={list().length} fallback={<Empty>No tools. Add a file under tools/ in the config repo, then reload.</Empty>}>
            <div class="stack">
              <For each={list()}>{(t) => <ToolCard tool={t} onChange={changed} onError={setError} />}</For>
            </div>
          </Show>
        )}
      </Show>
      <section class="card-box stack" style={{ gap: "6px" }} aria-label="Recent runs">
        <h3 style={{ margin: 0 }}>Recent runs</h3>
        <Show when={runs()?.length} fallback={<p class="muted small" style={{ margin: 0 }}>No runs yet.</p>}>
          <table class="small">
            <thead>
              <tr>
                <th>When</th>
                <th>Tool</th>
                <th>Agent</th>
                <th>Arguments</th>
                <th>Exit</th>
                <th>Time</th>
              </tr>
            </thead>
            <tbody>
              <For each={runs()}>
                {(r) => (
                  <tr>
                    <td title={r.at}>{ago(r.at)}</td>
                    <td class="mono">{r.tool}</td>
                    <td>{r.agent}</td>
                    <td class="mono">{r.argv}</td>
                    <td>
                      {r.exit}
                      <Show when={r.truncated}>
                        {" "}
                        <span class="chip warn">truncated</span>
                      </Show>
                    </td>
                    <td>{r.duration}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Show>
      </section>
    </div>
  );
}

/** One tool: its definition, its grants with revoke, a new grant, and Delete for an API tool. */
export function ToolCard(props: { tool: Tool; onChange: () => void; onError: (e: unknown) => void }) {
  const t = () => props.tool;
  const [who, setWho] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const options = () => granteeOptions(t(), projects.projects(), agents.state.agents);
  const run = async (call: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await call();
      props.onChange();
    } catch (e) {
      props.onError(e);
    } finally {
      setBusy(false);
    }
  };
  const grant = (e: Event) => {
    e.preventDefault();
    const g = who() || options()[0]?.value;
    if (!g) return;
    void run(() => api.tools.grant(t().name, g)).then(() => setWho(""));
  };
  return (
    <article class="card-box stack" style={{ gap: "6px" }} aria-label={`Tool ${t().name}`}>
      <div class="spread">
        <span class="row" style={{ gap: "6px", "flex-wrap": "wrap" }}>
          <strong class="mono">{t().name}</strong>
          <span class="chip">{t().project ?? "global"}</span>
          <span class="chip" title={t().source === "file" ? `tools/${t().name}.toml in the config repo` : "made through the API"}>
            {t().source}
          </span>
          <Show when={t().description}>
            <span class="muted small">{t().description}</span>
          </Show>
        </span>
        <Show when={t().source === "api"}>
          <ConfirmButton class="small" label="Delete" confirmLabel="Yes, delete the tool and its grants" disabled={busy()} onConfirm={() => run(() => api.tools.remove(t().name))} />
        </Show>
      </div>
      <dl class="kv small">
        <dt>Target</dt>
        <dd class="mono">{t().target}</dd>
        <dt>Command</dt>
        <dd class="mono">{commandLine(t().command)}</dd>
        <dt>Arguments</dt>
        <dd>{argsText(t().args)}</dd>
        <dt>Approval</dt>
        <dd>{approvalText(t().approval)}</dd>
        <dt>Stdin</dt>
        <dd>{t().stdin ? "passed" : "closed"}</dd>
        <Show when={Object.keys(t().env_secrets ?? {}).length}>
          <dt>Secrets</dt>
          <dd class="mono">
            {Object.entries(t().env_secrets ?? {})
              .map(([env, s]) => `${env} = ${s}`)
              .join(", ")}
          </dd>
        </Show>
      </dl>
      <div class="stack" style={{ gap: "4px" }}>
        <span class="small">Granted to</span>
        <Show when={t().grants?.length} fallback={<span class="muted small">nobody</span>}>
          <ul class="small" style={{ margin: 0, "padding-left": "1.2em" }}>
            <For each={t().grants}>
              {(g) => (
                <li class="row" style={{ gap: "6px" }}>
                  <span>{g.grantee_label}</span>
                  <span class="muted">by {g.created_by}</span>
                  <ConfirmButton class="small" label="Revoke" confirmLabel="Yes, revoke" disabled={busy()} onConfirm={() => run(() => api.tools.revoke(t().name, g.grantee))} />
                </li>
              )}
            </For>
          </ul>
        </Show>
        <Show when={options().length}>
          <form class="row small" style={{ gap: "6px" }} onSubmit={grant} aria-label={`Grant ${t().name}`}>
            <select aria-label="Grantee" value={who() || options()[0]?.value} onChange={(e) => setWho(e.currentTarget.value)}>
              <For each={options()}>
                {(o) => (
                  <option value={o.value} selected={o.value === (who() || options()[0]?.value)}>
                    {o.label}
                  </option>
                )}
              </For>
            </select>
            <button class="small primary" type="submit" disabled={busy()}>
              Grant
            </button>
          </form>
        </Show>
      </div>
    </article>
  );
}

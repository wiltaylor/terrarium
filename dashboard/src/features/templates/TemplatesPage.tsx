// Agent templates (docs/api.md §21): the global templates and those of each
// project, with a scope filter. Create, edit, and delete (an in-page
// confirm). `template` events keep the list fresh.

import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { AgentTemplate } from "../../api/types";
import { ago } from "../../lib/format";
import { api, events, projects } from "../../stores";
import { pathKey, scopeLabel } from "./model";
import { TemplateDialog } from "./TemplateDialog";

export default function TemplatesPage() {
  // "all", "global", or a project name.
  const [scope, setScope] = createSignal("all");
  const [dialog, setDialog] = createSignal<{ template?: AgentTemplate } | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [list, { refetch }] = createResource(() => api.templates.list().catch((e: unknown) => (setError(e), [] as AgentTemplate[])));
  const shown = () => (list() ?? []).filter((t) => scope() === "all" || (scope() === "global" ? t.project === null : t.project === scope()));

  onMount(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind !== "template") return;
      clearTimeout(timer);
      timer = setTimeout(() => void refetch(), 300);
    });
    onCleanup(() => {
      off();
      clearTimeout(timer);
    });
  });

  const remove = async (t: AgentTemplate) => {
    try {
      await api.templates.remove(t.id);
      setError(null);
      void refetch();
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Agent templates">
        <select value={scope()} onChange={(e) => setScope(e.currentTarget.value)} aria-label="Scope">
          <option value="all">All scopes</option>
          <option value="global">Global</option>
          <For each={projects.projects()}>{(p) => <option value={p.name}>Project: {p.name}</option>}</For>
        </select>
        <button onClick={() => void refetch()}>Refresh</button>
        <button class="primary" onClick={() => setDialog({})}>
          New template
        </button>
      </PageHead>
      <p class="muted small">
        A template holds every agent setting except the name and the project. Pick one in the New agent form, or use <code>terrarium agent new &lt;name&gt; --template &lt;template&gt;</code>. A project template wins over a global one with the same name.
      </p>
      <Show when={dialog()}>
        {(d) => (
          <TemplateDialog
            template={d().template}
            scope={scope() === "all" || scope() === "global" ? "" : scope()}
            onClose={() => setDialog(null)}
            onSaved={() => void refetch()}
          />
        )}
      </Show>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={shown().length} fallback={<Empty>{list.loading ? "Loading…" : "No templates match."}</Empty>}>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Scope</th>
                <th>Harness</th>
                <th>Model</th>
                <th>Prompt</th>
                <th>Roles</th>
                <th>Rev.</th>
                <th>Source</th>
                <th>Updated</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={shown()}>
                {(t) => (
                  <tr>
                    <td>
                      <strong>{t.name}</strong>
                      <Show when={t.description}>
                        <div class="muted small">{t.description}</div>
                      </Show>
                    </td>
                    <td>
                      <span class={`chip ${t.project ? "info" : ""}`}>{scopeLabel(t)}</span>
                    </td>
                    <td>{t.spec.harness}</td>
                    <td class="mono small">{t.spec.model ?? <span class="muted">default</span>}</td>
                    <td>{t.spec.prompt ?? <span class="muted">default</span>}</td>
                    <td>{(t.spec.roles ?? []).join(", ") || <span class="muted">member</span>}</td>
                    <td>{t.revision}</td>
                    <td class="small">{t.source}</td>
                    <td class="small" title={t.updated_at}>
                      {ago(t.updated_at, Date.now())}
                    </td>
                    <td>
                      <span class="row" style={{ gap: "4px" }}>
                        <button class="small" disabled={t.source === "file"} onClick={() => setDialog({ template: t })} aria-label={`Edit ${pathKey(t)}`}>
                          Edit
                        </button>
                        <ConfirmButton class="small danger" label="Delete" disabled={t.source === "file"} onConfirm={() => remove(t)} />
                      </span>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
    </>
  );
}

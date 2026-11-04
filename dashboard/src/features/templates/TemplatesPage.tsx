// Agent templates (docs/api.md §21). Under /settings/templates (a tab of the
// settings screen): the global templates and those of each project, with a
// scope filter. Create, edit, and delete (an in-page
// confirm). `template` events keep the list fresh. Under
// /p/:project/settings/templates (a tab of the project settings screen) the
// page lists the templates that apply to the project: its own, then the
// global ones. A global template that a project template of the same name
// overrides stays in the list with a chip, because the New agent chooser still
// offers it (as `global:<name>`). Each template is a WCL file of the config
// repo: "View WCL" shows the file, read-only, under the table.

import { useParams } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { AgentTemplate, TemplateWcl } from "../../api/types";
import { ago } from "../../lib/format";
import { api, events, projects } from "../../stores";
import { pathKey, scopeLabel, selectable } from "./model";
import { TemplateDialog } from "./TemplateDialog";

export default function TemplatesPage() {
  const route = useParams<{ project?: string }>();
  // "all", "global", or a project name.
  const [scope, setScope] = createSignal("all");
  const [dialog, setDialog] = createSignal<{ template?: AgentTemplate } | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  /** The WCL file on view, with its template key. */
  const [wcl, setWcl] = createSignal<(TemplateWcl & { key: string }) | null>(null);
  const [list, { refetch }] = createResource(() => api.templates.list().catch((e: unknown) => (setError(e), [] as AgentTemplate[])));
  /** The project of the route wins over the scope select. */
  const shown = () => {
    const all = list() ?? [];
    if (route.project) return selectable(all, route.project);
    return all.filter((t) => scope() === "all" || (scope() === "global" ? t.project === null : t.project === scope()));
  };
  /** A global template with the name of a template of the route project. */
  const overridden = (t: AgentTemplate) => !!route.project && t.project === null && shown().some((o) => o.project === route.project && o.name === t.name);

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

  const viewWcl = async (t: AgentTemplate) => {
    try {
      setWcl({ ...(await api.templates.wcl(t.id)), key: pathKey(t) });
      setError(null);
    } catch (e) {
      setError(e);
    }
  };

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
      <PageHead title={route.project ? "Templates" : "Agent templates"} level={2}>
        <Show when={!route.project}>
          <select value={scope()} onChange={(e) => setScope(e.currentTarget.value)} aria-label="Scope">
            <option value="all">All scopes</option>
            <option value="global">Global</option>
            <For each={projects.projects()}>{(p) => <option value={p.name}>Project: {p.name}</option>}</For>
          </select>
        </Show>
        <button onClick={() => void refetch()}>Refresh</button>
        <button class="primary" onClick={() => setDialog({})}>
          New template
        </button>
      </PageHead>
      <p class="muted small">
        A template holds every agent setting except the name and the project. Pick one in the New agent chooser, or use <code>terrarium agent new &lt;name&gt; --template &lt;template&gt;</code>. A project template wins over a global one with the same name. Each template is a WCL file in the config repo (<code>templates/</code>): a save here writes the file, and <code>terrarium admin config commit</code> commits it.
      </p>
      <Show when={dialog()}>
        {(d) => (
          <TemplateDialog
            template={d().template}
            scope={route.project ?? (scope() === "all" || scope() === "global" ? "" : scope())}
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
                      <Show when={overridden(t)}>
                        {" "}
                        <span class="chip warn" title={`The template ${route.project}/${t.name} wins for agents of ${route.project}. Pick the global ${t.name} card in the New agent chooser to use this one.`}>
                          overridden by project
                        </span>
                      </Show>
                    </td>
                    <td>
                      {t.spec.harness}
                      <Show when={t.subscription_missing}>
                        {(why) => (
                          <div>
                            <span class="chip warn" title={why()} data-testid="subscription-missing">
                              {t.spec.subscription} missing
                            </span>
                          </div>
                        )}
                      </Show>
                    </td>
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
                        <button class="small" onClick={() => void viewWcl(t)} aria-label={`View the WCL file of ${pathKey(t)}`}>
                          View WCL
                        </button>
                        <button class="small" onClick={() => setDialog({ template: t })} aria-label={`Edit ${pathKey(t)}`}>
                          Edit
                        </button>
                        <ConfirmButton class="small danger" label="Delete" onConfirm={() => remove(t)} />
                      </span>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={wcl()}>
        {(f) => (
          <section class="card-box stack" aria-label={`WCL file of ${f().key}`}>
            <div class="row" style={{ "justify-content": "space-between" }}>
              <strong class="mono">{f().path}</strong>
              <span class="row" style={{ gap: "4px" }}>
                <Show when={!f().on_disk}>
                  <span class="chip warn" title="The daemon has no config repo: this is the text that the file would have.">
                    not on disk
                  </span>
                </Show>
                <button class="small" onClick={() => setWcl(null)}>
                  Close
                </button>
              </span>
            </div>
            <pre class="mono small" style={{ margin: 0, "white-space": "pre", overflow: "auto", "max-height": "60vh" }}>
              {f().text}
            </pre>
          </section>
        )}
      </Show>
    </>
  );
}

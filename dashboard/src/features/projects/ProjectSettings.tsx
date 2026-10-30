import { A, useNavigate, useParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";
import { agents, api, projects } from "../../stores";
import { ProjectForm } from "./ProjectsPage";
import { ScopeChip } from "../targets/ScopeChip";
import { filterByScope, groupByScope } from "../../lib/target-scope";

/**
 * The General tab of the project settings screen (/p/:project/settings/general):
 * the project, its edit and delete, and the Targets panel. The repos have
 * their own page (features/repos, /p/:project/repos).
 */
export default function ProjectSettings() {
  const params = useParams<{ project: string }>();
  const navigate = useNavigate();
  const [project, { refetch }] = createResource(() => params.project, (p) => api.projects.get(p));
  const [error, setError] = createSignal<unknown>(null);
  const [editing, setEditing] = createSignal(false);

  const remove = async () => {
    try {
      await api.projects.remove(params.project);
      await projects.refresh();
      navigate("/projects");
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="General" level={2}>
        <button onClick={() => setEditing(true)} disabled={!project()}>
          Edit
        </button>
        <ConfirmButton label="Delete project" confirmLabel="Yes, delete the project, its board and repos" onConfirm={remove} />
      </PageHead>
      <ErrorBanner error={error() ?? project.error} onDismiss={() => setError(null)} />
      <Show when={project()}>
        {(p) => (
          <div class="card-box">
            <dl class="kv">
              <dt>Name</dt>
              <dd>{p().name}</dd>
              <dt>Description</dt>
              <dd>{p().description || <span class="muted">—</span>}</dd>
              <dt>Card prefix</dt>
              <dd class="mono">{p().card_prefix}</dd>
              <dt>Source</dt>
              <dd>{p().source ?? "api"}</dd>
              <dt>Agents</dt>
              <dd>{agents.inProject(p().name).length}</dd>
              <dt>Created</dt>
              <dd>{ago(p().created_at)}</dd>
            </dl>
          </div>
        )}
      </Show>
      <TargetsPanel project={params.project} />
      <p class="small muted">
        The repos of the project are on the <A href={`/p/${encodeURIComponent(params.project)}/repos`}>Repos</A> page.
      </p>
      <Show when={editing() && project()}>
        <ProjectForm project={project()!} onClose={() => setEditing(false)} onSaved={() => void refetch()} />
      </Show>
    </>
  );
}

/**
 * The Targets panel: the project's targets and the global ones (`GET
 * /api/projects/{name}/targets`, docs/api.md §17.2), with a link to the
 * Targets page for the project.
 */
export function TargetsPanel(props: { project: string }) {
  const [targets] = createResource(() => props.project, (p) => api.projects.targets(p));
  const own = () => filterByScope(targets() ?? [], "project", props.project).length;
  const href = (scope?: "project" | "global") => `/targets?project=${encodeURIComponent(props.project)}${scope ? `&scope=${scope}` : ""}`;
  return (
    <section class="card-box stack" style={{ "margin-top": "16px" }} aria-label="Targets">
      <div class="spread">
        <h2 style={{ margin: 0 }}>Targets</h2>
        <A href={href()}>Open in Targets</A>
      </div>
      <ErrorBanner error={targets.error} />
      <Show when={targets()?.length} fallback={<Empty>{targets.loading ? "Loading…" : "No targets for this project, and no global ones."}</Empty>}>
        <p class="small muted" style={{ margin: 0 }}>
          <A href={href("project")}>{own()} of {props.project}</A> · <A href={href("global")}>{(targets()?.length ?? 0) - own()} global</A>
        </p>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Kind</th>
                <th>Scope</th>
                <th>State</th>
              </tr>
            </thead>
            <tbody>
              <For each={groupByScope(targets() ?? []).reverse().flatMap((g) => g.targets)}>
                {(t) => (
                  <tr>
                    <td>
                      <strong>{t.name}</strong>
                    </td>
                    <td>{t.kind}</td>
                    <td>
                      <ScopeChip target={t} />
                    </td>
                    <td class="small">{t.state}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
    </section>
  );
}

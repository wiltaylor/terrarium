import { useInstance } from "../../instance";
import { A, useNavigate } from "../../instance/navigation";
import { createSignal, For, Show } from "solid-js";
import { Empty, ErrorBanner, Modal, PageHead } from "../../components/ui";
import type { Project } from "../../api/types";
import { defaultHomeColumns, type HomeColumn } from "../../api/home";
import { ProjectDot } from "../../components/ProjectDot";
import { ago } from "../../lib/format";
import { projectHealth, projectHealthLabel } from "../../lib/projectHealth";


export function ProjectForm(props: { project?: Project; onClose: () => void; onSaved: (p: Project) => void }) {
  const { api, projects } = useInstance();
  const [name, setName] = createSignal(props.project?.name ?? "");
  const [description, setDescription] = createSignal(props.project?.description ?? "");
  const [prefix, setPrefix] = createSignal(props.project?.card_prefix ?? "");
  const [homeColumns, setHomeColumns] = createSignal(props.project?.home_board_columns ?? defaultHomeColumns);
  const [error, setError] = createSignal<unknown>(null);
  return (
    <Modal title={props.project ? `Edit ${props.project.name}` : "New project"} onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form
        class="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          try {
            const p = props.project
              ? await api.projects.update(props.project.name, {
                  description: description(),
                  home_board_columns: homeColumns(),
                  ...(prefix().trim() && prefix().trim() !== props.project.card_prefix ? { card_prefix: prefix().trim() } : {}),
                })
              : await api.projects.create({ name: name().trim(), description: description() || undefined, card_prefix: prefix().trim() || undefined });
            await projects.refresh();
            props.onSaved(p);
            props.onClose();
          } catch (err) {
            setError(err);
          }
        }}
      >
        <label>
          Name
          <input value={name()} disabled={!!props.project} onInput={(e) => setName(e.currentTarget.value)} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" placeholder="ops" />
        </label>
        <label>
          Description
          <input value={description()} onInput={(e) => setDescription(e.currentTarget.value)} placeholder="Operations" />
        </label>
        <label>
          Card prefix {props.project ? "" : "(empty: the daemon makes one from the name)"}
          <input value={prefix()} onInput={(e) => setPrefix(e.currentTarget.value.toUpperCase())} pattern="[A-Z][A-Z0-9]{0,9}" placeholder="OPS" />
        </label>
        <Show when={props.project}>
          <fieldset disabled={props.project?.source === "file"}>
            <legend>Home board columns</legend>
            <p class="small muted">Comma-separated board roles or column names. Empty hides that column for this project.</p>
            <For each={Object.keys(defaultHomeColumns) as HomeColumn[]}>{key => <label>
              {({ in_progress: "In progress", review: "Review", accepted: "Accepted", blocked: "Blocked" })[key]}
              <input value={homeColumns()[key].join(", ")} onChange={e => setHomeColumns({ ...homeColumns(), [key]: e.currentTarget.value.split(",").map(s => s.trim()).filter(Boolean) })} />
            </label>}</For>
          </fieldset>
          <p class="muted small" style={{ margin: 0 }}>
            The prefix cannot change after the first card.
          </p>
        </Show>
        <div class="row">
          <button class="primary" type="submit">
            {props.project ? "Save" : "Create project"}
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

export default function ProjectsPage() {
  const { projects, agents } = useInstance();
  const navigate = useNavigate();
  const [creating, setCreating] = createSignal(false);
  return (
    <>
      <PageHead title="Projects">
        <button onClick={() => void projects.refresh()}>Refresh</button>
        <button class="primary" onClick={() => setCreating(true)}>
          New project
        </button>
      </PageHead>
      <Show when={projects.projects().length} fallback={<Empty>No projects yet.</Empty>}>
        <div class="card-box table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Prefix</th>
                <th>Description</th>
                <th>Agents</th>
                <th>Source</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              <For each={projects.projects()}>
                {(p) => {
                  // The colour of all its agents; the link label gives it in words.
                  const health = () => projectHealth(agents.inProject(p.name));
                  const label = () => projectHealthLabel(p.name, health());
                  return (
                    <tr>
                      <td>
                        <A href={`/p/${encodeURIComponent(p.name)}/settings/general`} class="project-name" aria-label={label()} title={label()}>
                          <ProjectDot level={health().level} />
                          <strong>{p.name}</strong>
                        </A>
                      </td>
                      <td class="mono">{p.card_prefix}</td>
                      <td>{p.description || <span class="muted">—</span>}</td>
                      <td>{agents.inProject(p.name).length}</td>
                      <td>{p.source ?? "api"}</td>
                      <td>{ago(p.created_at)}</td>
                    </tr>
                  );
                }}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={creating()}>
        <ProjectForm onClose={() => setCreating(false)} onSaved={(p) => (projects.select(p.name), navigate(`/p/${encodeURIComponent(p.name)}/settings/general`))} />
      </Show>
    </>
  );
}

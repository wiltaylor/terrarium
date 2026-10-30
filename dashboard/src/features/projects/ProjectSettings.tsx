import { A, useNavigate, useParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import { isMissingEndpoint } from "../../api/http";
import type { NewRepo, Repo, Target } from "../../api/types";
import { ago } from "../../lib/format";
import { agents, api, projects } from "../../stores";
import { ProjectForm } from "./ProjectsPage";
import { ScopeChip } from "../targets/ScopeChip";
import { filterByScope, groupByScope } from "../../lib/target-scope";

/** The General & repos tab of the project settings screen (/p/:project/settings/general). */
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
      <PageHead title="General & repos" level={2}>
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
      <ReposPanel project={params.project} />
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

/** The Repos panel (PLAN §17): list, attach, branch, check out onto a target. */
export function ReposPanel(props: { project: string }) {
  const [repos, { refetch }] = createResource(() => props.project, (p) => api.projects.repos(p));
  const [targets] = createResource(() => props.project, (p) => api.projects.targets(p).catch((): Target[] => []));
  const [error, setError] = createSignal<unknown>(null);
  const [form, setForm] = createSignal<NewRepo>({ url: "" });
  const set = (k: keyof NewRepo, v: string) => setForm({ ...form(), [k]: v });

  return (
    <section class="card-box stack" style={{ "margin-top": "16px" }}>
      <h2 style={{ margin: 0 }}>Repos</h2>
      <ErrorBanner error={error() ?? repos.error} onDismiss={() => setError(null)} />
      <Show when={repos()?.length} fallback={<Empty>{repos.loading ? "Loading…" : "No repos attached."}</Empty>}>
        <div class="stack" style={{ gap: "8px" }}>
          <For each={repos()}>{(r) => <RepoRow repo={r} targets={targets() ?? []} />}</For>
        </div>
      </Show>
      <form
        class="stack"
        style={{ gap: "8px" }}
        onSubmit={async (e) => {
          e.preventDefault();
          const f = form();
          const body: NewRepo = { url: f.url.trim() };
          for (const k of ["name", "default_branch", "credential", "path"] as const) if (f[k]?.trim()) body[k] = f[k]!.trim();
          try {
            await api.projects.attachRepo(props.project, body);
            setForm({ url: "" });
            setError(null);
            void refetch();
          } catch (err) {
            setError(err);
          }
        }}
      >
        <h3 style={{ margin: 0 }}>Attach a repo</h3>
        <div class="form-grid">
          <label>
            URL
            <input value={form().url} onInput={(e) => set("url", e.currentTarget.value)} required placeholder="https://github.com/me/infra.git" />
          </label>
          <label>
            Name (empty: from the URL)
            <input value={form().name ?? ""} onInput={(e) => set("name", e.currentTarget.value)} />
          </label>
          <label>
            Default branch
            <input value={form().default_branch ?? ""} onInput={(e) => set("default_branch", e.currentTarget.value)} placeholder="main" />
          </label>
          <label>
            Credential (a key name, never the secret)
            <input value={form().credential ?? ""} onInput={(e) => set("credential", e.currentTarget.value)} placeholder="github-deploy" />
          </label>
          <label>
            Checkout path
            <input value={form().path ?? ""} onInput={(e) => set("path", e.currentTarget.value)} placeholder="infra" />
          </label>
        </div>
        <div>
          <button class="primary" type="submit">
            Attach
          </button>
        </div>
      </form>
    </section>
  );
}

function RepoRow(props: { repo: Repo; targets: Target[] }) {
  const [branch, setBranch] = createSignal(props.repo.default_branch ?? "");
  const [target, setTarget] = createSignal("");
  const [path, setPath] = createSignal(props.repo.path ?? "");
  const [result, setResult] = createSignal<{ ok: boolean; text: string } | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  const checkout = async () => {
    setResult(null);
    setError(null);
    try {
      await api.repos.checkout(props.repo.id, { target: target() || undefined, branch: branch().trim() || undefined, path: path().trim() || undefined });
      setResult({ ok: true, text: "Checkout requested." });
    } catch (e) {
      if (isMissingEndpoint(e)) setResult({ ok: false, text: "Checkout is not available yet: the daemon does not have POST /api/repos/{id}/checkout." });
      else setError(e);
    }
  };

  return (
    <div class="comment stack" style={{ gap: "6px" }}>
      <div class="spread">
        <span>
          <strong>{props.repo.name}</strong> <span class="mono small muted">{props.repo.url}</span>
        </span>
        <span class="muted small">
          {props.repo.credential ? `credential ${props.repo.credential} · ` : ""}
          {props.repo.source ?? "api"}
        </span>
      </div>
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <label class="row small">
          Branch
          <input style={{ width: "12em" }} value={branch()} onInput={(e) => setBranch(e.currentTarget.value)} placeholder={props.repo.default_branch ?? "main"} aria-label="Branch" />
        </label>
        <label class="row small">
          Target
          <select value={target()} onChange={(e) => setTarget(e.currentTarget.value)} aria-label="Target">
            <option value="">the agent workspace</option>
            <For each={props.targets}>{(t) => <option value={t.id}>{t.name}</option>}</For>
          </select>
        </label>
        <label class="row small">
          Path
          <input style={{ width: "10em" }} value={path()} onInput={(e) => setPath(e.currentTarget.value)} aria-label="Path" />
        </label>
        <button class="small" onClick={checkout}>
          Check out
        </button>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={result()}>{(r) => <span class={`small ${r().ok ? "" : "muted"}`}>{r().text}</span>}</Show>
    </div>
  );
}

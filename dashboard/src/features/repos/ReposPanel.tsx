import { useInstance } from "../../instance";
// The Repos page of a project (/p/:project/repos): the repos, an Add repo
// form (./AddRepoForm, `POST /api/projects/{name}/repos`, project_admin), and for each repo
// a link to its browser (/p/:project/repos/:repo) and a checkout row.

import { A } from "../../instance/navigation";
import { createEffect, createResource, createSignal, For, on, Show } from "solid-js";
import { isMissingEndpoint } from "../../api/http";
import type { Repo, Target } from "../../api/types";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";

import { AddRepoForm } from "./AddRepoForm";

export const repoHref = (project: string, repo: string) => `/p/${encodeURIComponent(project)}/repos/${encodeURIComponent(repo)}`;

/** Project-local state is discarded only when the explicit project changes. */
export function ReposPanel(props: { project: string }) {
  return <Show when={props.project} keyed>{(project) => <ReposContent project={project} />}</Show>;
}

function ReposContent(props: { project: string }) {
  const { api, repos: reposStore } = useInstance();
  const project = () => props.project;
  const [repos, { refetch }] = createResource(() => project(), (p) => api.projects.repos(p));
  const [targets] = createResource(() => project(), (p) => api.projects.targets(p).catch((): Target[] => []));
  const [adding, setAdding] = createSignal(false);
  // An add here, or one from another view, changes the repos store: read the list again.
  createEffect(on(() => reposStore.inProject(project()).length, () => void refetch(), { defer: true }));

  return (
    <>
      <PageHead title="Repos" level={2}>
        <button class="primary" onClick={() => setAdding(!adding())} aria-expanded={adding()}>
          {adding() ? "Close" : "Add repo"}
        </button>
      </PageHead>
      <Show when={adding()}>
        <AddRepoForm
          project={project()}
          onAdded={() => setAdding(false)}
        />
      </Show>
      <ErrorBanner error={repos.error} />
      <Show when={repos()?.length} fallback={<Empty>{repos.loading ? "Loading…" : "No repos yet. Add one: the daemon clones it into a workspace at a checkout."}</Empty>}>
        <div class="stack" style={{ gap: "8px" }}>
          <For each={repos()}>{(r) => <RepoRow repo={r} targets={targets() ?? []} />}</For>
        </div>
      </Show>
    </>
  );
}

function RepoRow(props: { repo: Repo; targets: Target[] }) {
  const { api } = useInstance();
  const [open, setOpen] = createSignal(false);
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
      if (isMissingEndpoint(e)) setResult({ ok: false, text: "Checkout is not available here: the daemon does not have POST /api/repos/{id}/checkout." });
      else setError(e);
    }
  };

  return (
    <div class="comment stack" style={{ gap: "6px" }}>
      <div class="spread" style={{ "flex-wrap": "wrap" }}>
        <span style={{ "min-width": 0 }}>
          <A href={repoHref(props.repo.project, props.repo.name)}>
            <strong>{props.repo.name}</strong>
          </A>{" "}
          <span class="mono small muted" style={{ "overflow-wrap": "anywhere" }}>
            {props.repo.url}
          </span>
        </span>
        <span class="muted small">
          {props.repo.default_branch ? `${props.repo.default_branch} · ` : ""}
          {props.repo.credential ? `credential ${props.repo.credential} · ` : ""}
          {props.repo.source ?? "api"}
        </span>
      </div>
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <A class="small" href={repoHref(props.repo.project, props.repo.name)}>
          Browse files, changes, and commits
        </A>
        <button class="small" onClick={() => setOpen(!open())} aria-expanded={open()}>
          Check out…
        </button>
      </div>
      <Show when={open()}>
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
      </Show>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={result()}>{(r) => <span class={`small ${r().ok ? "" : "muted"}`}>{r().text}</span>}</Show>
    </div>
  );
}

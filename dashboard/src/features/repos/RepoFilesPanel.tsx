import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import type { Repo } from "../../api/types";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import { useInstance } from "../../instance";
import { FilesTab } from "./FilesTab";
import { treeText } from "./RepoPage";

/** Selection is local, never inferred from the host route or another project. */
export function RepoFilesPanel(props: { project: string }) {
  return <Show when={props.project} keyed>{(project) => <RepoSelector project={project} />}</Show>;
}

function RepoSelector(props: { project: string }) {
  const { api } = useInstance();
  const [repos] = createResource(() => props.project, (project) => api.projects.repos(project));
  const [name, setName] = createSignal("");
  const repo = createMemo(() => repos.error ? undefined : repos()?.find((r) => r.name === name()));
  return (
    <>
      <PageHead title="Repo files" level={2}>
        <select aria-label="Repository" value={name()} onChange={(e) => setName(e.currentTarget.value)}>
          <option value="">Choose a repo</option>
          <For each={repos.error ? [] : repos()}>{(r) => <option value={r.name}>{r.name}</option>}</For>
        </select>
      </PageHead>
      <ErrorBanner error={repos.error} />
      <Show when={repo()?.id} keyed fallback={<Empty>{repos.loading ? "Loading…" : name() ? "Not Found" : "Choose a repo."}</Empty>}>
        {() => <RepoFiles repo={repo()!} />}
      </Show>
    </>
  );
}

function RepoFiles(props: { repo: Repo }) {
  const { api } = useInstance();
  const [tree, setTree] = createSignal("main");
  const [path, setPath] = createSignal("");
  const [refresh, setRefresh] = createSignal(0);
  const [trees, { refetch }] = createResource(() => props.repo.id, (id) => api.repos.trees(id, refresh() > 0));
  const current = createMemo(() => trees.error ? undefined : trees()?.trees.find((t) => t.id === tree()));
  return (
    <>
      <div class="row">
        <select aria-label="Tree" value={tree()} onChange={(e) => { setTree(e.currentTarget.value); setPath(""); }}>
          <For each={trees.error ? [] : trees()?.trees}>{(t) => <option value={t.id}>{treeText(t)}</option>}</For>
        </select>
        <button disabled={trees.loading} onClick={() => { setRefresh((n) => n + 1); void refetch(); }}>Refresh</button>
      </div>
      <ErrorBanner error={trees.error ?? current()?.error} />
      <Show when={current() && !current()!.error} fallback={<Empty>{trees.loading ? "Loading…" : "Not Found"}</Empty>}>
        <FilesTab repo={props.repo} tree={tree()} head={current()!.head} reload={`${tree()}#${refresh()}`} path={path()} onPath={setPath} />
      </Show>
    </>
  );
}

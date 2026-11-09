// The browser of one repo (/p/:project/repos/:repo, docs/api.md §5.3): a
// tree selector (main, and each agent checkout), then the tabs Files,
// Changes, and Commits. The query keeps the tree, the tab, the path (a file
// or a folder), and the commit, so a link opens the same view.
//
// Agents control every name, file, and diff here. They go into text nodes
// only. The Files tab (./FilesTab) has the one HTML render: a Markdown file
// through the safe Markdown component (lib/sanitize).

import { A, useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createMemo, createResource, createSignal, For, Match, on, Show, Switch } from "solid-js";
import type { Repo, RepoCommit, RepoTree } from "../../api/types";
import { DiffView } from "../../components/DiffView";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { FilesTab } from "./FilesTab";

type Tab = "files" | "changes" | "commits";
const TABS: { id: Tab; label: string }[] = [
  { id: "files", label: "Files" },
  { id: "changes", label: "Changes" },
  { id: "commits", label: "Commits" },
];

// `file` is the query of an older build: it opens as `path`.
type Search = { tree?: string; tab?: string; path?: string; file?: string; commit?: string };

const short = (sha?: string | null) => (sha ? sha.slice(0, 8) : "—");

/** The text of a tree option: label, ahead/behind, and the work tree state. */
export function treeText(t: RepoTree): string {
  if (t.kind === "main") return `main${t.branch && t.branch !== "main" ? ` (${t.branch})` : ""}`;
  const parts = [t.label];
  if (t.ahead != null || t.behind != null) parts.push(`↑${t.ahead ?? 0} ↓${t.behind ?? 0}`);
  if (t.error) parts.push("unreadable");
  else if (t.dirty && t.dirty.staged + t.dirty.unstaged + t.dirty.untracked > 0) parts.push("changed");
  return parts.join(" · ");
}

export default function RepoPage() {
  const params = useParams<{ project: string; repo: string }>();
  const [search, setSearch] = useSearchParams<Search>();
  const [repo] = createResource(
    () => [params.project, params.repo] as const,
    async ([p, name]): Promise<Repo> => {
      const all = await api.projects.repos(p);
      const r = all.find((x) => x.name === name);
      if (!r) throw new Error(`No repo \`${name}\` in project \`${p}\`.`);
      return r;
    },
  );
  const [refresh, setRefresh] = createSignal(0);
  const [trees, { refetch: refetchTrees }] = createResource(
    () => repo()?.id,
    (id) => api.repos.trees(id, refresh() > 0),
  );
  const treeId = () => search.tree || "main";
  const tab = (): Tab => (TABS.some((t) => t.id === search.tab) ? (search.tab as Tab) : "files");
  const current = createMemo(() => trees()?.trees.find((t) => t.id === treeId()));
  const reloadKey = createMemo(() => `${treeId()}#${refresh()}`);

  const doRefresh = () => {
    setRefresh(refresh() + 1);
    void refetchTrees();
  };

  return (
    <>
      <PageHead
        level={2}
        title={
          <>
            <A href={`/p/${encodeURIComponent(params.project)}/repos`}>Repos</A> / {params.repo}
          </>
        }
      >
        <button onClick={doRefresh} disabled={trees.loading} title="Fetch main from the remote, and read the agent checkouts again">
          {trees.loading ? "Refreshing…" : "Refresh"}
        </button>
      </PageHead>
      <ErrorBanner error={repo.error ?? trees.error} />
      <Show when={repo()}>
        {(r) => (
          <p class="small muted" style={{ margin: "0 0 10px", "overflow-wrap": "anywhere" }}>
            <span class="mono">{r().url}</span>
            {trees()?.default_branch ? ` · default branch ${trees()!.default_branch}` : ""}
          </p>
        )}
      </Show>
      <Show when={trees()}>
        {(ts) => (
          <div class="row repo-tree-bar" style={{ "flex-wrap": "wrap", gap: "8px", "margin-bottom": "10px" }}>
            <label class="row small">
              Tree
              <select value={treeId()} onChange={(e) => setSearch({ tree: e.currentTarget.value, path: undefined, file: undefined, commit: undefined })} aria-label="Tree">
                <For each={ts().trees}>{(t) => <option value={t.id}>{treeText(t)}</option>}</For>
              </select>
            </label>
            <Show when={current()}>{(t) => <TreeFacts tree={t()} />}</Show>
          </div>
        )}
      </Show>
      <Show when={current()?.error}>
        <div class="error-banner" role="alert">
          {current()!.error}
        </div>
      </Show>
      <Show when={trees() && !current()}>
        <Empty>No tree `{treeId()}` for this repo. Choose one above.</Empty>
      </Show>
      <Show when={repo() && current() && !current()!.error}>
        <nav class="tabs" aria-label="Repo views">
          <For each={TABS}>
            {(t) => (
              <button class={tab() === t.id ? "active" : ""} aria-current={tab() === t.id ? "page" : undefined} onClick={() => setSearch({ tab: t.id, commit: undefined })}>
                {t.label}
              </button>
            )}
          </For>
        </nav>
        <Switch>
          <Match when={tab() === "files"}>
            <FilesTab repo={repo()!} tree={treeId()} head={current()!.head} reload={reloadKey()} path={search.path ?? search.file ?? ""} onPath={(p) => setSearch({ path: p || undefined, file: undefined })} />
          </Match>
          <Match when={tab() === "changes"}>
            <ChangesTab repo={repo()!} tree={current()!} reload={reloadKey()} onOpen={(p) => setSearch({ tab: "files", path: p, file: undefined })} />
          </Match>
          <Match when={tab() === "commits"}>
            <CommitsTab repo={repo()!} tree={current()!} reload={reloadKey()} commit={search.commit} onCommit={(c) => setSearch({ commit: c })} />
          </Match>
        </Switch>
      </Show>
    </>
  );
}

function TreeFacts(props: { tree: RepoTree }) {
  const t = () => props.tree;
  return (
    <span class="small muted">
      <Show
        when={t().kind === "agent"}
        fallback={
          <>
            head <span class="mono">{short(t().head)}</span>
            {t().mirror?.fetched_at ? ` · fetched ${ago(t().mirror!.fetched_at!)}` : ""}
            <Show when={t().mirror?.error}>
              {" · "}
              <span class="chip warn" title={t().mirror!.error}>
                the last fetch failed
              </span>
            </Show>
          </>
        }
      >
        <A href={`/agents/${encodeURIComponent(t().agent_id ?? "")}`}>{t().agent}</A> on <span class="mono">{t().branch ?? "detached HEAD"}</span>
        {" · "}
        {t().ahead ?? 0} ahead, {t().behind ?? 0} behind <span class="mono">{t().base_ref?.replace(/^refs\/(remotes\/)?/, "") ?? "no base"}</span>
        <Show when={t().dirty}>
          {(d) => (
            <>
              {" · "}
              <Show when={d().staged + d().unstaged + d().untracked > 0} fallback={<span class="chip ok">clean</span>}>
                <span class="chip warn">
                  {d().staged} staged, {d().unstaged} changed, {d().untracked} untracked{d().partial ? " (at least)" : ""}
                </span>
              </Show>
            </>
          )}
        </Show>
      </Show>
    </span>
  );
}

// --- Changes -------------------------------------------------------------

/** `onOpen` shows a file of the tree in the Files tab. */
function ChangesTab(props: { repo: Repo; tree: RepoTree; reload: string; onOpen: (path: string) => void }) {
  const [changes] = createResource(
    () => [props.repo.id, props.tree.id, props.reload] as const,
    ([id, tree]) => api.repos.diff(id, tree),
  );
  return (
    <div class="stack">
      <ErrorBanner error={changes.error} />
      <Show when={changes.loading}>
        <Empty>Loading…</Empty>
      </Show>
      <Show when={changes()}>
        {(c) => (
          <Show
            when={c().uncommitted}
            fallback={
              <Empty>
                {c().note ?? "main is the base"}: pick an agent tree to see its changes against <span class="mono">{c().base_ref?.replace(/^refs\/heads\//, "") ?? "main"}</span>.
              </Empty>
            }
          >
            {(u) => (
              <>
                <section class="stack repo-section" style={{ gap: "6px" }} aria-label="Committed changes">
                  <h2 style={{ margin: 0 }}>Committed</h2>
                  <p class="small muted" style={{ margin: 0 }}>
                    <span class="mono">{c().base_ref?.replace(/^refs\/(remotes\/)?/, "") ?? "base"}</span> at <span class="mono">{short(c().base)}</span> .. <span class="mono">{c().branch ?? "HEAD"}</span> at{" "}
                    <span class="mono">{short(c().head)}</span>
                    {c().note ? ` · ${c().note}` : ""}
                  </p>
                  <DiffView set={c().committed} empty="No commits on this tree that the base does not have." onOpen={props.onOpen} />
                </section>
                <section class="stack repo-section" style={{ gap: "6px" }} aria-label="Uncommitted changes">
                  <h2 style={{ margin: 0 }}>Uncommitted</h2>
                  <Show when={u().note}>
                    <p class="small muted">{u().note}</p>
                  </Show>
                  <h3 style={{ margin: 0 }}>Staged</h3>
                  <DiffView set={u().staged} empty="Nothing staged." onOpen={props.onOpen} />
                  <h3 style={{ margin: 0 }}>Not staged</h3>
                  <DiffView set={u().unstaged} empty="No changes in the work tree." onOpen={props.onOpen} />
                  <h3 style={{ margin: 0 }}>Untracked</h3>
                  <Show when={u().untracked.length} fallback={<Empty>No untracked files.</Empty>}>
                    <ul class="mono small untracked">
                      <For each={u().untracked}>
                        {(p) => (
                          <li>
                            <button class="link mono" onClick={() => props.onOpen(p)}>
                              {p}
                            </button>
                          </li>
                        )}
                      </For>
                    </ul>
                    <Show when={u().untracked_truncated}>
                      <p class="small muted">More untracked files: the list shows the first 1000.</p>
                    </Show>
                  </Show>
                </section>
              </>
            )}
          </Show>
        )}
      </Show>
    </div>
  );
}

// --- Commits -------------------------------------------------------------

function CommitsTab(props: { repo: Repo; tree: RepoTree; reload: string; commit?: string; onCommit: (sha?: string) => void }) {
  const [items, setItems] = createSignal<RepoCommit[]>([]);
  const [next, setNext] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [loading, setLoading] = createSignal(false);
  const load = async (cursor: string | null) => {
    setLoading(true);
    try {
      const p = await api.repos.commits(props.repo.id, props.tree.id, cursor);
      setItems(cursor ? [...items(), ...p.items] : p.items);
      setNext(p.next);
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
  };
  createEffect(
    on(
      () => [props.repo.id, props.tree.id, props.reload],
      () => {
        setItems([]);
        void load(null);
      },
    ),
  );
  return (
    <Show when={props.commit} fallback={<CommitList items={items()} next={next()} loading={loading()} error={error()} tree={props.tree} onMore={() => void load(next())} onOpen={props.onCommit} />}>
      <CommitDetail repo={props.repo} tree={props.tree.id} sha={props.commit!} onBack={() => props.onCommit(undefined)} />
    </Show>
  );
}

function CommitList(props: { items: RepoCommit[]; next: string | null; loading: boolean; error: unknown; tree: RepoTree; onMore: () => void; onOpen: (sha: string) => void }) {
  return (
    <div class="stack" style={{ gap: "8px" }}>
      <p class="small muted" style={{ margin: 0 }}>
        {props.tree.kind === "main" ? `The history of ${props.tree.branch ?? "the default branch"}, newest first.` : `The commits of ${props.tree.branch ?? "HEAD"} that ${props.tree.base_ref?.replace(/^refs\/(remotes\/)?/, "") ?? "the base"} does not have, newest first.`}
      </p>
      <ErrorBanner error={props.error} />
      <Show when={props.items.length} fallback={<Empty>{props.loading ? "Loading…" : "No commits."}</Empty>}>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Commit</th>
                <th>Subject</th>
                <th>Author</th>
                <th>Date</th>
              </tr>
            </thead>
            <tbody>
              <For each={props.items}>
                {(c) => (
                  <tr>
                    <td>
                      <button class="link mono" onClick={() => props.onOpen(c.sha)}>
                        {short(c.sha)}
                      </button>
                    </td>
                    <td>{c.subject}</td>
                    <td class="small">{c.author_name}</td>
                    <td class="small muted" title={c.date}>
                      {ago(c.date)}
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={props.next}>
        <div>
          <button onClick={() => props.onMore()} disabled={props.loading}>
            {props.loading ? "Loading…" : "Load more"}
          </button>
        </div>
      </Show>
    </div>
  );
}

function CommitDetail(props: { repo: Repo; tree: string; sha: string; onBack: () => void }) {
  const [c] = createResource(
    () => [props.repo.id, props.tree, props.sha] as const,
    ([id, tree, sha]) => api.repos.commit(id, tree, sha),
  );
  return (
    <div class="stack" style={{ gap: "8px" }}>
      <div>
        <button class="link" onClick={() => props.onBack()}>
          ← All commits
        </button>
      </div>
      <ErrorBanner error={c.error} />
      <Show when={c.loading}>
        <Empty>Loading…</Empty>
      </Show>
      <Show when={c()}>
        {(d) => (
          <>
            <div class="card-box stack" style={{ gap: "6px" }}>
              <pre class="commit-message">{d().message}</pre>
              <p class="small muted" style={{ margin: 0 }}>
                <span class="mono">{d().sha}</span> · {d().author_name} &lt;{d().author_email}&gt; · <span title={d().date}>{ago(d().date)}</span>
                {d().committer_name !== d().author_name ? ` · committed by ${d().committer_name}` : ""}
                {d().parents.length ? ` · parent ${d().parents.map(short).join(", ")}` : " · root commit"}
              </p>
            </div>
            <DiffView set={d().diff} empty="This commit changes no file." />
          </>
        )}
      </Show>
    </div>
  );
}

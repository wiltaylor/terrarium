import { A, useParams } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, Match, Show, Switch, type JSX } from "solid-js";
import { Markdown } from "../../components/Markdown";
import { Empty, ErrorBanner, PageHead, Tabs } from "../../components/ui";
import type { Artifact } from "../../api/types";
import { ago, humanize } from "../../lib/format";
import { renderWiki, splitFrontMatter } from "../../lib/markdown";
import { api, projects } from "../../stores";
import { buildTree, type TreeNode } from "./tree";

const TABS = [
  { id: "page", label: "Page" },
  { id: "edit", label: "Edit" },
  { id: "history", label: "History" },
] as const;

function Tree(props: { nodes: TreeNode[]; project: string; current: string }): JSX.Element {
  return (
    <ul class="tree">
      <For each={props.nodes}>
        {(n) => (
          <li>
            <Show when={n.page} fallback={<div class="dir">{n.name}/</div>}>
              {(p) => (
                <A href={`/p/${props.project}/wiki/${p().path}`} class={props.current === p().path ? "active" : ""}>
                  {p().title}
                  <Show when={p().status === "ready_for_review"}>
                    {" "}
                    <span class="chip warn">review</span>
                  </Show>
                </A>
              )}
            </Show>
            <Show when={n.children.length}>
              <Tree nodes={n.children} project={props.project} current={props.current} />
            </Show>
          </li>
        )}
      </For>
    </ul>
  );
}

export default function WikiPage() {
  const params = useParams<{ project: string; path?: string }>();
  const path = () => (params.path && params.path.length ? decodeURIComponent(params.path) : "index");
  const [tab, setTab] = createSignal<(typeof TABS)[number]["id"]>("page");
  const [error, setError] = createSignal<string | null>(null);
  const [q, setQ] = createSignal("");

  const [tree, { refetch: refetchTree }] = createResource(() => ({ p: params.project, q: q() }), (k) => api.wiki.tree(k.p, k.q || undefined));
  const [allPages] = createResource(() => params.project, (p) => api.wiki.tree(p));
  const [versions] = createResource(() => params.project, (p) => api.versions.list(p));
  const [artifacts] = createResource(() => params.project, (p) => api.artifacts.list(p));
  const summary = () => allPages()?.find((p) => p.path === path());
  const [page, { mutate: setPage }] = createResource(() => summary()?.id, (id) => api.wiki.get(id));
  const [revisions, { refetch: refetchRevs }] = createResource(() => (tab() === "history" ? summary()?.id : undefined), (id) => api.wiki.revisions(id));
  const [backlinks] = createResource(() => summary()?.id, (id) => api.wiki.backlinks(id));

  const artifactMap = createMemo(() => Object.fromEntries((artifacts() ?? []).map((a) => [a.id, a])) as Record<string, Artifact>);
  const html = createMemo(() => {
    const p = page();
    if (!p) return "";
    const prefixes = projects.prefixes();
    return renderWiki(p.body, {
      project: params.project,
      pages: new Set((allPages() ?? []).map((x) => x.path)),
      versions: new Set((versions() ?? []).map((v) => v.name)),
      cardPrefixes: new Set(Object.keys(prefixes)),
      prefixProject: prefixes,
      artifactUrl: (id) => artifactMap()[id]?.url,
    });
  });
  const meta = () => splitFrontMatter(page()?.body ?? "").meta;

  const [draft, setDraft] = createSignal("");
  const [saveSummary, setSaveSummary] = createSignal("");
  const [reviewComment, setReviewComment] = createSignal("");
  const [diff, setDiff] = createSignal<{ rev: number; text: string } | null>(null);

  const run = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <>
      <PageHead title={`Wiki · ${params.project}`}>
        <input type="search" placeholder="Search pages" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="Search pages" />
      </PageHead>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="split">
        <nav class="card-box" aria-label="Pages">
          <Show when={tree()?.length} fallback={<span class="muted">No pages.</span>}>
            <Tree nodes={buildTree(tree() ?? [])} project={params.project} current={path()} />
          </Show>
        </nav>

        <Show when={page()} fallback={<Empty>{allPages.loading || page.loading ? "Loading…" : `No page at ${path()}.`}</Empty>}>
          {(p) => (
            <article class="stack">
              <div class="spread">
                <div>
                  <h1 style={{ margin: 0 }}>{p().title}</h1>
                  <div class="muted small">
                    {p().path} · rev {p().current_rev} · {p().updated_by}, {ago(p().updated_at)} · <span class="chip">{humanize(p().status)}</span>
                  </div>
                </div>
              </div>

              <Show when={p().status === "ready_for_review"}>
                <div class="review-bar">
                  <strong>Ready for review</strong>
                  <span class="muted small">reviewer: {meta().reviewer ?? p().reviewer ?? "operator"}</span>
                  <input style={{ flex: 1, "min-width": "140px" }} placeholder="Comment" value={reviewComment()} onInput={(e) => setReviewComment(e.currentTarget.value)} aria-label="Review comment" />
                  <button class="primary" onClick={() => run(async () => setPage(await api.wiki.review(p().id, "approve", reviewComment())))}>
                    Approve
                  </button>
                  <button disabled={!reviewComment().trim()} onClick={() => run(async () => (setPage(await api.wiki.review(p().id, "comment", reviewComment())), setReviewComment("")))}>
                    Comment
                  </button>
                </div>
              </Show>

              <Tabs
                tabs={TABS}
                active={tab()}
                onChange={(t) => {
                  if (t === "edit") {
                    setDraft(p().body);
                    setSaveSummary("");
                  }
                  setTab(t);
                }}
              />
              <Switch>
                <Match when={tab() === "page"}>
                  <div class="card-box">
                    <Markdown html={html()} artifacts={artifactMap()} />
                  </div>
                  <Show when={backlinks()?.length}>
                    <div class="small">
                      Backlinks:{" "}
                      <For each={backlinks()}>
                        {(b) => (
                          <>
                            <A href={`/p/${params.project}/wiki/${b.path}`}>{b.title}</A>{" "}
                          </>
                        )}
                      </For>
                    </div>
                  </Show>
                </Match>
                <Match when={tab() === "edit"}>
                  <div class="stack">
                    <textarea class="mono" style={{ "min-height": "340px" }} value={draft()} onInput={(e) => setDraft(e.currentTarget.value)} aria-label="Page body" />
                    <div class="row">
                      <input style={{ flex: 1 }} placeholder="Summary of the change (required)" value={saveSummary()} onInput={(e) => setSaveSummary(e.currentTarget.value)} aria-label="Summary" />
                      <button
                        class="primary"
                        disabled={!saveSummary().trim()}
                        onClick={() =>
                          run(async () => {
                            setPage(await api.wiki.save(p().id, { body: draft(), base: p().current_rev, summary: saveSummary() }));
                            setTab("page");
                            void refetchTree();
                          })
                        }
                      >
                        Save revision {p().current_rev + 1}
                      </button>
                    </div>
                    <p class="muted small" style={{ margin: 0 }}>
                      The save carries base rev {p().current_rev}. The daemon refuses it if someone saved since.
                    </p>
                  </div>
                </Match>
                <Match when={tab() === "history"}>
                  <div class="table-wrap">
                    <table>
                      <thead>
                        <tr>
                          <th>Rev</th>
                          <th>Author</th>
                          <th>Summary</th>
                          <th>When</th>
                          <th />
                        </tr>
                      </thead>
                      <tbody>
                        <For each={revisions()}>
                          {(r) => (
                            <tr>
                              <td>{r.rev}</td>
                              <td>{r.author}</td>
                              <td>{r.summary}</td>
                              <td>{ago(r.created_at)}</td>
                              <td class="row">
                                <Show when={r.rev > 1}>
                                  <button class="small" onClick={() => run(async () => setDiff({ rev: r.rev, text: (await api.wiki.diff(p().id, r.rev - 1, r.rev)).diff }))}>
                                    Diff
                                  </button>
                                </Show>
                                <Show when={r.rev !== p().current_rev}>
                                  <button class="small" onClick={() => run(async () => (setPage(await api.wiki.restore(p().id, r.rev)), void refetchRevs()))}>
                                    Restore
                                  </button>
                                </Show>
                              </td>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
                  </div>
                  <Show when={diff()}>
                    {(d) => (
                      <div>
                        <h3>
                          Diff rev {d().rev - 1} → {d().rev}
                        </h3>
                        <pre>{d().text || "(no line changes)"}</pre>
                      </div>
                    )}
                  </Show>
                </Match>
              </Switch>
            </article>
          )}
        </Show>
      </div>
    </>
  );
}

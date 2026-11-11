import { useInstance } from "../../instance";
// The Wiki tab of the knowledge screen (app/nav.ts KNOWLEDGE_TABS): the page
// tree and the page of the URL (/p/:project/knowledge/wiki/*path).

import { A, useParams } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, Match, Show, Switch, type JSX } from "solid-js";
import { Markdown } from "../../components/Markdown";
import { Empty, ErrorBanner, PageHead, Tabs } from "../../components/ui";
import type { Artifact } from "../../api/types";
import { ago, humanize } from "../../lib/format";
import { renderWiki, splitFrontMatter } from "../../lib/markdown";
import { isMissingEndpoint } from "../../api/http";
import type { WikiPage as Page } from "../../api/types";


import { bookmarkAddError } from "../../stores/bookmarks";
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
                <A href={wikiHref(props.project, p().path)} class={props.current === p().path ? "active" : ""}>
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

export const wikiHref = (project: string, path: string) => `/p/${encodeURIComponent(project)}/knowledge/wiki/${path.split("/").map(encodeURIComponent).join("/")}`;

export default function WikiPage() {
  const { api, projects, bookmarks } = useInstance();
  const params = useParams<{ project: string; path?: string }>();
  const path = () => (params.path && params.path.length ? decodeURIComponent(params.path) : "index");
  const bookmarkUrl = () => wikiHref(params.project, path());
  const [tab, setTab] = createSignal<(typeof TABS)[number]["id"]>("page");
  const [error, setError] = createSignal<unknown>(null);
  const [q, setQ] = createSignal("");

  const [tree, { refetch: refetchTree }] = createResource(() => ({ p: params.project, q: q() }), (k) => api.wiki.tree(k.p, k.q ? { q: k.q } : {}));
  const [allPages, { refetch: refetchAll }] = createResource(() => params.project, (p) => api.wiki.tree(p));
  const [artifacts] = createResource(() => params.project, (p) => api.artifacts.list(p).catch((): Artifact[] => []));
  // The page key is `project/path` (docs/api.md §11.1).
  const [page, { mutate: setPage }] = createResource(
    () => (allPages() ? `${params.project}/${path()}` : undefined),
    (key) => api.wiki.get(key).catch((e: unknown) => (isMissingEndpoint(e) ? Promise.reject(e) : null)),
  );
  const [revisions, { refetch: refetchRevs }] = createResource(() => (tab() === "history" ? page()?.id : undefined), (id) => api.wiki.revisions(id));
  const [backlinks] = createResource(() => page()?.id, (id) => api.wiki.backlinks(id));

  const artifactMap = createMemo(() => Object.fromEntries((artifacts() ?? []).map((a) => [a.id, a])) as Record<string, Artifact>);
  // Signed URLs for the artifacts the page links or shows inline (`artifact:ART-n`).
  const [artifactUrls] = createResource(
    () => {
      const p = page();
      if (!p) return null;
      const fromLinks = (p.links ?? []).filter((l) => l.kind === "artifact").map((l) => l.target.split("/").pop()!);
      const fromBody = [...p.body.matchAll(/artifact:(ART-\d+)/g)].map((m) => m[1]!);
      const ids = [...new Set([...fromLinks, ...fromBody])].filter((id) => artifactMap()[id]);
      return ids.length ? ids : null;
    },
    async (ids) => Object.fromEntries(await Promise.all(ids.map(async (id) => [id, (await api.artifacts.url(artifactMap()[id]!.uid)).url] as const))),
  );
  const html = createMemo(() => {
    const p = page();
    if (!p) return "";
    const prefixes = projects.prefixes();
    return renderWiki(p.body, {
      project: params.project,
      pages: new Set((allPages() ?? []).map((x) => x.path)),
      cardPrefixes: new Set(Object.keys(prefixes)),
      prefixProject: prefixes,
      artifactUrl: (id) => artifactUrls()?.[id],
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
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Wiki" level={2}>
        <input type="search" placeholder="Search pages" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="Search pages" />
      </PageHead>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <ErrorBanner error={bookmarks.error()} />
      <div class="split">
        <nav class="card-box" aria-label="Pages">
          <Show when={tree()?.length} fallback={<span class="muted">No pages.</span>}>
            <Tree nodes={buildTree(tree() ?? [])} project={params.project} current={path()} />
          </Show>
        </nav>

        <Show
          when={page()}
          fallback={
            <Show when={allPages.loading || page.loading} fallback={<NewPage project={params.project} path={path()} onCreated={(p) => (setPage(p), void refetchAll(), void refetchTree())} />}>
              <Empty>Loading…</Empty>
            </Show>
          }
        >
          {(p) => (
            <article class="stack">
              <div class="spread">
                <div>
                  <h3 style={{ margin: 0 }}>{p().title}</h3>
                  <button aria-pressed={bookmarks.has(bookmarkUrl())} onClick={() => {
                    if (bookmarks.has(bookmarkUrl())) bookmarks.remove(bookmarkUrl());
                    else if (!bookmarks.add(p().title, bookmarkUrl())) setError(new Error(bookmarkAddError(p().title, bookmarkUrl(), window.location.origin)));
                  }}>
                    {bookmarks.has(bookmarkUrl()) ? "Unpin bookmark" : "Pin to bookmarks"}
                  </button>
                  <div class="muted small">
                    {p().path} · rev {p().rev} · {p().author_name}, {ago(p().updated_at)}
                    <Show when={p().status}>
                      {(st) => (
                        <>
                          {" "}
                          · <span class="chip">{humanize(st())}</span>
                        </>
                      )}
                    </Show>
                  </div>
                </div>
              </div>

              <Show when={p().status === "ready_for_review"}>
                <div class="review-bar">
                  <strong>Ready for review</strong>
                  <span class="muted small">reviewer: {meta().reviewer ?? p().reviewer ?? "operator"}</span>
                  <input style={{ flex: 1, "min-width": "140px" }} placeholder="Comment" value={reviewComment()} onInput={(e) => setReviewComment(e.currentTarget.value)} aria-label="Review comment" />
                  <button class="primary" onClick={() => run(async () => (setPage(await api.wiki.review(p().id, "approve", reviewComment() || undefined)), void refetchTree()))}>
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
                    <Markdown html={html()} artifacts={artifactMap()} project={params.project} />
                  </div>
                  <Show when={backlinks()?.length}>
                    <div class="small">
                      Backlinks:{" "}
                      <For each={backlinks()}>
                        {(b) => (
                          <>
                            <A href={`/p/${params.project}/knowledge/wiki/${b.path}`}>{b.title}</A>{" "}
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
                            // A stale base is `409 stale_base`; the banner shows the diff from `details`.
                            setPage(await api.wiki.save(p().id, { body: draft(), base: p().rev, summary: saveSummary() }));
                            setTab("page");
                            void refetchTree();
                          })
                        }
                      >
                        Save revision {p().rev + 1}
                      </button>
                    </div>
                    <p class="muted small" style={{ margin: 0 }}>
                      The save carries base rev {p().rev}. The daemon refuses it if someone saved since, and shows the changes since your base.
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
                              <td>{r.author_name ?? r.author}</td>
                              <td>{r.summary}</td>
                              <td>{ago(r.created_at)}</td>
                              <td class="row">
                                <Show when={r.rev > 1}>
                                  <button class="small" onClick={() => run(async () => setDiff({ rev: r.rev, text: (await api.wiki.diff(p().id, r.rev - 1, r.rev)).diff }))}>
                                    Diff
                                  </button>
                                </Show>
                                <Show when={r.rev !== p().rev}>
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

/** No page at this path yet: offer to create it. */
function NewPage(props: { project: string; path: string; onCreated: (p: Page) => void }) {
  const { api } = useInstance();
  const [body, setBody] = createSignal(`# ${props.path.split("/").pop()}\n`);
  const [summary, setSummary] = createSignal("Create page");
  const [error, setError] = createSignal<unknown>(null);
  return (
    <div class="card-box stack">
      <p style={{ margin: 0 }}>
        No page at <span class="mono">{props.path}</span>. Create it:
      </p>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <textarea class="mono" style={{ "min-height": "160px" }} value={body()} onInput={(e) => setBody(e.currentTarget.value)} aria-label="New page body" />
      <div class="row">
        <input style={{ flex: 1 }} value={summary()} onInput={(e) => setSummary(e.currentTarget.value)} aria-label="Summary" />
        <button
          class="primary"
          disabled={!summary().trim()}
          onClick={async () => {
            try {
              props.onCreated(await api.wiki.create(props.project, { path: props.path, body: body(), summary: summary() }));
            } catch (e) {
              setError(e);
            }
          }}
        >
          Create page
        </button>
      </div>
    </div>
  );
}

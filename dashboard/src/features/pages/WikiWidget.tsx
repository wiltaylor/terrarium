import { createResource, onCleanup, Show } from "solid-js";
import { useInstance } from "../../instance";
import { A } from "../../instance/navigation";
import { Markdown } from "../../components/Markdown";
import { ErrorBanner } from "../../components/ui";
import { renderWiki } from "../../lib/markdown";
import { ApiError } from "../../api/http";
import { wikiHref } from "../wiki/WikiPage";

export function WikiWidget(props: { project: string; path: string }) {
  const { api, events, projects } = useInstance();
  const [page, { refetch }] = createResource(() => [props.project, props.path] as const, async ([project, path]) => {
    // Never accept a global ID or another project's path as the source.
    try {
      const p = await api.wiki.get(`${project}/${path}`);
      return p.project === project && p.path === path && !p.deleted_at ? p : null;
    } catch (e) { if (e instanceof ApiError && e.status === 404) return null; throw e; }
  });
  const [tree] = createResource(() => props.project, p => api.wiki.tree(p));
  const [artifacts] = createResource(() => props.project, p => api.artifacts.list(p));
  const off = events.on(e => { if (e.project === props.project && e.kind.startsWith("wiki")) void refetch(); });
  onCleanup(off);
  return <><ErrorBanner error={page.error} /><Show when={page.latest} fallback={<p>{page.loading && page.latest === undefined ? "Loading wiki page…" : "page not found"}</p>}>{p => <>
    <h3>{p().title}</h3><A href={wikiHref(props.project, props.path)}>Open in wiki</A><span class="small muted"> Revision {p().rev}</span>
    <Markdown project={props.project} artifacts={Object.fromEntries((artifacts.latest ?? []).map(a => [a.id, a]))} html={renderWiki(p().body, { project: props.project, pages: new Set((tree.latest ?? []).map(p => p.path)), cardPrefixes: new Set(Object.keys(projects.prefixes())) })} />
  </>}</Show></>;
}

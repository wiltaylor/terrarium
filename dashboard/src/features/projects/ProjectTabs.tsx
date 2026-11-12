// The tab row of a project: one route link per PROJECT_TABS entry. The project
// shell shows it on every /p/:project page, and the agent page shows it with
// Agents active (TER-153), since /agents/:id sits outside /p.

import { A, useLocation, useNavigate } from "../../instance/navigation";
import { createEffect, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import { useInstance } from "../../instance";
import { ErrorBanner } from "../../components/ui";
import { PROJECT_TABS } from "../../app/nav";
import type { TabBadge } from "./tabCounts";

/**
 * `base` is the encoded project URL, `/p/<name>`. `active` says which tab path is selected.
 * `badge` gives the count of a tab (TER-154): the words go into the accessible name and the tooltip, the number is aria-hidden.
 */
export function ProjectTabs(props: { base: string; active: (path: string) => boolean; badge?: (path: string) => TabBadge | null }) {
  const location = useLocation();
  const { api, events } = useInstance();
  const navigate = useNavigate();
  const project = () => decodeURIComponent(props.base.split("/").pop()!);
  const [pages, { refetch }] = createResource(project, p => api.pages.list(p));
  const [adding, setAdding] = createSignal(false);
  const [slug, setSlug] = createSignal("");
  const [title, setTitle] = createSignal("");
  const [error, setError] = createSignal<unknown>();
  const [saving, setSaving] = createSignal(false);
  const off = events.on(e => { if (e.project === project() && e.kind === "page.updated") void refetch(); });
  onCleanup(off);
  async function add() {
    setSaving(true); setError(null);
    try {
      await api.pages.save(project(), slug(), { title: title(), position: Math.max(-1, ...(pages.latest?.items ?? []).map(p => p.position)) + 1, widgets: [] });
      await refetch(); setAdding(false); navigate(`${props.base}/pages/${slug()}`);
    } catch (e) { setError(e); } finally { setSaving(false); }
  }
  let tabs!: HTMLElement;

  // Creating a page can leave the row scrolled to Add page at the far end.
  // Reveal the selected tab without moving the document vertically.
  createEffect(
    on(
      [() => location.pathname, () => pages.latest],
      () => {
        const active = tabs.querySelector<HTMLElement>("a.active");
        if (!active) return;
        const bounds = tabs.getBoundingClientRect();
        const selected = active.getBoundingClientRect();
        if (selected.left < bounds.left) tabs.scrollLeft += selected.left - bounds.left;
        else if (selected.right > bounds.right) tabs.scrollLeft += selected.right - bounds.right;
      },
    ),
  );

  return (
    // Route links, not an ARIA tablist: every tab is a page.
    <nav ref={tabs} class="tabs project-tabs" aria-label="Project">
      <For each={pages.latest?.items}>{page => <A href={`${props.base}/pages/${page.slug}`} class={props.active(`pages/${page.slug}`) ? "active" : undefined} activeClass="" inactiveClass="">{page.title}</A>}</For>
      <For each={PROJECT_TABS}>
        {(tab) => (
          <A
            href={`${props.base}/${tab.path}`}
            class={props.active(tab.path) ? "active" : undefined}
            activeClass=""
            inactiveClass=""
            title={props.badge?.(tab.path)?.text}
          >
            {tab.label}
            <Show when={props.badge?.(tab.path)}>
              {(b) => (
                <>
                  <span class={b().cls} aria-hidden="true">
                    {b().n}
                  </span>
                  <span class="sr-only">, {b().text}</span>
                </>
              )}
            </Show>
          </A>
        )}
      </For>
      <Show when={pages.latest?.can_edit}>
        <button onClick={() => setAdding(!adding())} disabled={(pages.latest?.items.length ?? 0) >= 10}>Add page</button>
        <Show when={adding()}><form class="row" onSubmit={e => { e.preventDefault(); void add(); }}>
          <label>Page slug<input required pattern="[a-z0-9][a-z0-9_-]{0,63}" value={slug()} onInput={e => setSlug(e.currentTarget.value)} /></label>
          <label>Page title<input required maxlength={120} value={title()} onInput={e => setTitle(e.currentTarget.value)} /></label>
          <button type="submit" disabled={saving() || pages.latest?.items.some(p => p.slug === slug())}>Create page</button>
        </form></Show>
      </Show>
      <ErrorBanner error={error() ?? pages.error} />
    </nav>
  );
}

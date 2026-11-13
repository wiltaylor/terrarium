// The tab row of a project: one route link per PROJECT_TABS entry. The project
// shell shows it on every /p/:project page, and the agent page shows it with
// Agents active (TER-153), since /agents/:id sits outside /p.

import { A, useLocation, useNavigate } from "../../instance/navigation";
import { createEffect, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import { useInstance } from "../../instance";
import { ErrorBanner } from "../../components/ui";
import { PROJECT_TABS } from "../../app/nav";
import type { TabBadge } from "./tabCounts";
import { combineWidgetBadges } from "../pages/widgetBadges";
import { widgetBadgeSources } from "../pages/widgetRegistry";
import { projectPagesEnabled } from "../pages/pageCapability";
import { addPage, movePage, removePage, renamePage, useLayoutEditor } from "../pages/layoutDraft";
import type { PageBuiltin, PageWidget, ProjectPage } from "../../api/types";

/** The built-in pages in their default order. A built-in page is also active on its legacy route (`/p/<name>/<builtin>/...`). */
export const PAGE_BUILTINS: readonly PageBuiltin[] = ["board", "tests", "schedules", "rules", "agents", "repos", "knowledge", "pipelines", "calendar", "logs"];
const builtinLabel = (b: PageBuiltin) => b[0]!.toUpperCase() + b.slice(1);

/**
 * `base` is the encoded project URL, `/p/<name>`. `active` says which tab path is selected.
 * `badge` gives the count of a tab (TER-154): the words go into the accessible name and the tooltip, the number is aria-hidden.
 */
export function ProjectTabs(props: { base: string; active: (path: string) => boolean; badge?: (path: string) => TabBadge | null }) {
  const location = useLocation();
  const { api, events, key: instanceKey } = useInstance();
  const navigate = useNavigate();
  const project = () => decodeURIComponent(props.base.split("/").pop()!);
  const scope = () => `${instanceKey}/${project()}`;
  const [scopedPages, { refetch, mutate }] = createResource(scope, async current => ({ scope: current, value: await api.pages.list(project()) }));
  const pages = { get latest() { return !scopedPages.error && scopedPages.latest?.scope === scope() ? scopedPages.latest.value : undefined; }, get error() { return scopedPages.error; } };
  // Solid throws when `latest` of an errored resource is read. Check the error first (fail closed).
  const enabled = () => !scopedPages.error && projectPagesEnabled(scopedPages.latest, scope(), undefined);
  // OFF keeps the old row: the fixed tabs plus the custom pages. Built-in
  // pages that an ON period left behind (a rollback) stay stored but hidden.
  const shown = (): ProjectPage[] => (pages.latest?.items ?? []).filter(page => enabled() || !page.builtin);
  const pageActive = (page: { slug: string; builtin?: PageBuiltin }) =>
    props.active(`pages/${page.slug}`) || (enabled() && !!page.builtin && props.active(page.builtin));
  const [adding, setAdding] = createSignal(false);
  const [restoring, setRestoring] = createSignal<PageBuiltin | "">("");
  const [copyFrom, setCopyFrom] = createSignal("");
  const [slug, setSlug] = createSignal("");
  const [title, setTitle] = createSignal("");
  const [error, setError] = createSignal<unknown>();
  const [saving, setSaving] = createSignal(false);
  const off = events.on(e => { if (e.project === project() && e.kind === "page.updated") void refetch(); });
  onCleanup(off);
  // Project pages v2 (TER-1073): one Edit for all tabs, one Save and Cancel.
  const editor = useLayoutEditor();
  const shared = () => !!editor && enabled();
  createEffect(() => editor?.setAllowed(shared() && pages.latest?.can_edit === true));
  const editing = () => shared() && editor!.editing();
  createEffect(on(() => editor?.committed(), layout => {
    const value = scopedPages.error ? undefined : scopedPages.latest;
    if (layout && value?.scope === scope()) mutate({ scope: value.scope, value: { ...value.value, items: layout.pages } });
  }, { defer: true }));
  const [dragging, setDragging] = createSignal<string>();
  const [draftSlug, setDraftSlug] = createSignal("");
  const [draftTitle, setDraftTitle] = createSignal("");
  const href = (slug: string) => `${props.base}/pages/${slug}`;
  function reorder(slug: string, to: number) { editor!.update(p => movePage(p, slug, to)); }
  // A moved DOM node loses focus in browsers. Keyboard users stay on the control.
  function keep(element: HTMLElement, change: () => void) { change(); if (!(element as HTMLButtonElement).disabled) element.focus(); }
  function removeTab(slug: string) {
    if (!editor!.update(p => removePage(p, slug))) return;
    if (props.active(`pages/${slug}`)) navigate(editor!.draft()?.[0] ? href(editor!.draft()![0]!.slug) : `${props.base}/board`);
  }
  function addTab() {
    const slug = draftSlug();
    if (!editor!.update(p => addPage(p, slug, draftTitle()))) return;
    setDraftSlug(""); setDraftTitle(""); setAdding(false);
    navigate(href(slug));
  }
  async function saveLayout(replace = false) {
    if (await editor!.save(replace)) setAdding(false);
  }
  function cancelLayout() {
    if (editor!.dirty() && !window.confirm("Discard your unsaved page layout changes?")) return;
    editor!.cancel(); setAdding(false);
    // A page that exists only in the draft has no saved route.
    const slug = pages.latest?.items.find(page => props.active(`pages/${page.slug}`));
    if (!slug && location.pathname.includes("/pages/")) navigate(pages.latest?.items[0] ? href(pages.latest.items[0].slug) : `${props.base}/board`);
  }
  const draftBuiltin = (b: PageBuiltin) => editor?.draft()?.find(page => page.builtin === b);
  // Reset or add again one built-in page from the server default (one write, same base revision).
  async function restore() {
    const b = restoring();
    if (!b || !editor) return;
    const existing = draftBuiltin(b);
    if (existing && !window.confirm(`Reset the page "${existing.title || existing.slug}" to the default ${builtinLabel(b)} page?`)) return;
    if (await editor.apply(base => api.pages.resetLayout(project(), { base_revision: base, builtin: b }))) setRestoring("");
  }
  // Copy replaces every page of this project. The server needs the explicit confirmation too.
  async function copy() {
    const source = copyFrom().trim();
    if (!source || !editor) return;
    if (!window.confirm(`Replace all pages of ${project()} with the pages of ${source}? The current pages of ${project()} are lost.`)) return;
    if (await editor.apply(base => api.pages.copyLayout(project(), { base_revision: base, source_project: source, confirm: true }))) {
      setCopyFrom("");
      const first = editor.draft()?.[0];
      if (location.pathname.includes("/pages/") && !editor.draft()?.some(page => props.active(`pages/${page.slug}`))) navigate(first ? href(first.slug) : `${props.base}/board`);
    }
  }
  const pageBadge = (page: { slug: string; widgets: PageWidget[] }) => {
    // No question-bearing v2 part is defined yet. C7 must preserve this
    // alias fallback until explicit question metadata replaces it.
    const alias = page.slug === (enabled() ? pages.latest?.dashboard_slug : pages.latest?.dashboard_slug ?? "dashboard");
    if (!enabled()) return alias ? props.badge?.("dashboard") : null;
    return combineWidgetBadges([
      ...widgetBadgeSources(page.widgets), ...(alias ? ["dashboard" as const] : []),
    ], source => props.badge?.(source) ?? null);
  };
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
      <Show when={editing()} fallback={<For each={shown()}>{page => {
        const badge = () => pageBadge(page);
        return <A href={href(page.slug)} class={pageActive(page) ? "active" : undefined} activeClass="" inactiveClass="" title={badge()?.text}>
          {page.title}<Show when={badge()}>{b => <><span class={b().cls} aria-hidden="true">{b().n}</span><span class="sr-only">, {b().text}</span></>}</Show>
        </A>;
      }}</For>}>
        {/* Keyed by slug, so reorder and rename keep the DOM and focus. */}
        <For each={editor!.draft()?.map(page => page.slug)}>{slug => {
          const page = () => editor!.page(slug);
          const index = () => editor!.draft()?.findIndex(item => item.slug === slug) ?? -1;
          const count = () => editor!.draft()?.length ?? 0;
          return <span class="project-tab-edit" data-slug={slug} classList={{ active: pageActive({ slug, builtin: page()?.builtin }), dragging: dragging() === slug }} draggable={true}
            onDragStart={e => { setDragging(slug); e.dataTransfer?.setData("text/plain", slug); }}
            onDragOver={e => { if (dragging()) e.preventDefault(); }}
            onDrop={e => { e.preventDefault(); const from = dragging(); setDragging(); if (from && from !== slug) reorder(from, index()); }}
            onDragEnd={() => setDragging()}>
            <A href={href(slug)} class={pageActive({ slug, builtin: page()?.builtin }) ? "active" : undefined} activeClass="" inactiveClass=""
              aria-keyshortcuts="Alt+ArrowLeft Alt+ArrowRight"
              onKeyDown={e => { if (!e.altKey || (e.key !== "ArrowLeft" && e.key !== "ArrowRight")) return; e.preventDefault(); keep(e.currentTarget, () => reorder(slug, index() + (e.key === "ArrowLeft" ? -1 : 1))); }}>{page()?.title || slug}</A>
            <input aria-label={`Title of ${slug}`} required maxlength={120} value={page()?.title ?? ""} onInput={e => editor!.update(p => renamePage(p, slug, e.currentTarget.value))} />
            <button type="button" aria-label={`Move ${slug} left`} disabled={editor!.busy() || index() <= 0} onClick={e => keep(e.currentTarget, () => reorder(slug, index() - 1))}>←</button>
            <button type="button" aria-label={`Move ${slug} right`} disabled={editor!.busy() || index() >= count() - 1} onClick={e => keep(e.currentTarget, () => reorder(slug, index() + 1))}>→</button>
            <button type="button" aria-label={`Remove ${slug}`} disabled={editor!.busy()} onClick={() => removeTab(slug)}>×</button>
          </span>;
        }}</For>
      </Show>
      {/* ON: the stored pages replace the fixed tabs. Settings is behind the cog. */}
      <For each={enabled() ? [] : PROJECT_TABS}>
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
      {/* The legacy immediate-save Add is for OFF only. With v2 ON and no shell editor (the agent page), there is no Add. */}
      <Show when={!enabled() && pages.latest?.can_edit}>
        <button onClick={() => setAdding(!adding())} disabled={(pages.latest?.items.length ?? 0) >= 10}>Add page</button>
        <Show when={adding()}><form class="row" onSubmit={e => { e.preventDefault(); void add(); }}>
          <label>Page slug<input required pattern="[a-z0-9][a-z0-9_-]{0,63}" value={slug()} onInput={e => setSlug(e.currentTarget.value)} /></label>
          <label>Page title<input required maxlength={120} value={title()} onInput={e => setTitle(e.currentTarget.value)} /></label>
          <button type="submit" disabled={saving() || pages.latest?.items.some(p => p.slug === slug())}>Create page</button>
        </form></Show>
      </Show>
      <Show when={enabled()}>
        <A href={`${props.base}/settings`} class={props.active("settings") ? "tab-cog active" : "tab-cog"} activeClass="" inactiveClass="" aria-label="Project settings" title="Project settings">
          <span aria-hidden="true">⚙</span>
        </A>
      </Show>
      <Show when={shared() && pages.latest?.can_edit}>
        <Show when={editing()} fallback={<button disabled={editor!.busy()} onClick={() => void editor!.begin()}>Edit pages</button>}>
          <button type="button" onClick={() => setAdding(!adding())} disabled={editor!.busy() || (editor!.draft()?.length ?? 0) >= 30}>Add page</button>
          <Show when={adding()}><form class="row" onSubmit={e => { e.preventDefault(); addTab(); }}>
            <label>Page slug<input required pattern="[a-z0-9][a-z0-9_-]{0,63}" value={draftSlug()} onInput={e => setDraftSlug(e.currentTarget.value)} /></label>
            <label>Page title<input required maxlength={120} value={draftTitle()} onInput={e => setDraftTitle(e.currentTarget.value)} /></label>
            <button type="submit" disabled={editor!.draft()?.some(p => p.slug === draftSlug())}>Add to draft</button>
          </form></Show>
          <form class="row" onSubmit={e => { e.preventDefault(); void restore(); }}>
            <label>Built-in page<select value={restoring()} onChange={e => setRestoring(e.currentTarget.value as PageBuiltin | "")}>
              <option value="">Choose a built-in page</option>
              <For each={PAGE_BUILTINS}>{b => <option value={b}>{builtinLabel(b)}{draftBuiltin(b) ? " (reset to default)" : " (add again)"}</option>}</For>
            </select></label>
            <button type="submit" disabled={!restoring() || editor!.busy() || editor!.dirty()} title={editor!.dirty() ? "Save or cancel your changes first" : undefined}>Restore default</button>
          </form>
          <form class="row" onSubmit={e => { e.preventDefault(); void copy(); }}>
            <label>Copy layout from project<input value={copyFrom()} onInput={e => setCopyFrom(e.currentTarget.value)} /></label>
            <button type="submit" class="danger" disabled={!copyFrom().trim() || copyFrom().trim() === project() || editor!.busy() || editor!.dirty()} title={editor!.dirty() ? "Save or cancel your changes first" : undefined}>Copy layout</button>
          </form>
          <button type="button" disabled={editor!.busy()} onClick={cancelLayout}>Cancel</button>
          <button type="button" class="primary" disabled={editor!.busy() || editor!.draft()?.some(p => !p.title.trim())} onClick={() => void saveLayout()}>Save pages</button>
          <Show when={editor!.conflict() !== undefined}><div class="row" role="alert">
            <span>The layout changed elsewhere (revision {editor!.conflict()}). Your draft is kept.</span>
            <button type="button" disabled={editor!.busy()} onClick={() => { if (window.confirm("Discard your draft and load the latest layout?")) { editor!.cancel(); void editor!.begin(); } }}>Load latest</button>
            <button type="button" class="danger" disabled={editor!.busy()} onClick={() => { if (window.confirm("Replace the latest layout with your draft?")) void saveLayout(true); }}>Replace with my draft</button>
          </div></Show>
          <Show when={editor!.remote() !== undefined && editor!.conflict() === undefined}><p class="muted" role="status">Someone changed these pages. Your draft is kept. Save reports a conflict.</p></Show>
        </Show>
      </Show>
      {/* A failed refresh while drafting removes the rights (fail closed) but keeps the draft. */}
      <Show when={editor?.draft() && !editing()}><div class="row" role="alert">
        <span>Could not check your rights to edit these pages. Your unsaved draft is kept.</span>
        <button type="button" disabled={scopedPages.loading} onClick={() => void refetch()}>Retry</button>
        <button type="button" onClick={() => { if (window.confirm("Discard your unsaved page layout changes?")) editor!.cancel(); }}>Discard draft</button>
      </div></Show>
      <ErrorBanner error={error() ?? (editor?.conflict() === undefined ? editor?.error() : undefined) ?? pages.error} />
    </nav>
  );
}

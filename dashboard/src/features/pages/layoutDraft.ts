// The shared all-tabs layout draft of one project (TER-1073). One Edit on the
// tab row starts it, every tab and widget change goes into it, and one Save
// writes the whole layout with PUT /page-layout (docs/api.md, project page
// layout v2). The draft belongs to instance+project, never to one slug, so a
// tab change inside the project keeps it. Remote page.updated events and 409
// conflicts never replace it: only Cancel or a successful Save end it.

import { createContext, createEffect, createSignal, on, onCleanup, useContext } from "solid-js";
import { ApiError } from "../../api/http";
import type { Api } from "../../api/client";
import type { AgentEvent, PageWidget, ProjectLayout, ProjectLayoutPage } from "../../api/types";

/** The server bound of a whole layout. */
export const MAX_LAYOUT_PAGES = 30;

/** A copy with exactly the input fields and position equal to the array index. */
export function canonicalPages(pages: readonly ProjectLayoutPage[]): ProjectLayoutPage[] {
  return pages.map((page, position) => ({
    slug: page.slug, title: page.title, position, widgets: structuredClone(page.widgets),
    ...(page.builtin ? { builtin: page.builtin } : {}),
  }));
}

/** Draft pages from a snapshot, in tab order. The snapshot is not changed. */
export function draftFromLayout(layout: ProjectLayout): ProjectLayoutPage[] {
  return canonicalPages([...layout.pages].sort((a, b) => a.position - b.position || a.slug.localeCompare(b.slug)));
}

// Each operation returns new pages, or undefined when it refuses (nothing changes).
export function renamePage(pages: readonly ProjectLayoutPage[], slug: string, title: string) {
  if (!pages.some(page => page.slug === slug)) return undefined;
  return pages.map(page => page.slug === slug ? { ...page, title } : page);
}

export function movePage(pages: readonly ProjectLayoutPage[], slug: string, to: number) {
  const from = pages.findIndex(page => page.slug === slug);
  if (from < 0 || to < 0 || to >= pages.length) return undefined;
  const next = [...pages];
  next.splice(to, 0, ...next.splice(from, 1));
  return next.map((page, position) => page.position === position ? page : { ...page, position });
}

export function addPage(pages: readonly ProjectLayoutPage[], slug: string, title: string) {
  if (pages.length >= MAX_LAYOUT_PAGES || !/^[a-z0-9][a-z0-9_-]{0,63}$/.test(slug) || !title.trim() || pages.some(page => page.slug === slug)) return undefined;
  return [...pages, { slug, title, position: pages.length, widgets: [] }];
}

export function removePage(pages: readonly ProjectLayoutPage[], slug: string) {
  if (!pages.some(page => page.slug === slug)) return undefined;
  return pages.filter(page => page.slug !== slug).map((page, position) => page.position === position ? page : { ...page, position });
}

export function setPageWidgets(pages: readonly ProjectLayoutPage[], slug: string, widgets: PageWidget[]) {
  if (!pages.some(page => page.slug === slug)) return undefined;
  return pages.map(page => page.slug === slug ? { ...page, widgets } : page);
}

/** Moves one widget instance to the bottom of another page. ID and options stay. */
export function moveWidget(pages: readonly ProjectLayoutPage[], id: string, from: string, to: string) {
  const source = pages.find(page => page.slug === from);
  const target = pages.find(page => page.slug === to);
  const widget = source?.widgets.find(item => item.id === id);
  if (!source || !target || !widget || from === to || target.widgets.length >= 104 || pages.some(page => page.slug !== from && page.widgets.some(item => item.id === id))) return undefined;
  const y = Math.max(0, ...target.widgets.map(item => item.y + item.h));
  if (y + widget.h > 10000) return undefined;
  return pages.map(page => page.slug === from ? { ...page, widgets: page.widgets.filter(item => item.id !== id) }
    : page.slug === to ? { ...page, widgets: [...page.widgets, { ...widget, x: widget.frame && widget.frame !== "grid" ? 0 : widget.x, y }] } : page);
}

export interface LayoutEditor {
  /** `<instance>/<project>`. */
  scope(): string;
  /** The draft pages, while a draft of this scope exists. */
  draft(): ProjectLayoutPage[] | undefined;
  page(slug: string): ProjectLayoutPage | undefined;
  /** The draft exists and the tab row reports v2 and write rights. */
  editing(): boolean;
  setAllowed(allowed: boolean): void;
  /** The draft differs from the snapshot it started from. */
  dirty(): boolean;
  /** The revision the draft started from. */
  base(): number | undefined;
  /** A newer revision (or `true` without a number) was announced while drafting. */
  remote(): number | true | undefined;
  /** The current revision from a 409, while the conflict is open. */
  conflict(): number | undefined;
  busy(): boolean;
  error(): unknown;
  begin(): Promise<void>;
  cancel(): void;
  /** One PUT with the start revision, or with the conflict revision when `replace`. */
  save(replace?: boolean): Promise<boolean>;
  update(change: (pages: readonly ProjectLayoutPage[]) => ProjectLayoutPage[] | undefined): boolean;
  /** The canonical snapshot of the last successful save in this scope. */
  committed(): ProjectLayout | undefined;
}

export const LayoutEditorContext = createContext<LayoutEditor>();
export const useLayoutEditor = () => useContext(LayoutEditorContext);

interface Draft { scope: string; base: number; start: string; pages: ProjectLayoutPage[] }

export function createLayoutEditor(
  api: Pick<Api, "pages">,
  events: { on(fn: (event: AgentEvent) => void): () => void },
  instanceKey: string,
  project: () => string,
): LayoutEditor {
  const scope = () => `${instanceKey}/${project()}`;
  const [state, setState] = createSignal<Draft>();
  const [allowed, setAllowed] = createSignal(false);
  const [remote, setRemote] = createSignal<number | true>();
  const [conflict, setConflict] = createSignal<number>();
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>();
  const [committed, setCommitted] = createSignal<{ scope: string; layout: ProjectLayout }>();
  // Each begin or cancel invalidates older pending loads.
  let generation = 0;
  const current = () => { const d = state(); return d && d.scope === scope() ? d : undefined; };
  const clear = () => { generation++; setState(undefined); setRemote(undefined); setConflict(undefined); setError(undefined); setBusy(false); };
  createEffect(on(scope, clear, { defer: true }));
  const off = events.on(event => {
    const d = current();
    if (!d || event.project !== project() || event.kind !== "page.updated") return;
    const revision = (event.data as { revision?: unknown } | undefined)?.revision;
    if (typeof revision !== "number") setRemote(old => old ?? true);
    else if (revision > d.base) setRemote(old => typeof old === "number" && old >= revision ? old : revision);
  });
  onCleanup(off);

  const editor: LayoutEditor = {
    scope,
    draft: () => current()?.pages,
    page: slug => current()?.pages.find(page => page.slug === slug),
    editing: () => !!current() && allowed(),
    setAllowed,
    dirty: () => { const d = current(); return !!d && JSON.stringify(canonicalPages(d.pages)) !== d.start; },
    base: () => current()?.base,
    remote: () => current() ? remote() : undefined,
    conflict: () => current() ? conflict() : undefined,
    busy,
    error: () => current() || busy() ? error() : undefined,
    async begin() {
      if (current() || busy() || !allowed()) return;
      const want = scope(); const mine = ++generation;
      setBusy(true); setError(undefined);
      try {
        const layout = await api.pages.layout(project());
        if (mine !== generation || want !== scope()) return;
        const pages = draftFromLayout(layout);
        setRemote(undefined); setConflict(undefined);
        setState({ scope: want, base: layout.revision, start: JSON.stringify(pages), pages });
      } catch (e) { if (mine === generation && want === scope()) setError(e); }
      finally { if (mine === generation) setBusy(false); }
    },
    cancel: clear,
    async save(replace = false) {
      const d = current();
      if (!d || busy() || !allowed()) return false;
      const base = replace ? conflict() : d.base;
      if (base === undefined) return false;
      const want = d.scope; const mine = generation;
      setBusy(true); setError(undefined);
      try {
        const layout = await api.pages.saveLayout(project(), { base_revision: base, pages: canonicalPages(d.pages) });
        if (want !== scope()) return false;
        setCommitted({ scope: want, layout });
        // Edits wait while busy. A Cancel during the request already ended the draft.
        if (mine === generation) clear();
        return true;
      } catch (e) {
        if (mine !== generation || want !== scope()) return false;
        const revision = e instanceof ApiError && e.status === 409 ? (e.details as { revision?: unknown } | undefined)?.revision : undefined;
        if (typeof revision === "number") setConflict(revision);
        setError(e);
        return false;
      } finally { if (mine === generation) setBusy(false); }
    },
    update(change) {
      const d = current();
      if (!d || !allowed() || busy()) return false;
      const pages = change(d.pages);
      if (!pages) return false;
      setState({ ...d, pages });
      return true;
    },
    committed: () => { const c = committed(); return c && c.scope === scope() ? c.layout : undefined; },
  };
  return editor;
}

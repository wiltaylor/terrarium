import { useParams } from "@solidjs/router";
import { createEffect, createMemo, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import { useInstance } from "../../instance";
import { ApiError } from "../../api/http";
import { A, useNavigate } from "../../instance/navigation";
import type { DashboardRange, PageWidget, ProjectPageInput } from "../../api/types";
import { SnapGrid } from "../../components/SnapGrid";
import { Markdown } from "../../components/Markdown";
import { PostEditor } from "../../components/PostEditor";
import { ErrorBanner, PageHead, useNow } from "../../components/ui";
import { renderMarkdown } from "../../lib/markdown";
import { BlockView } from "../dashboard/blocks";
import { kindsFor, RANGES } from "../dashboard/model";
import { WikiWidget } from "./WikiWidget";
import { isSystemWidget, SystemWidget } from "./SystemWidget";
import { LEGACY_WIDGETS, WIDGET_REGISTRY, featureWidget, widgetDefinition, PROJECT_WIDGET_VIEWS } from "./widgetRegistry";
import type { ProjectWidgetKind, ProjectWidgetViews } from "../../api/types";
import { projectPagesEnabled } from "./pageCapability";
import { moveWidget, setPageWidgets, useLayoutEditor } from "./layoutDraft";
import { FeatureWidget } from "./FeatureWidget";
import { pageGrants } from "./pageGrants";

export default function CustomPage() {
  const { api, events, key: instanceKey } = useInstance();
  const params = useParams<{ project: string; slug: string }>();
  const navigate = useNavigate();
  const now = useNow(5000);
  const editor = useLayoutEditor();
  const key = () => `${params.project}/${params.slug}`;
  // A tab added in the shared draft has no saved page yet.
  const [page, { refetch, mutate }] = createResource(key, () => api.pages.get(params.project, params.slug).catch(e => {
    if (e instanceof ApiError && e.status === 404 && editor?.page(params.slug)) return undefined;
    throw e;
  }));
  const scope = () => `${instanceKey}/${params.project}`;
  const [scopedRights, { refetch: refetchRights }] = createResource(scope, async current => ({ scope: current, value: await api.pages.list(params.project) }));
  const rights = () => !scopedRights.error && scopedRights.latest?.scope === scope() ? scopedRights.latest?.value : undefined;
  // Solid throws when `latest` of an errored resource is read. Check the error first (fail closed).
  const enabled = () => !scopedRights.error && projectPagesEnabled(scopedRights.latest, scope(), undefined);
  const canEdit = () => !scopedRights.error && rights()?.can_edit === true;
  // The widget grants come from the domain rights, never from `can_edit` (TER-1077).
  const grants = createMemo(() => pageGrants(rights()?.rights, params.project));
  const [monitors, { refetch: refetchMonitors }] = createResource(() => params.project, p => api.monitors.list(p));
  const [draft, setDraft] = createSignal<ProjectPageInput | null>(null);
  const [error, setError] = createSignal<unknown>();
  const [saving, setSaving] = createSignal(false);
  const [editingWidget, setEditingWidget] = createSignal<string>();
  const [part, setPart] = createSignal("markdown");
  createEffect(on(key, () => { setDraft(null); setEditingWidget(undefined); setError(null); }));
  // Project pages v2 (TER-1073): the tab row owns one draft of all tabs. This
  // page edits only its widgets in that draft and has no Edit, Save or Delete.
  const shared = () => !!editor && enabled();
  const sharedPage = () => shared() && editor!.editing() ? editor!.page(params.slug) : undefined;
  const editing = () => shared() ? !!sharedPage() && canEdit() : !!draft() && canEdit();
  createEffect(on(() => editor?.committed(), layout => {
    const saved = layout?.pages.find(item => item.slug === params.slug);
    if (saved) mutate(saved);
  }, { defer: true }));
  // After a Retry on the tab row brings editing back, read this page's rights again too.
  createEffect(on(() => editor?.editing(), now => { if (now && scopedRights.error) void refetchRights(); }, { defer: true }));
  createEffect(on(() => editor?.draft(), (now, before) => { if (before && !now) { setEditingWidget(undefined); void refetch(); } }, { defer: true }));
  const widgets = () => shared() && editor!.editing() ? sharedPage()?.widgets ?? [] : draft()?.widgets ?? page.latest?.widgets ?? [];
  function changeWidgets(change: (widgets: PageWidget[]) => PageWidget[]) {
    if (!canEdit()) return;
    if (shared()) editor!.update(pages => { const current = pages.find(item => item.slug === params.slug); return current && setPageWidgets(pages, params.slug, change(current.widgets)); });
    else setDraft(d => d && { ...d, widgets: change(d.widgets) });
  }
  const off = events.on(e => {
    if (e.project !== params.project) return;
    if (e.kind === "page.updated") {
      void refetchRights();
      if (!draft() && !editor?.draft()) void refetch();
    }
    if (e.kind === "monitor.result") void refetchMonitors();
  });
  onCleanup(off);
  function patchWidget(id: string, patch: Partial<PageWidget>) {
    changeWidgets(widgets => widgets.map(w => w.id === id ? { ...w, ...patch } : w));
  }
  function addWidget() {
    if (!canEdit()) return;
    const definition = (enabled() ? WIDGET_REGISTRY : LEGACY_WIDGETS).find(item => item.part === part());
    if (!definition) return;
    const k = definition.kind;
    const id = [...crypto.getRandomValues(new Uint8Array(16))].map(byte => byte.toString(16).padStart(2, "0")).join("");
    const y = Math.max(0, ...widgets().map(w => w.y + w.h));
    const widget: PageWidget = definition.host === "feature" ? featureWidget(k as ProjectWidgetKind, definition.view!, id, y) : { id, kind: k, x: 0, y, w: 12, h: 3,
      ...(k === "markdown" ? { text: "" } : k === "wiki" ? { wiki: "index" } : isSystemWidget(k) ? {} : { monitor: monitors.latest?.find(m => kindsFor(m.kind).includes(k as "stat"))?.id ?? "" }) };
    changeWidgets(widgets => [...widgets, widget]);
    setEditingWidget(widget.id);
  }
  async function save() {
    const d = draft(); if (!d || !canEdit()) return;
    setSaving(true); setError(null);
    try { mutate(await api.pages.save(params.project, params.slug, d)); setDraft(null); setEditingWidget(undefined); await refetchRights(); }
    catch (e) { setError(e); } finally { setSaving(false); }
  }
  async function remove() {
    if (!canEdit()) return;
    if (!window.confirm("Delete this page?")) return;
    setSaving(true); setError(null);
    try { await api.pages.remove(params.project, params.slug); navigate(`/p/${encodeURIComponent(params.project)}/board`); }
    catch (e) { setError(e); } finally { setSaving(false); }
  }
  return <>
    <PageHead title={sharedPage()?.title ?? page.latest?.title ?? "Page"} level={2}>
      <Show when={!shared() && canEdit() && page.latest}>
        <Show when={draft()} fallback={<button onClick={() => { const p = page.latest!; setDraft(structuredClone({ title: p.title, position: p.position, widgets: p.widgets, ...(p.builtin ? { builtin: p.builtin } : {}) })); }}>Edit page</button>}>
          <button disabled={saving()} onClick={() => { setDraft(null); setEditingWidget(undefined); setError(null); void refetch(); }}>Cancel</button>
          <button class="primary" disabled={saving()} onClick={() => void save()}>Save page</button>
          <button class="danger" disabled={saving()} onClick={() => void remove()}>Delete page</button>
        </Show>
      </Show>
    </PageHead>
    <ErrorBanner error={error() ?? (sharedPage() ? undefined : page.error) ?? scopedRights.error ?? monitors.error} />
    {/* The tab row can still be editing when only this page's read failed. Its Retry does not reach this page. */}
    <Show when={scopedRights.error}><div class="row"><button type="button" disabled={scopedRights.loading} onClick={() => void refetchRights()}>Retry page rights</button></div></Show>
    <Show when={shared() && editor!.editing() && !sharedPage()}><p class="muted">This page is removed in the unsaved draft.</p></Show>
    <Show when={editing() && shared()}><div class="stack dash-editbar">
      <div class="row"><label>Widget kind<select value={part()} onChange={e => setPart(e.currentTarget.value)}><For each={WIDGET_REGISTRY}>{definition => <option value={definition.part}>{definition.part}</option>}</For></select></label>
      <button disabled={widgets().length >= 104} onClick={addWidget}>Add widget</button></div>
      <p>Drag Move or Resize to snap to cells. Arrow keys move, Shift+arrows resize. Save pages on the tab row keeps the changes of all tabs.</p>
    </div></Show>
    <Show when={!shared() && canEdit() && draft()}>{d => <div class="stack dash-editbar">
      <label>Page title<input required maxlength={120} value={d().title} onInput={e => setDraft(old => old && { ...old, title: e.currentTarget.value })} /></label>
      <label>Tab order (lower comes first)<input type="number" min="0" step="1" value={d().position} onInput={e => setDraft(old => old && { ...old, position: e.currentTarget.valueAsNumber })} /></label>
      <div class="row"><label>Widget kind<select value={part()} onChange={e => setPart(e.currentTarget.value)}><For each={enabled() ? WIDGET_REGISTRY : LEGACY_WIDGETS}>{definition => <option value={definition.part}>{definition.part}</option>}</For></select></label>
      <button disabled={widgets().length >= 104} onClick={addWidget}>Add widget</button></div>
      <p>Drag Move or Resize to snap to cells. Arrow keys move, Shift+arrows resize. Save to keep changes.</p>
    </div>}</Show>
    <SnapGrid widgets={widgets()} documentFlow={enabled()} editing={editing()} onChange={widgets => changeWidgets(() => widgets)} renderWidget={() => null} renderLiveWidget={current => {
      const w = new Proxy({} as PageWidget, { get: (_target, key) => Reflect.get(current(), key) });
      return <>
      <Show when={editing()}><div class="row">
        <button onClick={() => setEditingWidget(editingWidget() === w.id ? undefined : w.id)}>Edit widget</button>
        <button onClick={() => changeWidgets(widgets => widgets.filter(item => item.id !== w.id))}>Remove widget</button>
        <Show when={shared() && (editor!.draft()?.length ?? 0) > 1}><label>Move to page<select value="" onChange={e => {
          const to = e.currentTarget.value; e.currentTarget.value = "";
          if (to) editor!.update(pages => moveWidget(pages, w.id, params.slug, to));
        }}><option value="">Choose a page</option><For each={editor!.draft()?.filter(item => item.slug !== params.slug)}>{item => <option value={item.slug}>{item.title || item.slug}</option>}</For></select></label></Show>
      </div></Show>
      <Show when={editing() && editingWidget() === w.id}>
        <Show when={enabled()}><label>Frame<select value={w.frame ?? "grid"} onChange={e => {
          const frame = e.currentTarget.value as NonNullable<PageWidget["frame"]>;
          patchWidget(w.id, { frame, ...(frame === "grid" ? {} : { x: 0, w: 12 }) });
        }}><option value="grid">Grid</option><option value="full_width">Full width</option><option value="viewport">Window height</option></select></label></Show>
        <label>Widget title<input maxlength={120} value={w.title ?? ""} onInput={e => patchWidget(w.id, { title: e.currentTarget.value })} /></label>
        <Show when={enabled() && widgetDefinition(w)?.host === "feature"}>
          <label>View<select value={w.options?.view ?? widgetDefinition(w)?.view} onChange={e => patchWidget(w.id, { options: { ...w.options, view: e.currentTarget.value as ProjectWidgetViews[ProjectWidgetKind] } })}><For each={PROJECT_WIDGET_VIEWS[w.kind as ProjectWidgetKind]}>{view => <option value={view}>{view}</option>}</For></select></label>
          <label>Search<input aria-invalid={new TextEncoder().encode(w.options?.search ?? "").length > 256} value={w.options?.search ?? ""} onInput={e => patchWidget(w.id, { options: { ...w.options, search: e.currentTarget.value } })} /></label>
          <p class={new TextEncoder().encode(w.options?.search ?? "").length > 256 ? "error" : "muted"}>{new TextEncoder().encode(w.options?.search ?? "").length} / 256 bytes</p>
        </Show>
        <Show when={w.kind === "markdown"}>
          <PostEditor label="Markdown" project={params.project} text={w.text ?? ""}><label>Markdown<textarea value={w.text ?? ""} onInput={e => patchWidget(w.id, { text: e.currentTarget.value })} /></label></PostEditor>
          <p class={new TextEncoder().encode(w.text ?? "").length > 16384 ? "error" : "muted"}>{new TextEncoder().encode(w.text ?? "").length} / 16384 bytes</p>
        </Show>
        <Show when={w.kind === "wiki"}><label>Wiki path (this project)<input value={w.wiki ?? ""} onInput={e => patchWidget(w.id, { wiki: e.currentTarget.value })} /></label></Show>
        <Show when={widgetDefinition(w)?.host === "monitor"}><label>Monitor<select value={w.monitor ?? ""} onChange={e => patchWidget(w.id, { monitor: e.currentTarget.value })}><option value="">Choose a monitor</option><For each={monitors.latest?.filter(m => kindsFor(m.kind).includes(w.kind as "stat"))}>{m => <option value={m.id}>{m.name} ({m.kind})</option>}</For></select></label>
          <label>Range<select value={w.options?.range ?? "24h"} onChange={e => patchWidget(w.id, { options: { ...w.options, range: e.currentTarget.value as DashboardRange } })}><For each={RANGES}>{r => <option value={r.id}>Last {r.label}</option>}</For></select></label>
          <label>Decimals<select value={w.options?.decimals ?? ""} onChange={e => patchWidget(w.id, { options: { ...w.options, decimals: e.currentTarget.value === "" ? undefined : Number(e.currentTarget.value) } })}><option value="">Automatic</option><For each={[0, 1, 2, 3]}>{d => <option value={d}>{d}</option>}</For></select></label>
          <Show when={w.kind === "stat"}><label><input type="checkbox" checked={w.options?.sparkline ?? true} onChange={e => patchWidget(w.id, { options: { ...w.options, sparkline: e.currentTarget.checked } })} />Sparkline</label></Show>
        </Show>
        <Show when={!enabled() && (w.kind === "wiki" || w.kind === "markdown")}><label><input type="checkbox" checked={w.auto_height !== false} onChange={e => patchWidget(w.id, { auto_height: e.currentTarget.checked })} />Grow to content (uncheck to scroll within resized height)</label></Show>
      </Show>
      <Show when={w.title}><h3>{w.title}</h3></Show>
      <Show when={w.kind === "markdown"}><Markdown html={renderMarkdown(w.text ?? "", params.project)} project={params.project} /></Show>
      <Show when={w.kind === "wiki"}><WikiWidget project={params.project} path={w.wiki ?? ""} /></Show>
      <Show when={isSystemWidget(w.kind)}><SystemWidget kind={w.kind} project={params.project} monitors={monitors.latest ?? []} now={now()} /></Show>
      <Show when={!widgetDefinition(w)}><p>Not Found</p></Show>
      <Show when={widgetDefinition(w)?.host === "feature"}>
        {/* Until the first capability answer nothing shows, so an ON page never flashes Not Found. */}
        <Show when={enabled()} fallback={<Show when={scopedRights.error || scopedRights.latest}><p>Not Found</p></Show>}><FeatureWidget project={params.project} widget={current()} grants={grants()} /></Show>
      </Show>
      <Show when={widgetDefinition(w)?.host === "monitor"}>
        <Show when={!enabled() || monitors.latest?.some(m => m.id === w.monitor && m.project === params.project)} fallback={<p>Not Found</p>}>
          <BlockView block={{ id: w.id, title: w.title, monitor: w.monitor, kind: w.kind as "stat", size: "l", options: w.options ?? {} }} monitor={monitors.latest?.find(m => m.id === w.monitor && m.project === params.project)} project={params.project} now={now()} version={Date.parse(monitors.latest?.find(m => m.id === w.monitor && m.project === params.project)?.last?.at ?? "") || 0} />
        </Show>
      </Show>
    </>; }} />
    <Show when={(sharedPage() || page.latest) && !widgets().length && !editing()}><p>This page has no widgets.</p></Show>
    <A href={`/p/${encodeURIComponent(params.project)}/board`}>Board</A>
  </>;
}

import { useParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import { useInstance } from "../../instance";
import { A, useNavigate } from "../../instance/navigation";
import type { PageWidget, ProjectPageInput } from "../../api/types";
import { SnapGrid } from "../../components/SnapGrid";
import { Markdown } from "../../components/Markdown";
import { PostEditor } from "../../components/PostEditor";
import { ErrorBanner, PageHead, useNow } from "../../components/ui";
import { renderMarkdown } from "../../lib/markdown";
import { BlockView } from "../dashboard/blocks";
import { WikiWidget } from "./WikiWidget";

export default function CustomPage() {
  const { api, events } = useInstance();
  const params = useParams<{ project: string; slug: string }>();
  const navigate = useNavigate();
  const now = useNow(5000);
  const key = () => `${params.project}/${params.slug}`;
  const [page, { refetch, mutate }] = createResource(key, () => api.pages.get(params.project, params.slug));
  const [rights, { refetch: refetchRights }] = createResource(() => params.project, p => api.pages.list(p));
  const [monitors, { refetch: refetchMonitors }] = createResource(() => params.project, p => api.monitors.list(p));
  const [draft, setDraft] = createSignal<ProjectPageInput | null>(null);
  const [error, setError] = createSignal<unknown>();
  const [saving, setSaving] = createSignal(false);
  const [editingWidget, setEditingWidget] = createSignal<string>();
  const [kind, setKind] = createSignal<PageWidget["kind"]>("markdown");
  createEffect(on(key, () => { setDraft(null); setEditingWidget(undefined); setError(null); }));
  const widgets = () => draft()?.widgets ?? page.latest?.widgets ?? [];
  const off = events.on(e => {
    if (e.project !== params.project) return;
    if (e.kind === "page.updated") {
      void refetchRights();
      if (!draft()) void refetch();
    }
    if (e.kind === "monitor.result") void refetchMonitors();
  });
  onCleanup(off);
  function patchWidget(id: string, patch: Partial<PageWidget>) {
    setDraft(d => d && { ...d, widgets: d.widgets.map(w => w.id === id ? { ...w, ...patch } : w) });
  }
  function addWidget() {
    const k = kind();
    const widget: PageWidget = { id: crypto.randomUUID(), kind: k, x: 0, y: Math.max(0, ...widgets().map(w => w.y + w.h)), w: 12, h: 3,
      ...(k === "markdown" ? { text: "" } : k === "wiki" ? { wiki: "index" } : { monitor: monitors.latest?.[0]?.id ?? "" }) };
    setDraft(d => d && { ...d, widgets: [...d.widgets, widget] });
    setEditingWidget(widget.id);
  }
  async function save() {
    const d = draft(); if (!d) return;
    setSaving(true); setError(null);
    try { mutate(await api.pages.save(params.project, params.slug, d)); setDraft(null); setEditingWidget(undefined); await refetchRights(); }
    catch (e) { setError(e); } finally { setSaving(false); }
  }
  async function remove() {
    if (!window.confirm("Delete this page?")) return;
    setSaving(true); setError(null);
    try { await api.pages.remove(params.project, params.slug); navigate(`/p/${encodeURIComponent(params.project)}/board`); }
    catch (e) { setError(e); } finally { setSaving(false); }
  }
  return <>
    <PageHead title={page.latest?.title ?? "Page"} level={2}>
      <Show when={rights.latest?.can_edit && page.latest}>
        <Show when={draft()} fallback={<button onClick={() => { const p = page.latest!; setDraft(structuredClone({ title: p.title, position: p.position, widgets: p.widgets })); }}>Edit page</button>}>
          <button disabled={saving()} onClick={() => { setDraft(null); setEditingWidget(undefined); setError(null); void refetch(); }}>Cancel</button>
          <button class="primary" disabled={saving()} onClick={() => void save()}>Save page</button>
          <button class="danger" disabled={saving()} onClick={() => void remove()}>Delete page</button>
        </Show>
      </Show>
    </PageHead>
    <ErrorBanner error={error() ?? page.error ?? rights.error ?? monitors.error} />
    <Show when={draft()}>{d => <div class="stack dash-editbar">
      <label>Page title<input required maxlength={120} value={d().title} onInput={e => setDraft(old => old && { ...old, title: e.currentTarget.value })} /></label>
      <label>Tab order (lower comes first)<input type="number" min="0" step="1" value={d().position} onInput={e => setDraft(old => old && { ...old, position: e.currentTarget.valueAsNumber })} /></label>
      <div class="row"><label>Widget kind<select value={kind()} onChange={e => setKind(e.currentTarget.value as PageWidget["kind"])}><For each={["markdown", "wiki", "stat", "status", "chart", "text", "table"]}>{k => <option value={k}>{k}</option>}</For></select></label>
      <button disabled={widgets().length >= 100} onClick={addWidget}>Add widget</button></div>
      <p>Drag Move or Resize to snap to cells. Arrow keys move, Shift+arrows resize. Save to keep changes.</p>
    </div>}</Show>
    <SnapGrid widgets={widgets()} editing={!!draft() && !!rights.latest?.can_edit} onChange={widgets => setDraft(d => d && { ...d, widgets })} renderWidget={() => null} renderLiveWidget={current => {
      const w = new Proxy({} as PageWidget, { get: (_target, key) => Reflect.get(current(), key) });
      return <>
      <Show when={draft() && rights.latest?.can_edit}><div class="row">
        <button onClick={() => setEditingWidget(editingWidget() === w.id ? undefined : w.id)}>Edit widget</button>
        <button onClick={() => setDraft(d => d && { ...d, widgets: d.widgets.filter(item => item.id !== w.id) })}>Remove widget</button>
      </div></Show>
      <Show when={draft() && editingWidget() === w.id && rights.latest?.can_edit}>
        <label>Widget title<input maxlength={120} value={w.title ?? ""} onInput={e => patchWidget(w.id, { title: e.currentTarget.value })} /></label>
        <Show when={w.kind === "markdown"}>
          <PostEditor label="Markdown" project={params.project} text={w.text ?? ""}><label>Markdown<textarea value={w.text ?? ""} onInput={e => patchWidget(w.id, { text: e.currentTarget.value })} /></label></PostEditor>
          <p class={new TextEncoder().encode(w.text ?? "").length > 16384 ? "error" : "muted"}>{new TextEncoder().encode(w.text ?? "").length} / 16384 bytes</p>
        </Show>
        <Show when={w.kind === "wiki"}><label>Wiki path (this project)<input value={w.wiki ?? ""} onInput={e => patchWidget(w.id, { wiki: e.currentTarget.value })} /></label></Show>
        <Show when={w.kind !== "wiki" && w.kind !== "markdown"}><label>Monitor<select value={w.monitor ?? ""} onChange={e => patchWidget(w.id, { monitor: e.currentTarget.value })}><option value="">Choose a monitor</option><For each={monitors.latest}>{m => <option value={m.id}>{m.name} ({m.kind})</option>}</For></select></label></Show>
        <Show when={w.kind === "wiki" || w.kind === "markdown"}><label><input type="checkbox" checked={w.auto_height !== false} onChange={e => patchWidget(w.id, { auto_height: e.currentTarget.checked })} />Grow to content (uncheck to scroll within resized height)</label></Show>
      </Show>
      <Show when={w.title}><h3>{w.title}</h3></Show>
      <Show when={w.kind === "markdown"}><Markdown html={renderMarkdown(w.text ?? "", params.project)} project={params.project} /></Show>
      <Show when={w.kind === "wiki"}><WikiWidget project={params.project} path={w.wiki ?? ""} /></Show>
      <Show when={w.kind !== "wiki" && w.kind !== "markdown"}><BlockView block={{ ...w, kind: w.kind as "stat", size: "l", options: w.options ?? {} }} monitor={monitors.latest?.find(m => m.id === w.monitor)} project={params.project} now={now()} version={Date.parse(monitors.latest?.find(m => m.id === w.monitor)?.last?.at ?? "") || 0} /></Show>
    </>; }} />
    <Show when={page.latest && !widgets().length && !draft()}><p>This page has no widgets.</p></Show>
    <A href={`/p/${encodeURIComponent(params.project)}/board`}>Board</A>
  </>;
}

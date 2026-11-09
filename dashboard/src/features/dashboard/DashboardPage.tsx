// The project dashboard (/p/:project/dashboard, docs/api.md §30): a grid of
// blocks that show the monitors of the project, and the Monitors tab
// (`?tab=monitors`) that manages them. "Edit dashboard" edits a draft of the
// layout: add a block, change it, move it (drag, or the up and down
// buttons), remove it, then save. The page loads again on each
// `monitor.result` and `dashboard.updated` event of the project. The
// project actions (TER-53) are buttons above the blocks; they load again on
// each `schedule` event of the project.

import { useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createMemo, createResource, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { DashboardBlock, DashboardView, Monitor } from "../../api/types";
import { Empty, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { api, events } from "../../stores";
import { ActionButtons } from "../actions/ActionButtons";
import { projectActions } from "../actions/model";
import { BlockDialog } from "./BlockDialog";
import { BlockView } from "./blocks";
import { layoutProblems, moveBlock, SIZES, STATE_META, stateCounts, type BlockState } from "./model";
import { MonitorsPanel } from "./MonitorsPanel";
import { EtaAccuracy } from "./EtaAccuracy";

const TABS = [
  { id: "blocks", label: "Dashboard" },
  { id: "monitors", label: "Monitors" },
] as const;

export default function DashboardPage() {
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<{ tab?: string }>();
  const tab = () => (search.tab === "monitors" ? "monitors" : "blocks");
  const now = useNow(5_000);
  const [error, setError] = createSignal<unknown>(null);
  // One load per project mount, independent of monitor and layout updates.
  const [accuracy] = createResource(() => params.project, (p) => api.monitors.etaAccuracy(p));
  const [view, { refetch, mutate }] = createResource(
    () => params.project,
    (p) => api.monitors.dashboard(p),
  );
  // The series of a block load again when the last result of its monitor changes.
  const versions = createMemo(() => Object.fromEntries((view.latest?.monitors ?? []).map((m) => [m.id, Date.parse(m.last?.at ?? "") || 0])));
  const monitorOf = (id?: string) => view.latest?.monitors.find((m) => m.id === id);
  // The project actions. A failed list hides the buttons; the blocks stay.
  const [actions, { refetch: refetchActions }] = createResource(
    () => params.project,
    async (p) => {
      try {
        return await api.schedules.list({ project: p, kind: "action" });
      } catch {
        return [];
      }
    },
  );

  // ----- live updates -----
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    let ta: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.project !== params.project) return;
      if (e.kind === "schedule") {
        clearTimeout(ta);
        ta = setTimeout(() => void refetchActions(), 400);
        return;
      }
      if (e.kind !== "monitor.result" && e.kind !== "dashboard.updated") return;
      // A save of this page's draft must not drop the draft of another editor: only reload when not editing.
      if (e.kind === "dashboard.updated" && draft()) return;
      clearTimeout(t);
      t = setTimeout(() => void refetch(), 400);
    });
    onCleanup(() => (off(), clearTimeout(t), clearTimeout(ta)));
  });

  // ----- edit mode -----
  const [draft, setDraft] = createSignal<DashboardBlock[] | null>(null);
  const [dialog, setDialog] = createSignal<{ index: number | null } | null>(null);
  const [saving, setSaving] = createSignal(false);
  const [dragging, setDragging] = createSignal<number | null>(null);
  createEffect(on(() => params.project, () => setDraft(null)));
  const blocks = () => draft() ?? view.latest?.blocks ?? [];
  const problems = createMemo(() => (draft() ? layoutProblems(draft()!, view.latest?.monitors ?? []) : []));
  const save = async () => {
    const d = draft();
    if (!d) return;
    setSaving(true);
    setError(null);
    try {
      const v = await api.monitors.saveDashboard(params.project, { blocks: d });
      mutate(v as DashboardView);
      setDraft(null);
    } catch (e) {
      setError(e);
    } finally {
      setSaving(false);
    }
  };
  const move = (from: number, to: number) => setDraft((d) => (d ? moveBlock(d, from, to) : d));

  const counts = createMemo(() => stateCounts(view.latest?.monitors ?? [], now()));
  const summary = () => {
    const c = counts();
    const order: BlockState[] = ["crit", "warn", "unknown", "stale", "ok", "pending", "off"];
    return order.filter((s) => c[s] > 0).map((s) => ({ s, n: c[s] }));
  };

  return (
    <>
      <PageHead title="Dashboard">
        <Show when={tab() === "blocks"}>
          <Show
            when={draft()}
            fallback={
              <button onClick={() => setDraft(structuredClone(view.latest?.blocks ?? []))} disabled={!view.latest}>
                Edit dashboard
              </button>
            }
          >
            <button onClick={() => setDialog({ index: null })}>Add block</button>
            <button class="ghost" onClick={() => (setDraft(null), setError(null))}>
              Cancel
            </button>
            <button class="primary" disabled={saving() || problems().length > 0} onClick={() => void save()}>
              {saving() ? "Saving…" : "Save layout"}
            </button>
          </Show>
        </Show>
      </PageHead>
      <Tabs tabs={TABS} active={tab()} onChange={(t) => setSearch({ tab: t === "monitors" ? "monitors" : undefined })} />
      <ErrorBanner error={error() ?? view.error} onDismiss={() => setError(null)} />
      <Show when={tab() === "blocks" && projectActions(actions.latest ?? [], params.project).length > 0}>
        <div class="dash-actions row small" aria-label="Actions">
          <span class="muted">Actions</span>
          <ActionButtons actions={projectActions(actions.latest ?? [], params.project)} />
        </div>
      </Show>
      <Show when={view.latest}>
        <p class="dash-summary small" aria-live="polite">
          <Show when={summary().length} fallback={<span class="muted">No monitors yet.</span>}>
            <For each={summary()}>
              {(x) => (
                <span class={`mon-count ${STATE_META[x.s].cls}`}>
                  <span aria-hidden="true">{STATE_META[x.s].glyph}</span> {x.n} {STATE_META[x.s].label.toLowerCase()}
                </span>
              )}
            </For>
          </Show>
        </p>
      </Show>
      <Show when={tab() === "monitors"}>
        <MonitorsPanel project={params.project} monitors={view.latest?.monitors ?? []} now={now()} onChanged={() => void refetch()} />
      </Show>
      <Show when={tab() === "blocks"}>
        <ErrorBanner error={accuracy.error} />
        <Show when={accuracy()}>
          {(accuracy) => <EtaAccuracy value={accuracy()} />}
        </Show>
        <Show when={draft()}>
          <div class="dash-editbar small" role="status">
            Edit mode: drag a block, or use its arrows, to move it. Save to keep the layout.
            <For each={problems()}>{(p) => <div class="mon-err">{p}</div>}</For>
          </div>
        </Show>
        <Show
          when={blocks().length || draft()}
          fallback={
            <Show when={view.latest}>
              <Empty>
                <Show
                  when={view.latest!.monitors.length}
                  fallback={
                    <>
                      No monitors yet. Open the <button class="link-button" onClick={() => setSearch({ tab: "monitors" })}>Monitors</button> tab and make one, or run <code>terrarium monitor new</code>.
                    </>
                  }
                >
                  The dashboard has no blocks. Click <strong>Edit dashboard</strong> and add one for each monitor.
                </Show>
              </Empty>
            </Show>
          }
        >
          <div class={`dash-grid${draft() ? " editing" : ""}`}>
            <For each={blocks()}>
              {(b, i) => (
                <div
                  class={`dash-block size-${b.size}${dragging() === i() ? " dragging" : ""}`}
                  draggable={!!draft()}
                  onDragStart={(e) => {
                    setDragging(i());
                    e.dataTransfer?.setData("text/plain", String(i()));
                  }}
                  onDragEnd={() => setDragging(null)}
                  onDragOver={(e) => draft() && e.preventDefault()}
                  onDrop={(e) => {
                    e.preventDefault();
                    const from = dragging();
                    if (from !== null && from !== i()) move(from, i());
                    setDragging(null);
                  }}
                >
                  <Show when={draft()}>
                    <div class="dash-tools" aria-label={`Block ${i() + 1}`}>
                      <span class="small muted">
                        {i() + 1}. {b.kind}
                        {b.kind !== "note" ? ` · ${monitorOf(b.monitor)?.name ?? "missing monitor"}` : ""} · {SIZES.find((s) => s.id === b.size)?.id.toUpperCase()}
                      </span>
                      <span class="row">
                        <button class="small ghost" aria-label="Move up" disabled={i() === 0} onClick={() => move(i(), i() - 1)}>
                          ↑
                        </button>
                        <button class="small ghost" aria-label="Move down" disabled={i() === blocks().length - 1} onClick={() => move(i(), i() + 1)}>
                          ↓
                        </button>
                        <button class="small" onClick={() => setDialog({ index: i() })}>
                          Edit
                        </button>
                        <button class="small danger" onClick={() => setDraft((d) => (d ? d.filter((_, k) => k !== i()) : d))}>
                          Remove
                        </button>
                      </span>
                    </div>
                  </Show>
                  <BlockView block={b} monitor={monitorOf(b.monitor) as Monitor | undefined} project={params.project} now={now()} version={versions()[b.monitor ?? ""] ?? 0} />
                </div>
              )}
            </For>
          </div>
        </Show>
      </Show>
      <Show when={dialog()}>
        {(d) => (
          <BlockDialog
            block={d().index !== null ? blocks()[d().index!] : undefined}
            blocks={blocks()}
            monitors={view.latest?.monitors ?? []}
            onClose={() => setDialog(null)}
            onSave={(nb) => {
              const idx = d().index;
              setDraft((cur) => {
                const list = [...(cur ?? [])];
                if (idx === null) list.push(nb);
                else list[idx] = nb;
                return list;
              });
              setDialog(null);
            }}
          />
        )}
      </Show>
    </>
  );
}

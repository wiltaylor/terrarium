import { createResource, For, onCleanup, Show } from "solid-js";
import type { Monitor, PageWidget } from "../../api/types";
import { useInstance } from "../../instance";
import { ErrorBanner } from "../../components/ui";
import { ActionButtons } from "../actions/ActionButtons";
import { projectActions } from "../actions/model";
import { JobQueuePanel } from "../dashboard/JobQueuePanel";
import { EtaAccuracy } from "../dashboard/EtaAccuracy";
import { STATE_META, stateCounts, type BlockState } from "../dashboard/model";

export const SYSTEM_KINDS = ["actions", "monitor_summary", "job_queue", "eta_accuracy"] as const;
export const isSystemWidget = (kind: PageWidget["kind"]) => (SYSTEM_KINDS as readonly string[]).includes(kind);

function Actions(props: { project: string }) {
  const { api, events } = useInstance();
  const [actions, { refetch }] = createResource(() => props.project, p => api.schedules.list({ project: p, kind: "action" }).catch(() => []));
  onCleanup(events.on(e => { if (e.project === props.project && e.kind === "schedule") void refetch(); }));
  return <div class="dash-actions row small" aria-label="Actions"><ActionButtons actions={projectActions(actions.latest ?? [], props.project)} /></div>;
}
function Accuracy(props: { project: string }) {
  const { api } = useInstance();
  const [accuracy] = createResource(() => props.project, p => api.monitors.etaAccuracy(p));
  return <><ErrorBanner error={accuracy.error} /><Show when={accuracy.latest}>{value => <EtaAccuracy value={value()} />}</Show></>;
}
export function SystemWidget(props: { kind: PageWidget["kind"]; project: string; monitors: Monitor[]; now: number }) {
  const order: BlockState[] = ["crit", "warn", "unknown", "stale", "ok", "pending", "off"];
  const counts = () => stateCounts(props.monitors, props.now);
  return <>
    <Show when={props.kind === "actions"}><Actions project={props.project} /></Show>
    <Show when={props.kind === "job_queue"}><JobQueuePanel project={props.project} /></Show>
    <Show when={props.kind === "eta_accuracy"}><Accuracy project={props.project} /></Show>
    <Show when={props.kind === "monitor_summary"}><p class="dash-summary small" aria-live="polite">
      <Show when={props.monitors.length} fallback={<span class="muted">No monitors yet.</span>}>
        <For each={order.filter(s => counts()[s] > 0)}>{s => <span class={`mon-count ${STATE_META[s].cls}`}><span aria-hidden="true">{STATE_META[s].glyph}</span> {counts()[s]} {STATE_META[s].label.toLowerCase()}</span>}</For>
      </Show>
    </p></Show>
  </>;
}

import { For, Show } from "solid-js";
import type { AgentEvent } from "../api/types";
import { humanize } from "../lib/format";

const time = (iso: string) => new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });

export function EventList(props: { events: AgentEvent[]; showAgent?: boolean; limit?: number; listRef?: (el: HTMLDivElement) => void }) {
  const shown = () => props.events.slice(0, props.limit ?? 200);
  return (
    <Show when={shown().length} fallback={<div class="empty">No events yet.</div>}>
      <div class="event-list" ref={(el) => props.listRef?.(el)}>
        <For each={shown()}>
          {(e) => (
            <div class={`event-row${e.kind === "self_report" ? " self-report-row" : ""}`} title={e.kind === "self_report" ? "The agent's own word (terrarium status set). It does not change the runner status." : undefined}>
              <span class="muted mono small">{time(e.at)}</span>
              <span class="kind">
                <span class={`chip${e.kind === "quota.alert" || e.kind === "quota.blocked" || e.kind === "usage_limit" ? " warn" : e.kind === "self_report" ? " info" : ""}`}>{e.kind === "self_report" ? "says" : humanize(e.kind)}</span>
              </span>
              <span class="ellipsis" title={e.summary}>
                <Show when={props.showAgent && e.agent}>
                  <strong>{e.agent}</strong>
                  <span class="muted">{e.project ? ` @${e.project}` : ""}</span>:{" "}
                </Show>
                {e.summary}
              </span>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}

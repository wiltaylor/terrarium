import { For, Show } from "solid-js";
import type { AgentEvent } from "../api/types";
import { humanize } from "../lib/format";

const time = (iso: string) => new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });

export function EventList(props: { events: AgentEvent[]; showAgent?: boolean; limit?: number }) {
  const shown = () => props.events.slice(0, props.limit ?? 200);
  return (
    <Show when={shown().length} fallback={<div class="empty">No events yet.</div>}>
      <div class="event-list">
        <For each={shown()}>
          {(e) => (
            <div class="event-row">
              <span class="muted mono small">{time(e.at)}</span>
              <span class="kind">
                <span class="chip">{humanize(e.kind)}</span>
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

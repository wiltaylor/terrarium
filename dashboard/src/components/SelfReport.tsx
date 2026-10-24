// The agent's own word (`self_report`, `terrarium status set`). It sits apart
// from the runner status badge: the runner status is what the supervisor
// sees; this is what the agent says (docs/api.md §16.6).

import { Show } from "solid-js";
import type { Agent } from "../api/types";
import { ago } from "../lib/format";

export function SelfReport(props: { report: Agent["self_report"]; now?: number }) {
  return (
    <Show when={props.report}>
      {(r) => (
        <span class="self-report" title={`Self-report ${ago(r().at, props.now)}. The agent's own word; the runner status is the badge.`}>
          says <strong>{r().status}</strong>
          <Show when={r().note}>
            <span class="ellipsis" style={{ "max-width": "28ch" }}>
              · {r().note}
            </span>
          </Show>
        </span>
      )}
    </Show>
  );
}

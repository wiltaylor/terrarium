// The quota hold badge (PLAN §16.2): an agent whose new turns wait after a
// usage-limit error of the provider, or for a budget reset. There is no quota banner: each quota line
// (`alert_at`, 90, used up) is one mail in the Inbox.

import { Show } from "solid-js";
import { quota } from "../stores";
import { describeCause, type AgentHold } from "../stores/quota";

/** A badge for an agent whose turns wait after a usage-limit error or for a budget reset. */
export function HoldBadge(props: { agentId: string; now?: number; long?: boolean }) {
  const holds = () => quota.holdsFor(props.agentId, props.now);
  return (
    <Show when={holds()[0]}>
      {(h) => (
        <span class="chip bad hold-badge" title={holdTitle(h())}>
          {props.long ? holdLine(h()) : "quota hold"}
        </span>
      )}
    </Show>
  );
}

export const holdLine = (h: AgentHold): string =>
  `blocked by ${h.cause.startsWith("limit:") ? "a usage limit" : "quota"} until ${h.until ? new Date(h.until).toLocaleString() : "further notice"}`;
const holdTitle = (h: AgentHold): string =>
  `${holdLine(h)}: ${describeCause(h.cause)}. ${h.reason}${h.enforced ? "" : " (advice only: the supervisor does not hold turns yet)"}`;

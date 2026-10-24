// The quota banner (PLAN §16.2): a window over `alert_at`, or used up.

import { A } from "@solidjs/router";
import { For, Show } from "solid-js";
import { timeUntil } from "../lib/format";
import { agents, quota } from "../stores";
import { alertText, describeCause, type AgentHold } from "../stores/quota";
import { useNow } from "./ui";

export function QuotaBanner() {
  const now = useNow(30_000);
  const alerts = () => quota.alerts(now());
  const held = () => quota.state().holds.filter((h) => !h.until || Date.parse(h.until) > now());
  return (
    <Show when={alerts().length}>
      <div class="quota-banner stack" role="alert" style={{ gap: "6px" }}>
        <For each={alerts()}>
          {(a) => (
            <div class={`spread quota-alert ${a.level}`}>
              <span>
                <strong>{a.level === "exhausted" ? "Quota used up" : "Quota alert"}:</strong> {alertText(a)}
                <Show when={a.resets_at}> Resets in {timeUntil(a.resets_at!, now())}.</Show>
                <Show when={a.level === "exhausted" && held().length}>
                  {" "}
                  Held: {held().filter((h) => !a.subscription || h.cause === `quota:${a.subscription}`).map((h) => h.agent ?? agents.byId(h.agent_id)?.name ?? h.agent_id).join(", ") || "none"}.
                </Show>
              </span>
              <span class="row">
                <A class="btn small" href="/subscriptions">
                  Subscriptions
                </A>
                <button class="small ghost" onClick={() => quota.dismiss(a)} aria-label="Dismiss">
                  ✕
                </button>
              </span>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}

/** A badge for an agent whose turns wait for a quota or budget reset. */
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

export const holdLine = (h: AgentHold): string => `blocked by quota until ${h.until ? new Date(h.until).toLocaleString() : "further notice"}`;
const holdTitle = (h: AgentHold): string =>
  `${holdLine(h)}: ${describeCause(h.cause)}. ${h.reason}${h.enforced ? "" : " (advice only: the supervisor does not hold turns yet)"}`;

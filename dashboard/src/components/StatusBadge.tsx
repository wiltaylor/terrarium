import { Show } from "solid-js";
import type { Agent, AgentStatus } from "../api/types";
import { lifecycleBadge, queuedLabel } from "../lib/lifecycle";

/** The title of a status badge: a waiting agent lists what it waits on. */
export const statusTitle = (status: AgentStatus, waitingOn?: string): string =>
  status === "waiting" && waitingOn ? `Status: waiting on ${waitingOn}` : `Status: ${status}`;

/** The badge of an agent: its status, `paused`, or `stopped (idle)`; and the queued count when work waits. */
export function StatusBadge(props: { agent: Pick<Agent, "status" | "paused" | "status_reason" | "waiting_on" | "queued"> }) {
  const b = () => lifecycleBadge(props.agent);
  return (
    <>
      <span class={`badge ${b().cls}`} title={b().title}>
        {b().label}
      </span>
      <Show when={queuedLabel(props.agent)}>
        {(q) => (
          <span class="chip info" title="Turns, mail, and cards that wait for the agent.">
            {q()}
          </span>
        )}
      </Show>
    </>
  );
}

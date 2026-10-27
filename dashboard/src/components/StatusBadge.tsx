import type { AgentStatus } from "../api/types";

/** The title of a status badge: a waiting agent lists what it waits on. */
export const statusTitle = (status: AgentStatus, waitingOn?: string): string =>
  status === "waiting" && waitingOn ? `Status: waiting on ${waitingOn}` : `Status: ${status}`;

export function StatusBadge(props: { status: AgentStatus; waitingOn?: string }) {
  return (
    <span class={`badge st-${props.status}`} title={statusTitle(props.status, props.waitingOn)}>
      {props.status}
    </span>
  );
}

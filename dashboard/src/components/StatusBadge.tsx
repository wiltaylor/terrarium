import type { AgentStatus } from "../api/types";

export function StatusBadge(props: { status: AgentStatus }) {
  return (
    <span class={`badge st-${props.status}`} title={`Status: ${props.status}`}>
      {props.status}
    </span>
  );
}

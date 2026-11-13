import { Show } from "solid-js";
import type { Subscription } from "../api/types";

export function AssignedAgentsChip(props: { sub: Subscription }) {
  const known = () => props.sub.assigned_agent_count !== undefined && (props.sub.assigned_agent_count_scope === "all_projects" || (props.sub.assigned_agent_count_scope === "project" && !!props.sub.assigned_agent_count_project));
  const scope = () => props.sub.assigned_agent_count_scope === "project" ? ` in project ${props.sub.assigned_agent_count_project}` : " across all projects";
  return <span class="chip" title={known() ? `Effective stored assignments${scope()}, including inherited defaults and all statuses. Not running agents.` : "Assigned-agent count unavailable"}>
    <Show when={known()} fallback={"Agents unavailable"}>
      {props.sub.assigned_agent_count} agents{props.sub.assigned_agent_count_scope === "project" ? " in this project" : ""}
    </Show>
  </span>;
}

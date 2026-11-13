import { useParams } from "@solidjs/router";
import { createSignal } from "solid-js";
import { decodePathParam } from "../../lib/url";
import { AgentsPanel, CardAgentsPanel } from "./AgentsPanel";
export { AgentCard, RoleBadges } from "./AgentsPanel";

export function AgentsPage() {
  const route = useParams<{ project: string }>();
  const project = () => decodePathParam(route.project);
  const [refresh, setRefresh] = createSignal(0);
  return <>
    <AgentsPanel project={project()} onRefresh={() => setRefresh(n => n + 1)} />
    <CardAgentsPanel project={project()} refreshKey={refresh()} />
  </>;
}

import { createSignal, For } from "solid-js";
import { EventHistory } from "../../components/EventHistory";
import { PageHead } from "../../components/ui";
import { EVENT_KINDS, type EventKind } from "../../api/types";
import { agents, api, projects } from "../../stores";

export default function EventsPage() {
  const [kind, setKind] = createSignal<EventKind | "">("");
  const [scope, setScope] = createSignal<"all" | "project">("all");
  const [agent, setAgent] = createSignal("");
  const project = () => (scope() === "project" ? (projects.selected() ?? undefined) : undefined);

  return (
    <>
      <PageHead title="Event log">
        <select value={scope()} onChange={(e) => setScope(e.currentTarget.value as "all" | "project")} aria-label="Scope">
          <option value="all">All projects</option>
          <option value="project">Project: {projects.selected()}</option>
        </select>
        <select value={agent()} onChange={(e) => setAgent(e.currentTarget.value)} aria-label="Agent">
          <option value="">Any agent</option>
          <For each={agents.inProject(project())}>{(a) => <option value={a.id}>{`${a.project}/${a.name}`}</option>}</For>
        </select>
        <select value={kind()} onChange={(e) => setKind(e.currentTarget.value as EventKind | "")} aria-label="Kind">
          <option value="">Any kind</option>
          <For each={EVENT_KINDS}>{(k) => <option value={k}>{k}</option>}</For>
        </select>
      </PageHead>
      <div class="card-box">
        <EventHistory
          filterKey={`${kind()}|${project() ?? ""}|${agent()}`}
          fetch={(q) => api.events.history({ ...q, kind: kind() || undefined, project: project(), agent: agent() || undefined })}
          match={(e) => (!kind() || e.kind === kind()) && (!project() || e.project === project()) && (!agent() || e.agent_id === agent())}
          showAgent
        />
      </div>
    </>
  );
}

import { useInstance } from "../../instance";
// The Events tab of the Logs screen: the event history in the live mode of
// the log tabs (components/Live.tsx), with filters by kind and agent. `/logs/events` can show every project or
// the selected one. `/p/:project/logs/events` shows the project of the URL
// only, and the agent filter lists the agents of that project.

import { useParams } from "@solidjs/router";
import { createEffect, createSignal, For, on, Show } from "solid-js";
import { EventHistory } from "../../components/EventHistory";
import { createLive, LiveControl } from "../../components/Live";
import { PageHead } from "../../components/ui";
import { EVENT_KINDS, type EventKind } from "../../api/types";


export default function EventsPage() {
  const { projects, agents, api } = useInstance();
  const route = useParams<{ project?: string }>();
  const [kind, setKind] = createSignal<EventKind | "">("");
  const [scope, setScope] = createSignal<"all" | "project">("all");
  const [agent, setAgent] = createSignal("");
  /** The project of the route wins over the scope. */
  const project = () => route.project ?? (scope() === "project" ? (projects.selected() ?? undefined) : undefined);
  // An agent of the last project is not in the list of a new one.
  createEffect(on(project, () => setAgent(""), { defer: true }));
  const live = createLive();

  return (
    <>
      <PageHead title="Events" level={2}>
        <LiveControl live={live} count={live.state().held.length} />
        <Show when={!route.project}>
          <select value={scope()} onChange={(e) => setScope(e.currentTarget.value as "all" | "project")} aria-label="Scope">
            <option value="all">All projects</option>
            <option value="project">Project: {projects.selected()}</option>
          </select>
        </Show>
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
          live={live}
        />
      </div>
    </>
  );
}

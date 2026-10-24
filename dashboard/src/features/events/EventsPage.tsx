import { createSignal, For } from "solid-js";
import { EventList } from "../../components/EventList";
import { PageHead } from "../../components/ui";
import type { EventKind } from "../../api/types";
import { events, projects } from "../../stores";

const KINDS: EventKind[] = ["turn_start", "turn_end", "tool_call", "status", "exit", "mail", "card", "memory", "wiki", "version", "subscription"];

export default function EventsPage() {
  const [kind, setKind] = createSignal<EventKind | "">("");
  const [scope, setScope] = createSignal<"all" | "project">("all");
  const shown = () =>
    events.recent().filter((e) => (!kind() || e.kind === kind()) && (scope() === "all" || e.project === projects.selected()));
  return (
    <>
      <PageHead title="Event log">
        <select value={scope()} onChange={(e) => setScope(e.currentTarget.value as "all" | "project")} aria-label="Scope">
          <option value="all">All projects</option>
          <option value="project">Project: {projects.selected()}</option>
        </select>
        <select value={kind()} onChange={(e) => setKind(e.currentTarget.value as EventKind | "")} aria-label="Kind">
          <option value="">Any kind</option>
          <For each={KINDS}>{(k) => <option value={k}>{k}</option>}</For>
        </select>
        <span class="muted small">live since page load · {events.state()}</span>
      </PageHead>
      <div class="card-box">
        <EventList events={shown()} showAgent />
      </div>
    </>
  );
}

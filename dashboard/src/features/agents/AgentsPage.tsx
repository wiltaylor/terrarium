import { A, useNavigate, useParams } from "@solidjs/router";
import { createSignal, For, onMount, Show } from "solid-js";
import { LeaseChips } from "../../components/Leases";
import { HoldBadge } from "../../components/HoldBadge";
import { StatusBadge } from "../../components/StatusBadge";
import { Empty, ErrorBanner, PageHead, useNow } from "../../components/ui";
import { AGENT_STATUSES, type Agent, type AgentStatus } from "../../api/types";
import { duration } from "../../lib/format";
import { agents, leases } from "../../stores";
import { NewAgentDialog } from "./NewAgentDialog";

export function RoleBadges(props: { agent: Agent }) {
  return (
    <For each={props.agent.roles.filter((r) => r !== "member")}>
      {(r) => <span class={`chip ${r === "sysadmin" ? "bad" : "info"}`}>{r}</span>}
    </For>
  );
}

export function AgentCard(props: { agent: Agent; now: number }) {
  const a = () => props.agent;
  return (
    <div class="card-box agent-card">
      <div class="spread">
        <A class="name" href={`/agents/${encodeURIComponent(a().id)}`}>
          {a().name}
        </A>
        <span class="row" style={{ gap: "4px", "flex-wrap": "wrap", "justify-content": "flex-end" }}>
          <LeaseChips agentId={a().id} now={props.now} />
          <HoldBadge agentId={a().id} now={props.now} />
          <StatusBadge agent={a()} />
        </span>
      </div>
      <div class="row">
        <span class="chip">{a().harness}</span>
        <span class="chip">{a().project}</span>
        <Show when={a().model}>
          <span class="chip mono small" title={a().model}>
            {a().model}
          </span>
        </Show>
        <RoleBadges agent={a()} />
        <Show when={a().skills_restart}>
          <span class="chip warn" title="The live agent runs with other skills than its list. A restart applies the list.">
            restart to apply skills
          </span>
        </Show>
      </div>
      <dl class="kv">
        <dt>Card</dt>
        <dd>
          <Show when={a().current_card} fallback={<span class="muted">none</span>}>
            {(c) => <A href={`/p/${a().project}/board?card=${c()}`}>{c()}</A>}
          </Show>
        </dd>
        <Show when={a().waiting_on}>
          {(w) => (
            <>
              <dt>Waiting on</dt>
              <dd class="ellipsis" title={w()}>
                {w()}
              </dd>
            </>
          )}
        </Show>
        <Show when={a().turn_started_at}>
          {(t) => (
            <>
              <dt>Turn</dt>
              <dd>{duration(props.now - Date.parse(t()))}</dd>
            </>
          )}
        </Show>
      </dl>
    </div>
  );
}

/** The agents of the route project (`/p/:project/agents`). */
export function AgentsPage() {
  const now = useNow(1000);
  const navigate = useNavigate();
  const route = useParams<{ project: string }>();
  const [creating, setCreating] = createSignal(false);
  // The "holds <target>" chips: the leases older than the event backlog.
  onMount(() => void leases.loadAll());
  // `paused` is not a status: a paused agent is `stopped` with the pause flag.
  const [status, setStatus] = createSignal<AgentStatus | "paused" | "">("");
  const shown = () => agents.inProject(route.project).filter((a) => !status() || (status() === "paused" ? a.paused : a.status === status()));

  return (
    <>
      <PageHead title={`Agents · ${route.project}`}>
        <select value={status()} onChange={(e) => setStatus(e.currentTarget.value as AgentStatus | "paused" | "")} aria-label="Status filter">
          <option value="">Any status</option>
          <For each={AGENT_STATUSES}>{(s) => <option value={s}>{s}</option>}</For>
          <option value="paused">paused</option>
        </select>
        <button onClick={() => agents.refresh()}>Refresh</button>
        <button class="primary" onClick={() => setCreating(true)}>
          New agent
        </button>
      </PageHead>
      <Show when={creating()}>
        <NewAgentDialog project={route.project} onClose={() => setCreating(false)} onSaved={(a) => navigate(`/agents/${encodeURIComponent(a.id)}`)} />
      </Show>
      <ErrorBanner error={agents.state.error} />
      <Show when={shown().length} fallback={<Empty>{agents.state.loading ? "Loading…" : "No agents match."}</Empty>}>
        <div class="grid">
          <For each={shown()}>{(a) => <AgentCard agent={a} now={now()} />}</For>
        </div>
      </Show>
    </>
  );
}

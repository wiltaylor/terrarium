import { useInstance } from "../../instance";
import { A, useNavigate } from "../../instance/navigation";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { LeaseChips } from "../../components/Leases";
import { HoldBadge } from "../../components/HoldBadge";
import { StatusBadge } from "../../components/StatusBadge";
import { Empty, ErrorBanner, PageHead, useNow } from "../../components/ui";
import { AGENT_STATUSES, roleLabel, type Agent, type AgentStatus } from "../../api/types";
import { duration } from "../../lib/format";

import { PinButton } from "../../app/PinnedBar";
import { NewAgentDialog } from "./NewAgentDialog";

export function RoleBadges(props: { agent: Agent }) {
  return (
    <For each={props.agent.roles.filter((r) => r !== "member")}>
      {(r) => <span class={`chip ${r === "sysadmin" ? "bad" : "info"}`}>{roleLabel(r)}</span>}
    </For>
  );
}

export function AgentCard(props: { agent: Agent; now: number; onError?: (e: unknown) => void }) {
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
          {/* The pin the sidebar agent rows had (TER-153). An ephemeral agent is short-lived: no pin, as on the agent page. */}
          <Show when={!a().ephemeral}>
            <PinButton agent={a()} class="small" onError={props.onError} />
          </Show>
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
          <span class="chip warn" title="The live agent runs with other skills than its effective skills now (its list, or the default skills, changed). A restart applies them.">
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

export function AgentsPanel(props: { project: string; onRefresh?: () => void }) {
  const { leases, agents } = useInstance();
  const now = useNow(1000);
  const navigate = useNavigate();
  const project = () => props.project;
  const [creating, setCreating] = createSignal(false);
  const [pinError, setPinError] = createSignal<unknown>(null);
  // The "holds <target>" chips: the leases older than the event backlog.
  onMount(() => void leases.loadAll());
  // `paused` is not a status: a paused agent is `stopped` with the pause flag.
  const [status, setStatus] = createSignal<AgentStatus | "paused" | "">("");
  const shown = () => agents.inProject(project()).filter((a) => !status() || (status() === "paused" ? a.paused : a.status === status()));

  return (
    <>
      <PageHead title={`Agents · ${project()}`} level={2}>
        <select value={status()} onChange={(e) => setStatus(e.currentTarget.value as AgentStatus | "paused" | "")} aria-label="Status filter">
          <option value="">Any status</option>
          <For each={AGENT_STATUSES}>{(s) => <option value={s}>{s}</option>}</For>
          <option value="paused">paused</option>
        </select>
        <button
          onClick={() => {
            void agents.refresh();
            props.onRefresh?.();
          }}
        >
          Refresh
        </button>
        <button class="primary" onClick={() => setCreating(true)}>
          New agent
        </button>
      </PageHead>
      <Show when={creating()}>
        <NewAgentDialog project={project()} onClose={() => setCreating(false)} onSaved={(a) => navigate(`/agents/${encodeURIComponent(a.id)}`)} />
      </Show>
      <ErrorBanner error={agents.state.error} />
      <ErrorBanner error={pinError()} onDismiss={() => setPinError(null)} />
      <Show when={shown().length} fallback={<Empty>{agents.state.loading ? "Loading…" : "No agents match."}</Empty>}>
        <div class="grid">
          <For each={shown()}>{(a) => <AgentCard agent={a} now={now()} onError={setPinError} />}</For>
        </div>
      </Show>

    </>
  );
}

// `empty` is the text that a page widget shows when the project has no card agents.
// The Agents tab leaves it out and shows nothing.
type CardAgentsProps = { project: string; refreshKey?: number; empty?: string };
export function CardAgentsPanel(props: CardAgentsProps) {
  return <Show when={props.project} keyed>{(project) => <CardAgentsContent {...props} project={project} />}</Show>;
}

function CardAgentsContent(props: CardAgentsProps) {
  const { leases, api, events } = useInstance();
  const now = useNow(1000);
  const project = () => props.project;
  onMount(() => void leases.loadAll());
  // The card agents (TER-75): the ephemeral agents bound to a card or assigned
  // one. The agents store leaves ephemeral agents out, so the page reads them.
  const [cardAgents, { refetch }] = createResource(
    () => [project(), props.refreshKey] as const,
    async ([p]) => (await api.agents.list(p, { card: true })).filter((a) => a.ephemeral),
  );
  let pending: ReturnType<typeof setTimeout> | undefined;
  const off = events.on((e) => {
    if (e.project !== project() || !(e.kind === "card" || e.kind === "status" || e.kind.startsWith("agent."))) return;
    clearTimeout(pending);
    pending = setTimeout(() => void refetch(), 1000);
  });
  onCleanup(() => {
    clearTimeout(pending);
    off();
  });

  return <><ErrorBanner error={props.empty === undefined ? undefined : cardAgents.error} />
      <Show when={(cardAgents.latest ?? []).length} fallback={<Show when={props.empty !== undefined && cardAgents.state === "ready"}><Empty>{props.empty}</Empty></Show>}>
        <h3>Card agents</h3>
        <div class="grid card-agents">
          <For each={cardAgents.latest}>{(a) => <AgentCard agent={a} now={now()} />}</For>
        </div>
      </Show>
  </>;
}

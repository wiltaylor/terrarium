import { A, useNavigate } from "@solidjs/router";
import { createSignal, For, Show } from "solid-js";
import { HoldBadge } from "../../components/QuotaBanner";
import { SelfReport } from "../../components/SelfReport";
import { StatusBadge } from "../../components/StatusBadge";
import { Empty, ErrorBanner, PageHead, useNow } from "../../components/ui";
import { AGENT_STATUSES, type Agent, type AgentStatus } from "../../api/types";
import { ago, duration } from "../../lib/format";
import { agents, projects } from "../../stores";
import { AgentForm } from "./AgentForm";

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
        <span class="row" style={{ gap: "4px" }}>
          <HoldBadge agentId={a().id} now={props.now} />
          <StatusBadge status={a().status} />
        </span>
      </div>
      <SelfReport report={a().self_report} now={props.now} />
      <div class="row">
        <span class="chip">{a().harness}</span>
        <span class="chip">{a().project}</span>
        <Show when={a().model}>
          <span class="chip mono small">{a().model}</span>
        </Show>
        <RoleBadges agent={a()} />
      </div>
      <dl class="kv">
        <dt>Card</dt>
        <dd>
          <Show when={a().current_card} fallback={<span class="muted">none</span>}>
            {(c) => <A href={`/p/${a().project}/board?card=${c()}`}>{c()}</A>}
          </Show>
        </dd>
        <Show when={a().turn_started_at}>
          {(t) => (
            <>
              <dt>Turn</dt>
              <dd>{duration(props.now - Date.parse(t()))}</dd>
            </>
          )}
        </Show>
        <dt>Last event</dt>
        <dd class="ellipsis" title={a().last_event?.summary}>
          <Show when={a().last_event} fallback={<span class="muted">none</span>}>
            {(e) => (
              <>
                {e().summary} <span class="muted">· {ago(e().at, props.now)}</span>
              </>
            )}
          </Show>
        </dd>
      </dl>
    </div>
  );
}

export function AgentsPage() {
  const now = useNow(1000);
  const navigate = useNavigate();
  const [creating, setCreating] = createSignal(false);
  const [scope, setScope] = createSignal<"project" | "all">("project");
  const [status, setStatus] = createSignal<AgentStatus | "">("");
  const shown = () =>
    agents
      .inProject(scope() === "project" ? projects.selected() : null)
      .filter((a) => !status() || a.status === status());

  return (
    <>
      <PageHead title="Agents">
        <select value={scope()} onChange={(e) => setScope(e.currentTarget.value as "project" | "all")} aria-label="Scope">
          <option value="project">Project: {projects.selected() ?? "none"}</option>
          <option value="all">All projects</option>
        </select>
        <select value={status()} onChange={(e) => setStatus(e.currentTarget.value as AgentStatus | "")} aria-label="Status filter">
          <option value="">Any status</option>
          <For each={AGENT_STATUSES}>{(s) => <option value={s}>{s}</option>}</For>
        </select>
        <button onClick={() => agents.refresh()}>Refresh</button>
        <button class="primary" onClick={() => setCreating(true)}>
          New agent
        </button>
      </PageHead>
      <Show when={creating()}>
        <AgentForm project={projects.selected()} onClose={() => setCreating(false)} onSaved={(a) => navigate(`/agents/${encodeURIComponent(a.id)}`)} />
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

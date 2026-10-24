// Agent list with live status from the event stream.

import { createStore, reconcile } from "solid-js/store";
import type { Api } from "../api/client";
import { AGENT_STATUSES, type Agent, type AgentEvent, type AgentStatus } from "../api/types";
import type { EventsStore } from "./events";

const isStatus = (s: unknown): s is AgentStatus => AGENT_STATUSES.includes(s as AgentStatus);

/** The agent ULID of an event: `agent_id`, or `data.agent_id`. */
export const eventAgentId = (e: AgentEvent): string | undefined =>
  e.agent_id ?? (typeof e.data?.agent_id === "string" ? e.data.agent_id : undefined);

/** Find the agent an event is about. By ULID first, then by name and project. */
export function findEventAgent(agents: Agent[], e: AgentEvent): number {
  const id = eventAgentId(e);
  if (id) return agents.findIndex((a) => a.id === id);
  if (!e.agent) return -1;
  return agents.findIndex((a) => a.name === e.agent && (!e.project || a.project === e.project));
}

/** Apply one event to the agent list. Pure; returns a new array when something changed. */
export function applyAgentEvent(agents: Agent[], e: AgentEvent): Agent[] {
  const i = findEventAgent(agents, e);
  if (i < 0) return agents;
  const a = { ...agents[i]!, last_event: e };
  const status = e.data?.status;
  if (e.kind === "status" && isStatus(status)) {
    a.status = status;
    if (status === "working") a.turn_started_at ??= e.at;
    else delete a.turn_started_at;
  }
  if (e.kind === "session_started" && typeof e.data?.harness_session === "string") a.session = e.data.harness_session;
  if (e.kind === "turn_start") {
    a.status = "working";
    a.turn_started_at = e.at;
  }
  if (e.kind === "turn_end") {
    if (a.status === "working") a.status = "idle";
    delete a.turn_started_at;
  }
  if (e.kind === "exit") a.status = "error";
  if (e.kind === "self_report") {
    // The agent's own word. It does not change the runner status (docs/api.md §16.6).
    a.self_report = { status: String(e.data?.status ?? ""), at: e.at, ...(typeof e.data?.note === "string" && e.data.note ? { note: e.data.note } : {}) };
  }
  if (e.kind === "card") {
    // The `agent` of a card event is the assignee (docs/api.md §9).
    const card = typeof e.data?.card_id === "string" ? e.data.card_id : e.card;
    const column = e.data?.column;
    if (card && column === "in_progress") a.current_card = card;
    else if (card && a.current_card === card && typeof column === "string") delete a.current_card;
  }
  const next = agents.slice();
  next[i] = a;
  return next;
}

export function createAgentsStore(api: Pick<Api, "agents">, events?: Pick<EventsStore, "on"> & Partial<Pick<EventsStore, "recent">>) {
  const [state, setState] = createStore<{ agents: Agent[]; loading: boolean; error: unknown }>({
    agents: [],
    loading: false,
    error: null,
  });
  let pending: ReturnType<typeof setTimeout> | undefined;

  const refresh = async () => {
    setState("loading", true);
    try {
      const list = await api.agents.list();
      // Keep the client-side fields (turn age, last event) across a refresh.
      const merged = list.map((a) => {
        const old = state.agents.find((x) => x.id === a.id);
        return old ? { ...a, last_event: old.last_event, self_report: old.self_report, turn_started_at: a.status === "working" ? old.turn_started_at : undefined } : a;
      });
      // Events that came before the list (the socket backlog) apply now, oldest first.
      const replayed = (events?.recent?.() ?? []).slice().reverse().reduce(applyAgentEvent, merged);
      // The list is the truth for status; the replay adds last events, turn start, and the card.
      const final = replayed.map((a, i) => {
        const fresh = merged[i]!;
        return { ...a, status: fresh.status, current_card: fresh.current_card, turn_started_at: fresh.status === "working" ? a.turn_started_at : undefined };
      });
      setState("agents", reconcile(final, { key: "id" }));
      setState("error", null);
    } catch (e) {
      setState("error", e);
    } finally {
      setState("loading", false);
    }
  };

  /** Refresh soon. Many calls in a burst give one request. */
  const refreshSoon = (ms = 500) => {
    clearTimeout(pending);
    pending = setTimeout(() => void refresh(), ms);
  };

  const unsubscribe = events?.on((e) => {
    const next = applyAgentEvent(state.agents, e);
    if (next !== state.agents) setState("agents", reconcile(next, { key: "id" }));
    else if (eventAgentId(e) && !state.loading) refreshSoon(); // an agent this list does not have yet
  });

  return {
    state,
    refresh,
    refreshSoon,
    byId: (id: string) => state.agents.find((a) => a.id === id || `${a.project}/${a.name}` === id),
    byName: (project: string, name: string) => state.agents.find((a) => a.project === project && a.name === name),
    inProject: (project?: string | null) => state.agents.filter((a) => !project || a.project === project),
    /** Put a fresh copy from the daemon into the list. */
    upsert(agent: Agent) {
      const i = state.agents.findIndex((a) => a.id === agent.id);
      if (i < 0) setState("agents", (list) => [...list, agent]);
      else setState("agents", i, (old) => ({ ...agent, last_event: old.last_event, self_report: old.self_report, turn_started_at: old.turn_started_at }));
    },
    drop(id: string) {
      setState("agents", (list) => list.filter((a) => a.id !== id));
    },
    dispose: () => {
      clearTimeout(pending);
      unsubscribe?.();
    },
  };
}

export type AgentsStore = ReturnType<typeof createAgentsStore>;

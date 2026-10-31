// Agent list with live status from the event stream.

import { createStore, reconcile } from "solid-js/store";
import type { Api } from "../api/client";
import { AGENT_STATUSES, type Agent, type AgentEvent, type AgentStatus } from "../api/types";
import type { EventsStore } from "./events";

/** The lifecycle events after which the list is read again (the queued count). */
const LIFECYCLE = new Set<AgentEvent["kind"]>(["agent.paused", "agent.unpaused", "agent.woken"]);

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
  // A removal names the agent by ULID only: a new agent can have the old name.
  if (e.kind === "agent_removed") return removeAgent(agents, eventAgentId(e));
  const i = findEventAgent(agents, e);
  if (i < 0) return agents;
  const a = { ...agents[i]!, last_event: e };
  const status = e.data?.status;
  if (e.kind === "status" && isStatus(status)) {
    a.status = status;
    // `idle` for an idle stop, `paused`, `stopped`, crash text, and so on.
    const reason = e.data?.reason;
    if (typeof reason === "string" && reason) a.status_reason = reason;
    else delete a.status_reason;
    // The open waits come with the status (absent: none).
    const waits = e.data?.waits;
    const on = e.data?.waiting_on;
    if (typeof waits === "number" && waits > 0) a.waits = waits;
    else delete a.waits;
    if (typeof on === "string" && on) a.waiting_on = on;
    else delete a.waiting_on;
    if (status === "working") a.turn_started_at ??= e.at;
    else delete a.turn_started_at;
  }
  if (e.kind === "session_started" && typeof e.data?.harness_session === "string") a.session = e.data.harness_session;
  if (e.kind === "turn_start") {
    a.status = "working";
    a.turn_started_at = e.at;
  }
  if (e.kind === "turn_end") {
    if (a.status === "working") a.status = a.waits ? "waiting" : "idle";
    delete a.turn_started_at;
  }
  if (e.kind === "exit") a.status = "error";
  if (e.kind === "agent.paused") a.paused = true;
  if (e.kind === "agent.unpaused") a.paused = false;
  // The end of a compaction that the daemon ran (PLAN §18, "Auto compact").
  if (e.kind === "agent.compacted") {
    a.last_compact_at = e.at;
    a.last_compact_ok = e.data?.ok !== false;
  }
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

/** The list without the agent `id`. Pure; the same array when the list does not have it. */
export function removeAgent(agents: Agent[], id: string | undefined): Agent[] {
  if (!id || !agents.some((a) => a.id === id)) return agents;
  return agents.filter((a) => a.id !== id);
}

/**
 * Put a fresh list from the daemon together with the events that came before it
 * (the socket backlog, newest first). The fresh list is the truth for which agents
 * there are and for their status; the replay adds the last events, the turn start,
 * and the self-report. A replayed event cannot add an agent, and an agent in
 * `removed` stays out (a list that was fetched before the delete).
 */
export function mergeFresh(fresh: Agent[], old: Agent[], recent: AgentEvent[], removed: ReadonlySet<string> = new Set()): Agent[] {
  const merged = fresh
    .filter((a) => !removed.has(a.id))
    .map((a) => {
      const o = old.find((x) => x.id === a.id);
      return o ? { ...a, last_event: o.last_event, self_report: o.self_report, turn_started_at: a.status === "working" ? o.turn_started_at : undefined } : a;
    });
  // Oldest first.
  const replayed = recent.slice().reverse().reduce(applyAgentEvent, merged);
  return replayed.map((a) => {
    const f = merged.find((x) => x.id === a.id)!;
    return { ...a, status: f.status, status_reason: f.status_reason, paused: f.paused, current_card: f.current_card, waits: f.waits, waiting_on: f.waiting_on, turn_started_at: f.status === "working" ? a.turn_started_at : undefined };
  });
}

export function createAgentsStore(api: Pick<Api, "agents">, events?: Pick<EventsStore, "on"> & Partial<Pick<EventsStore, "recent">>) {
  const [state, setState] = createStore<{ agents: Agent[]; loading: boolean; error: unknown }>({
    agents: [],
    loading: false,
    error: null,
  });
  let pending: ReturnType<typeof setTimeout> | undefined;
  // The agents the daemon removed in this session. Their later events do not
  // ask for a refresh, and a list fetched before the removal does not bring them back.
  const removed = new Set<string>();

  const refresh = async () => {
    setState("loading", true);
    try {
      const list = await api.agents.list();
      const final = mergeFresh(list, state.agents, events?.recent?.() ?? [], removed);
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
    const id = eventAgentId(e);
    if (e.kind === "agent_removed" && id) removed.add(id);
    const next = applyAgentEvent(state.agents, e);
    const known = next !== state.agents;
    if (known) setState("agents", reconcile(next, { key: "id" }));
    else if (id && !removed.has(id) && !state.loading) refreshSoon(); // an agent this list does not have yet
    // No event carries the queued count: a wake, a pause, an unpause, or a stop changes it.
    if (known && (LIFECYCLE.has(e.kind) || (e.kind === "status" && e.data?.status === "stopped"))) refreshSoon();
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
      removed.add(id);
      setState("agents", (list) => list.filter((a) => a.id !== id));
    },
    dispose: () => {
      clearTimeout(pending);
      unsubscribe?.();
    },
  };
}

export type AgentsStore = ReturnType<typeof createAgentsStore>;

// Agent list with live status from the event stream.

import { createStore, reconcile } from "solid-js/store";
import type { Api } from "../api/client";
import { AGENT_STATUSES, type Agent, type AgentEvent, type AgentStatus } from "../api/types";
import type { EventsStore } from "./events";

const isStatus = (s: unknown): s is AgentStatus => AGENT_STATUSES.includes(s as AgentStatus);

/** Apply one event to the agent list. Pure; returns a new array when something changed. */
export function applyAgentEvent(agents: Agent[], e: AgentEvent): Agent[] {
  if (!e.agent) return agents;
  const i = agents.findIndex((a) => (a.id === e.data?.agent_id) || (a.name === e.agent && (!e.project || a.project === e.project)));
  if (i < 0) return agents;
  const a = { ...agents[i]!, last_event: e };
  const status = e.data?.status;
  if (e.kind === "status" && isStatus(status)) {
    a.status = status;
    if (status === "working") a.turn_started_at = e.at;
    else delete a.turn_started_at;
  }
  if (e.kind === "turn_start") {
    a.status = "working";
    a.turn_started_at = e.at;
  }
  if (e.kind === "turn_end" && a.status === "working") {
    a.status = "idle";
    delete a.turn_started_at;
  }
  if (e.kind === "exit") a.status = "error";
  const next = agents.slice();
  next[i] = a;
  return next;
}

export function createAgentsStore(api: Pick<Api, "agents">, events?: Pick<EventsStore, "on">) {
  const [state, setState] = createStore<{ agents: Agent[]; loading: boolean; error: string | null }>({
    agents: [],
    loading: false,
    error: null,
  });

  const refresh = async () => {
    setState("loading", true);
    try {
      const list = await api.agents.list();
      setState("agents", reconcile(list, { key: "id" }));
      setState("error", null);
    } catch (e) {
      setState("error", (e as Error).message);
    } finally {
      setState("loading", false);
    }
  };

  const unsubscribe = events?.on((e) => {
    const next = applyAgentEvent(state.agents, e);
    if (next !== state.agents) setState("agents", reconcile(next, { key: "id" }));
  });

  return {
    state,
    refresh,
    byId: (id: string) => state.agents.find((a) => a.id === id),
    byName: (project: string, name: string) => state.agents.find((a) => a.project === project && a.name === name),
    inProject: (project?: string | null) => state.agents.filter((a) => !project || a.project === project),
    dispose: () => unsubscribe?.(),
  };
}

export type AgentsStore = ReturnType<typeof createAgentsStore>;

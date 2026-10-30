// Quota holds from the event stream (docs/api.md §16.6, §16.7).
//
// `quota.blocked` holds an agent's new turns: after a usage-limit error of
// the provider (cause `limit:<subscription>`), or a spent daily budget.
// The agent badges show it. `quota.resumed` ends a hold. A `quota.alert` raises no
// banner: the operator gets one mail in the Inbox for each line
// (`alert_at`, 90, used up). The logic is pure (`applyQuotaEvent`,
// `holdsFor`); the store only feeds it events.

import { createSignal } from "solid-js";
import type { AgentEvent, QuotaHold } from "../api/types";
import type { EventsStore } from "./events";
import { eventAgentId } from "./agents";

export interface AgentHold {
  agent_id: string;
  agent?: string;
  project?: string;
  cause: string;
  until?: string;
  reason: string;
  enforced: boolean;
  at: string;
}

export interface QuotaState {
  holds: AgentHold[];
}

export const emptyQuota = (): QuotaState => ({ holds: [] });

const str = (v: unknown): string | undefined => (typeof v === "string" && v ? v : undefined);

/** `quota:<name>` for a plan; `budget:project:<p>[/<sub>]` for a budget. */
export function describeCause(cause: string): string {
  if (cause.startsWith("limit:")) return `a usage-limit error on ${cause.slice(6)}`;
  if (cause.startsWith("quota:")) return `subscription ${cause.slice(6)}`;
  if (cause.startsWith("budget:project:")) return `the daily budget of ${cause.slice(15)}`;
  if (cause.startsWith("budget:")) return `the daily budget of ${cause.slice(7)}`;
  return cause;
}

/** Apply one event. Pure: returns the same object when nothing changed. */
export function applyQuotaEvent(s: QuotaState, e: AgentEvent): QuotaState {
  const d = e.data ?? {};
  switch (e.kind) {
    case "quota.blocked": {
      const agent_id = eventAgentId(e) ?? e.agent;
      const cause = str(d.cause);
      if (!agent_id || !cause) return s;
      const hold: AgentHold = {
        agent_id,
        cause,
        reason: str(d.reason) ?? "",
        enforced: d.enforced === true,
        at: e.at,
        ...(e.agent ? { agent: e.agent } : {}),
        ...(e.project ? { project: e.project } : {}),
        ...(str(d.until) ? { until: str(d.until) } : {}),
      };
      return { ...s, holds: [...s.holds.filter((h) => !(h.agent_id === agent_id && h.cause === cause)), hold] };
    }
    case "quota.resumed": {
      const agent_id = eventAgentId(e) ?? e.agent;
      if (!agent_id) return s;
      const cause = str(d.cause);
      const holds = s.holds.filter((h) => h.agent_id !== agent_id || (cause !== undefined && h.cause !== cause && d.holds_left !== 0));
      return holds.length === s.holds.length ? s : { ...s, holds };
    }
    default:
      return s;
  }
}

/** The holds on one agent that still apply. The latest reset first. */
export function holdsFor(s: QuotaState, agentId: string, now = Date.now()): AgentHold[] {
  return s.holds
    .filter((h) => h.agent_id === agentId && (!h.until || Date.parse(h.until) > now))
    .sort((a, b) => (b.until ?? "").localeCompare(a.until ?? ""));
}

/** Seed the holds of one agent from `GET /api/usage/me` (holds older than the event backlog). */
export function withHolds(s: QuotaState, agentId: string, holds: QuotaHold[], agentName?: string): QuotaState {
  const mine: AgentHold[] = holds.map((h) => ({
    agent_id: agentId,
    cause: h.cause,
    reason: h.reason,
    enforced: h.enforced,
    at: h.created_at ?? new Date(0).toISOString(),
    ...(agentName ? { agent: agentName } : {}),
    ...(h.until ? { until: h.until } : {}),
  }));
  return { ...s, holds: [...s.holds.filter((h) => h.agent_id !== agentId), ...mine] };
}

export function createQuotaStore(events?: Pick<EventsStore, "on"> & Partial<Pick<EventsStore, "recent">>) {
  const [state, setState] = createSignal<QuotaState>(emptyQuota());
  // The backlog first, oldest first, then live events.
  const backlog = (events?.recent?.() ?? []).slice().reverse();
  if (backlog.length) setState((s) => backlog.reduce(applyQuotaEvent, s));
  const off = events?.on((e) => {
    if (!e.kind.startsWith("quota.")) return;
    setState((s) => applyQuotaEvent(s, e));
  });

  return {
    state,
    holdsFor: (agentId: string, now?: number) => holdsFor(state(), agentId, now),
    seedHolds: (agentId: string, holds: QuotaHold[], name?: string) => setState((s) => withHolds(s, agentId, holds, name)),
    dispose: () => off?.(),
  };
}

export type QuotaStore = ReturnType<typeof createQuotaStore>;

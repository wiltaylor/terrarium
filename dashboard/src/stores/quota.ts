// Quota alerts and holds from the event stream (docs/api.md §16.6, §16.7).
//
// `quota.alert` raises a banner. `quota.blocked` holds an agent's new turns
// until a reset; the agent badges show it. `quota.sample` clears an alert
// whose window reads under the line again, and `quota.resumed` ends a hold.
// The logic is pure (`applyQuotaEvent`, `activeAlerts`, `holdsFor`); the
// store only feeds it events.

import { createSignal } from "solid-js";
import type { AgentEvent, QuotaHold } from "../api/types";
import type { EventsStore } from "./events";
import { eventAgentId } from "./agents";

export interface QuotaAlert {
  /** One per window: `<subscription>/<window>`, or `budget:<project>/<subscription|*>/<window>`. */
  key: string;
  subscription?: string;
  /** Set for a project budget. */
  project?: string;
  window: string;
  used_pct: number;
  level: "alert" | "exhausted";
  resets_at?: string;
  alert_at?: number;
  at: string;
}

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
  alerts: QuotaAlert[];
  holds: AgentHold[];
  /** Dismissed alerts as `<key>/<level>/<resets_at>`: a new period or level shows again. */
  dismissed: string[];
}

export const emptyQuota = (): QuotaState => ({ alerts: [], holds: [], dismissed: [] });

const str = (v: unknown): string | undefined => (typeof v === "string" && v ? v : undefined);
const num = (v: unknown): number | undefined => (typeof v === "number" && Number.isFinite(v) ? v : undefined);

export const alertKey = (d: { subscription?: string; project?: string; window: string }): string =>
  d.project ? `budget:${d.project}/${d.subscription ?? "*"}/${d.window}` : `${d.subscription ?? "?"}/${d.window}`;

export const dismissKey = (a: QuotaAlert): string => `${a.key}/${a.level}/${a.resets_at ?? ""}`;

/** `quota:<name>` for a plan; `budget:project:<p>[/<sub>]` for a budget. */
export function describeCause(cause: string): string {
  if (cause.startsWith("quota:")) return `subscription ${cause.slice(6)}`;
  if (cause.startsWith("budget:project:")) return `the daily budget of ${cause.slice(15)}`;
  if (cause.startsWith("budget:")) return `the daily budget of ${cause.slice(7)}`;
  return cause;
}

/** Apply one event. Pure: returns the same object when nothing changed. */
export function applyQuotaEvent(s: QuotaState, e: AgentEvent): QuotaState {
  const d = e.data ?? {};
  switch (e.kind) {
    case "quota.alert": {
      const window = str(d.window) ?? "day";
      const level = d.level === "exhausted" ? "exhausted" : "alert";
      const alert: QuotaAlert = {
        key: alertKey({ subscription: str(d.subscription), project: str(d.project), window }),
        window,
        level,
        used_pct: num(d.used_pct) ?? (level === "exhausted" ? 100 : 0),
        at: e.at,
        ...(str(d.subscription) ? { subscription: str(d.subscription) } : {}),
        ...(str(d.project) ? { project: str(d.project) } : {}),
        ...(str(d.resets_at) ? { resets_at: str(d.resets_at) } : {}),
        ...(num(d.alert_at) !== undefined ? { alert_at: num(d.alert_at) } : {}),
      };
      return { ...s, alerts: [...s.alerts.filter((a) => a.key !== alert.key), alert] };
    }
    case "quota.sample": {
      const sub = str(d.subscription);
      if (!sub || !Array.isArray(d.windows)) return s;
      let changed = false;
      const alerts: QuotaAlert[] = [];
      for (const a of s.alerts) {
        const w = a.project ? undefined : (d.windows as Record<string, unknown>[]).find((x) => a.subscription === sub && x.window === a.window);
        const pct = w ? num(w.used_pct) : undefined;
        if (pct === undefined) {
          alerts.push(a);
          continue;
        }
        changed = true;
        const line = a.alert_at ?? 100;
        if (pct < line) continue; // under the line again: the alert ends
        alerts.push({ ...a, used_pct: pct, level: pct >= 100 ? "exhausted" : a.level === "exhausted" ? "alert" : a.level, resets_at: str(w!.resets_at) ?? a.resets_at, at: e.at });
      }
      return changed ? { ...s, alerts } : s;
    }
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

/** Alerts to show: not dismissed, and their reset is still ahead. Used-up plans first. */
export function activeAlerts(s: QuotaState, now = Date.now()): QuotaAlert[] {
  return s.alerts
    .filter((a) => !s.dismissed.includes(dismissKey(a)) && (!a.resets_at || Date.parse(a.resets_at) > now))
    .sort((a, b) => (a.level === b.level ? b.used_pct - a.used_pct || a.key.localeCompare(b.key) : a.level === "exhausted" ? -1 : 1));
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

/** One line for the banner. */
export function alertText(a: QuotaAlert): string {
  const what = a.project ? `The daily budget of ${a.project}${a.subscription ? ` on ${a.subscription}` : ""}` : `${a.subscription} (${a.window} window)`;
  if (a.level === "exhausted") return `${what} is used up. New turns of its agents wait for the reset.`;
  return `${what} is at ${Math.round(a.used_pct)}%${a.alert_at !== undefined ? `, over the alert line of ${a.alert_at}%` : ""}.`;
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
    alerts: (now?: number) => activeAlerts(state(), now),
    holdsFor: (agentId: string, now?: number) => holdsFor(state(), agentId, now),
    seedHolds: (agentId: string, holds: QuotaHold[], name?: string) => setState((s) => withHolds(s, agentId, holds, name)),
    dismiss: (a: QuotaAlert) => setState((s) => ({ ...s, dismissed: [...s.dismissed, dismissKey(a)] })),
    dispose: () => off?.(),
  };
}

export type QuotaStore = ReturnType<typeof createQuotaStore>;

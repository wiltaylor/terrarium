// Target leases (docs/api.md §17.7): pure helpers for the countdown, the
// `until` input, the `409 lease_held` refusal, and the `lease` event.
// No Solid imports.

import { ApiError } from "../api/http";
import type { AgentEvent, Lease, LeaseEventData, LeaseHeldDetails, LeaseState } from "../api/types";

const pad = (n: number) => String(n).padStart(2, "0");

/**
 * The time left on a lease, for a live countdown: `42s`, `12m 05s`,
 * `2h 05m`, `3d 4h`. `null` is `until revoked`. A time that passed is
 * `expired`.
 */
export function countdown(expiresAt: string | null | undefined, now = Date.now()): string {
  if (!expiresAt) return "until revoked";
  const ms = Date.parse(expiresAt) - now;
  if (Number.isNaN(ms)) return "—";
  if (ms <= 0) return "expired";
  const s = Math.ceil(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${pad(s % 60)}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${pad(m % 60)}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** The lease is active now: the daemon's state is `active` and `expires_at` is later than now. */
export const leaseActive = (l: Pick<Lease, "state" | "expires_at">, now = Date.now()): boolean =>
  l.state === "active" && (!l.expires_at || Date.parse(l.expires_at) > now);

/** The state to show: an active lease past its time is `expired` before the daemon's timer says so. */
export const shownState = (l: Pick<Lease, "state" | "expires_at">, now = Date.now()): LeaseState =>
  l.state === "active" && !leaseActive(l, now) ? "expired" : l.state;

/** `project/name` of the holder, else its ULID (a deleted agent). */
export const holderLabel = (l: Pick<Lease, "agent" | "agent_name">): string => l.agent_name ?? l.agent;

/** `operator`, `system`, or `agent:<ulid>` as a readable name. `name` looks up an agent ULID. */
export function actorLabel(actor: string | null | undefined, name?: (id: string) => string | undefined): string {
  if (!actor) return "—";
  if (!actor.startsWith("agent:")) return actor;
  const id = actor.slice(6);
  return name?.(id) ?? id;
}

export type UntilMode = "duration" | "time" | "none";

/** The duration form that the daemon takes: `90s`, `30m`, `2h`, `1d`. */
export const DURATION_RE = /^\d+\s*[smhd]$/;

/**
 * The `until` of a grant or an extend, from the form. A duration goes as it
 * is; a `datetime-local` value goes as an RFC 3339 time. `none` is `null`:
 * until a revoke.
 */
export function untilValue(mode: UntilMode, value: string, now = Date.now()): { until: string | null } | { error: string } {
  if (mode === "none") return { until: null };
  const v = value.trim();
  if (mode === "duration") {
    if (!DURATION_RE.test(v)) return { error: "Give a duration such as 90s, 30m, 2h, or 1d." };
    if (/^0+\s*[smhd]$/.test(v)) return { error: "The duration must be more than zero." };
    return { until: v.replace(/\s+/g, "") };
  }
  const at = Date.parse(v);
  if (!v || Number.isNaN(at)) return { error: "Give a date and a time." };
  if (at <= now) return { error: "The time must be in the future." };
  return { until: new Date(at).toISOString() };
}

/** A `409 lease_held` refusal, with the holder from `details`. Null for any other error. */
export function leaseHeld(e: unknown): (LeaseHeldDetails & { message: string }) | null {
  if (!(e instanceof ApiError) || e.code !== "lease_held") return null;
  const d = (e.details ?? {}) as Partial<LeaseHeldDetails>;
  return {
    message: e.message,
    target: d.target ?? "",
    holder: d.holder ?? d.holder_id ?? "another agent",
    holder_id: d.holder_id ?? "",
    lease_id: d.lease_id ?? "",
    exclusive: d.exclusive ?? false,
    expires_at: d.expires_at ?? null,
  };
}

/** The `data` of a `lease` event, or null for another kind or a bad payload. */
export function leaseEventData(e: AgentEvent): LeaseEventData | null {
  if (e.kind !== "lease") return null;
  const d = e.data as Partial<LeaseEventData> | undefined;
  if (!d || typeof d.target !== "string" || typeof d.lease_id !== "string") return null;
  if (d.change !== "granted" && d.change !== "extended" && d.change !== "ended") return null;
  return {
    change: d.change,
    target: d.target,
    lease_id: d.lease_id,
    expires_at: d.expires_at ?? null,
    exclusive: !!d.exclusive,
    state: d.state ?? (d.change === "ended" ? "revoked" : "active"),
  };
}

/** The holder ULID of a `lease` event: `agent_id`, or `data.agent_id`. */
export const leaseEventAgent = (e: AgentEvent): string | undefined => e.agent_id ?? (typeof e.data?.agent_id === "string" ? e.data.agent_id : undefined);

/**
 * Apply a `lease` event to a lease list (one target's or one agent's).
 * `keepEnded`: the list shows the history too, so an ended lease stays with
 * its new state; else it leaves the list. A lease that the list does not
 * have asks for a refetch: the event does not carry `granted_by` or `reason`.
 * `match` says if the event is about this list. Pure.
 */
export function applyLeaseEvent(
  list: Lease[],
  e: AgentEvent,
  opts: { match: (d: LeaseEventData, agentId: string | undefined) => boolean; keepEnded?: boolean },
): { list: Lease[]; refetch: boolean } {
  const d = leaseEventData(e);
  if (!d || !opts.match(d, leaseEventAgent(e))) return { list, refetch: false };
  const i = list.findIndex((l) => l.id === d.lease_id);
  if (i < 0) return { list, refetch: d.change !== "ended" || !!opts.keepEnded };
  const old = list[i]!;
  if (d.change === "ended" && !opts.keepEnded) return { list: list.filter((_, j) => j !== i), refetch: false };
  const next = list.slice();
  next[i] = {
    ...old,
    expires_at: d.expires_at,
    exclusive: d.exclusive,
    state: d.state,
    ...(d.change === "ended" && !old.revoked_at ? { revoked_at: e.at, revoked_by: e.actor ?? null } : {}),
  };
  return { list: next, refetch: false };
}

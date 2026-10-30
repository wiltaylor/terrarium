// The lifecycle of an agent as the dashboard shows it: the badge (status,
// `paused`, or `stopped (idle)`), the queued count, and the lifecycle
// buttons of the agent page. Pure.
//
// The daemon wakes a stopped agent when work comes (a turn, mail, a card).
// It stops an agent that stays idle for its idle timeout (`status_reason`
// `idle`). A paused agent does not run, and nothing wakes it.

import type { Agent } from "../api/types";

type Lifecycle = Pick<Agent, "status" | "paused" | "status_reason" | "waiting_on" | "queued">;

export interface LifecycleBadge {
  /** The badge text. */
  label: string;
  /** The CSS classes after `badge`: `st-<status>`, or `st-paused`. */
  cls: string;
  /** The tooltip and the accessible text. */
  title: string;
}

/** True when the daemon stopped the agent after its idle timeout. */
export const idleStopped = (a: Pick<Agent, "status" | "status_reason">): boolean => a.status === "stopped" && a.status_reason === "idle";

export function lifecycleBadge(a: Lifecycle): LifecycleBadge {
  if (a.paused) return { label: "paused", cls: "st-paused", title: "Paused: nothing wakes the agent. Turns and mail stay queued until you resume it." };
  if (idleStopped(a)) return { label: "stopped (idle)", cls: "st-stopped", title: "Stopped after the idle timeout. A turn, mail, or a card wakes it." };
  if (a.status === "stopped") return { label: "stopped", cls: "st-stopped", title: "Status: stopped. A turn, mail, or a card wakes it." };
  if (a.status === "waiting" && a.waiting_on) return { label: a.status, cls: `st-${a.status}`, title: `Status: waiting on ${a.waiting_on}` };
  return { label: a.status, cls: `st-${a.status}`, title: `Status: ${a.status}` };
}

/** "3 queued", or null when nothing waits for the agent. */
export const queuedLabel = (a: Pick<Agent, "queued">): string | null => (a.queued && a.queued > 0 ? `${a.queued} queued` : null);

/** One lifecycle button of the agent page. */
export interface LifecycleAction {
  kind: "pause" | "resume" | "restart";
  label: string;
  disabled: boolean;
  title: string;
}

/**
 * The lifecycle buttons of the agent page. There is no Start and no Stop:
 * work wakes the agent, and the idle timeout stops it. Pause blocks the
 * wakes; Resume clears the pause. Restart is off while the agent is paused.
 */
export function lifecycleActions(a: Pick<Agent, "paused">): LifecycleAction[] {
  const toggle: LifecycleAction = a.paused
    ? { kind: "resume", label: "Resume", disabled: false, title: "Clear the pause. The agent wakes at once when work waits for it." }
    : { kind: "pause", label: "Pause", disabled: false, title: "Stop the agent if it runs, and do not let work wake it. Turns and mail stay queued." };
  const restart: LifecycleAction = a.paused
    ? { kind: "restart", label: "Restart", disabled: true, title: "The agent is paused. Resume it first." }
    : { kind: "restart", label: "Restart", disabled: false, title: "Stop the agent and start it again with a resume." };
  return [toggle, restart];
}

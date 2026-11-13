// The lifecycle of an agent as the dashboard shows it: the badge (status,
// `paused`, or `stopped (idle)`), the queued count, and the lifecycle
// buttons of the agent page (also Compact and New session). Pure.
//
// The daemon wakes a stopped agent when work comes (a turn, mail, a card).
// It stops an agent that stays idle for its idle timeout (`status_reason`
// `idle`). A paused agent does not run, and nothing wakes it. The daemon
// can compact the session of an agent at rest (`status_reason`
// `compacting` while it runs), or start a new, clean session. A request on
// a busy agent waits (`reset_pending`) until the current turn ends.

import type { Agent, ResetOutcome } from "../api/types";

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

/** True while the work of the agent waits for a free stream of its self-hosted subscription (`stream_limit: ...`). */
export const streamWaiting = (a: Pick<Agent, "status" | "status_reason">): boolean => a.status !== "stopped" && (a.status_reason ?? "").startsWith("stream_limit");

export function lifecycleBadge(a: Lifecycle): LifecycleBadge {
  if (a.paused) return { label: "paused", cls: "st-paused", title: "Paused: nothing wakes the agent. Turns and mail stay queued until you resume it." };
  if (a.status === "stopped" && a.status_reason === "subscription_disabled")
    return { label: "stopped (subscription disabled)", cls: "st-stopped", title: "Stopped. Its subscription is disabled: nothing wakes it until the subscription is enabled. Turns and mail stay queued." };
  if (streamWaiting(a)) return { label: "waiting for a free stream", cls: `st-${a.status}`, title: `Its work waits for a free stream of its self-hosted subscription (${a.status_reason}).` };
  if (idleStopped(a)) return { label: "stopped (idle)", cls: "st-stopped", title: "Stopped after the idle timeout. A turn, mail, or a card wakes it." };
  if (a.status === "stopped") return { label: "stopped", cls: "st-stopped", title: "Status: stopped. A turn, mail, or a card wakes it." };
  if (a.status === "waiting" && a.waiting_on) return { label: a.status, cls: `st-${a.status}`, title: `Status: waiting on ${a.waiting_on}` };
  return { label: a.status, cls: `st-${a.status}`, title: `Status: ${a.status}` };
}

/** "3 queued", or null when nothing waits for the agent. */
export const queuedLabel = (a: Pick<Agent, "queued">): string | null => (a.queued && a.queued > 0 ? `${a.queued} queued` : null);

/** "2 held", or null when no held mail waits for a release or the caller may not see it. Apart from {@link queuedLabel}. */
export const heldLabel = (a: Pick<Agent, "held_mail">): string | null => (a.held_mail && a.held_mail > 0 ? `${a.held_mail} held` : null);

/** The held mail line of the agent page: the count, or `unavailable` when the daemon left the field out for the caller. */
export const heldText = (a: Pick<Agent, "held_mail">): string => (a.held_mail === undefined ? "unavailable" : String(a.held_mail));

/** The tooltip of the held chip. */
export const HELD_TITLE = "Mail that the loop guard held. No pointer went to the agent, and it is not in the queued count. The operator resumes the thread in the Inbox.";

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

/** The Compact or the New session button of the agent page: off, with the reason, when the daemon would refuse it. */
export interface ResetAction {
  disabled: boolean;
  title: string;
}

/** The agent runs a turn, starts, or compacts: a reset waits until the turn ends. */
const busy = (a: Pick<Agent, "status">): boolean => a.status === "working" || a.status === "blocked" || a.status === "starting" || a.status === "restarting";

/**
 * The Compact button (`POST .../compact`). The daemon compacts only an agent
 * that runs, is not paused, has no quota or budget hold, and has a harness
 * that can compact. `held` is true while a hold lasts. An agent at rest
 * compacts now; a busy agent compacts when its turn ends. Each compaction is
 * one model call.
 */
export function compactButton(a: Pick<Agent, "harness" | "status" | "paused" | "status_reason" | "compact_supported">, held = false): ResetAction {
  if (a.compact_supported === false) return { disabled: true, title: `auto compact not supported for ${a.harness}` };
  if (a.paused) return { disabled: true, title: "The agent is paused. Resume it first." };
  if (held) return { disabled: true, title: "A quota or budget hold lasts. The agent does not compact until it ends." };
  if (a.status === "stopped" || a.status === "error") return { disabled: true, title: `The agent is ${a.status}. It compacts only while it runs. Use New session to start clean.` };
  if (a.status === "working" && a.status_reason === "compacting") return { disabled: false, title: "The agent compacts its session now. A new request waits until this compaction ends." };
  if (busy(a)) return { disabled: false, title: `The agent is ${a.status}. The compaction waits in a queue until the current turn ends.` };
  return { disabled: false, title: "Compact the session now: the harness summarises the conversation (one model call). New turns wait until it ends." };
}

/**
 * The New session button (`POST .../new-session`): the daemon ends the
 * session and starts a new, clean harness session. It is always on; the title
 * tells when the new session starts.
 */
export function newSessionButton(a: Pick<Agent, "status" | "paused">): ResetAction {
  if (a.paused) return { disabled: false, title: "The agent is paused. The new session waits in a queue until you resume the agent." };
  if (a.status === "stopped" || a.status === "error")
    return { disabled: false, title: `The agent is ${a.status}. With a "then" prompt, it starts at once in a new session. Without one, its next wake starts a new session.` };
  if (busy(a)) return { disabled: false, title: `The agent is ${a.status}. The new session waits in a queue until the current turn ends.` };
  return { disabled: false, title: "End the session and start a new, clean session now. The agent forgets the conversation: a recap and a \"then\" prompt can follow." };
}

/** The chip of a reset that waits: the text and the tooltip. */
export interface ResetChip {
  label: string;
  title: string;
}

/** The "compaction queued" or "new session queued" chip of the agent page; null when no reset waits. */
export function resetChip(a: Pick<Agent, "status" | "reset_pending">, ago?: (iso: string) => string): ResetChip | null {
  const p = a.reset_pending;
  if (!p) return null;
  const nextWake = p.kind === "new_session" && (a.status === "stopped" || a.status === "error");
  const label = p.kind === "compact" ? "compaction queued" : nextWake ? "new session at next wake" : "new session queued";
  const what = p.kind === "compact" ? "A compaction" : "A new session";
  const when = nextWake ? "It starts at the next wake of the agent." : "It runs when the current turn ends.";
  const parts = [
    `${what} waits. Requested by ${p.by}${ago ? ` ${ago(p.requested_at)}` : ""}.`,
    when,
    p.then_set ? 'A "then" prompt follows it.' : 'No "then" prompt.',
    ...(p.kind === "new_session" ? [p.recap ? "A recap turn goes first." : "No recap turn."] : []),
    "A new request replaces it.",
  ];
  return { label, title: parts.join(" ") };
}

/** The notice after a reset request: when the reset runs, and whether it replaced a queued request. */
export function resetNotice(r: ResetOutcome, paused = false): string {
  const what = r.kind === "compact" ? "Compaction" : "New session";
  const then = r.then_set ? ' The "then" prompt goes as the next turn.' : "";
  const main =
    r.state === "started"
      ? `${what} started.`
      : r.state === "next_wake"
        ? `${what} at the next wake: the agent stays stopped until work wakes it.`
        : paused
          ? `${what} queued: it runs after you resume the agent.`
          : `${what} queued: it runs when the current turn ends.`;
  return `${main}${then}${r.replaced ? " It replaced the queued request." : ""}`;
}

/** "last compacted 5 min ago", with "(failed)" for a compaction that did not end well; null when none. */
export function lastCompactedText(a: Pick<Agent, "last_compact_at" | "last_compact_ok">, ago: (iso: string) => string): string | null {
  if (!a.last_compact_at) return null;
  return `last compacted ${ago(a.last_compact_at)}${a.last_compact_ok === false ? " (failed)" : ""}`;
}

/** The most bytes of a "then" prompt (32 KiB). */
export const RESET_THEN_MAX = 32 * 1024;

/** The problem of a "then" prompt, or null. A blank prompt is no prompt. */
export function resetThenProblem(then: string): string | null {
  const bytes = new TextEncoder().encode(then).length;
  return bytes > RESET_THEN_MAX ? `The "then" prompt has ${bytes} bytes; it has at most ${RESET_THEN_MAX}.` : null;
}

/** The body of a reset request: a blank "then" prompt is left out; `recap` goes only for a new session. */
export function resetBody(kind: ResetOutcome["kind"], then: string, recap: boolean): { then?: string; recap?: boolean } {
  const body: { then?: string; recap?: boolean } = then.trim() ? { then } : {};
  if (kind === "new_session") body.recap = recap;
  return body;
}

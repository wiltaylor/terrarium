// Waits and watches on the agent page: pure helpers. No Solid imports.

import type { AgentEvent, NewWatch, ProcessWaitResult, ProcessWaitSpec, Wait, Watch, WatchOn } from "../../api/types";
import { WATCH_ON } from "../../api/types";

/** Is this the result of a process run? */
export const isProcessResult = (r: Wait["result"]): r is ProcessWaitResult =>
  !!r && typeof r === "object" && ("exit_code" in r || "signal" in r) && "reason" in r;

const secs = (n: number | undefined) => (typeof n === "number" && Number.isFinite(n) ? ` · ${n < 10 ? n.toFixed(1) : Math.round(n)}s` : "");

/** A short line for the result of a closed wait: `exit 3 · 12s`, `killed (SIGKILL)`, or the outcome. */
export function waitResultText(w: Pick<Wait, "result" | "state">): string {
  const r = w.result;
  if (isProcessResult(r)) {
    if (r.signal) return `${r.reason === "exited" ? "signal" : r.reason} (${r.signal})${secs(r.duration_secs)}`;
    if (r.exit_code !== null && r.exit_code !== undefined) return `exit ${r.exit_code}${secs(r.duration_secs)}`;
    return `${r.reason}${secs(r.duration_secs)}`;
  }
  if (r && typeof r === "object" && typeof r.outcome === "string" && r.outcome) return r.outcome;
  return "—";
}

/** The chip class of a wait state. */
export const waitStateChip = (s: Wait["state"]): string => ({ open: "info", resolved: "ok", timed_out: "warn", cancelled: "", failed: "bad" })[s] ?? "";

/** The command of a process wait, else null. */
export const waitCommand = (w: Pick<Wait, "kind" | "spec">): string | null =>
  w.kind === "process" && typeof (w.spec as ProcessWaitSpec).command === "string" ? (w.spec as ProcessWaitSpec).command! : null;

/** Open waits first (newest first), then the closed ones (newest first). */
export function splitWaits(list: Wait[]): { open: Wait[]; closed: Wait[] } {
  const at = (w: Wait) => w.resolved_at ?? w.created_at;
  return {
    open: list.filter((w) => w.state === "open").sort((x, y) => y.created_at.localeCompare(x.created_at)),
    closed: list.filter((w) => w.state !== "open").sort((x, y) => at(y).localeCompare(at(x))),
  };
}

/** Put the daemon's answer for one wait into the list. */
export function putWait(list: Wait[], w: Wait): Wait[] {
  const i = list.findIndex((x) => x.id === w.id);
  if (i < 0) return [w, ...list];
  const next = list.slice();
  next[i] = w;
  return next;
}

/** Is a `wait` event about the waits of this agent? */
export const isWaitEventFor = (e: AgentEvent, agentId: string): boolean =>
  e.kind === "wait" && (e.agent_id === agentId || e.data?.agent_id === agentId);

/** `on`: the watch is on this agent. `by`: this agent watches another one. */
export const watchRole = (w: Pick<Watch, "agent" | "watcher">, agentId: string): "on" | "by" => (w.agent === agentId ? "on" : "by");

/** Turn one status of the watch form on or off. The order stays that of `WATCH_ON`. */
export function toggleOn(list: WatchOn[], s: WatchOn, checked: boolean): WatchOn[] {
  const set = new Set(list);
  if (checked) set.add(s);
  else set.delete(s);
  return WATCH_ON.filter((x) => set.has(x));
}

/** The body of the operator's watch, or the reason the form cannot send. */
export function watchBody(on: WatchOn[], once: boolean): { body: NewWatch } | { error: string } {
  if (!on.length) return { error: "Pick one status or more." };
  return { body: { on: [...on], once, debounce: true } };
}

/** A short text of a watch: `idle, blocked · once · fired 2×`. */
export function watchText(w: Pick<Watch, "on" | "once" | "fired">): string {
  return [w.on.join(", "), w.once ? "once" : null, w.fired ? `fired ${w.fired}×` : null].filter(Boolean).join(" · ");
}

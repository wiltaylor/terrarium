// The one state chip of a card tile (TER-158, decision D4 of TER-155): from
// live data only (the card's open job and the assignee agent's status), never
// from the `validation:*` labels. Pure functions, no Solid imports.

import type { Card } from "../api/types";

export type CardStateKind = "working" | "waiting" | "idle" | "queued" | "running" | "awaiting_review" | "merge_queued" | "agent";

export interface CardState {
  kind: CardStateKind;
  /** The chip text: `queued 2 of 5`, `awaiting review`. */
  label: string;
  /** The chip tone class: `info`, `ok`, `warn`, `bad`, or "" for a plain chip. */
  tone: string;
  /** The tooltip. */
  title: string;
}

/** The columns where a card has no state chip: its work is over. */
const OVER = new Set(["done", "accepted"]);
/** The column of a card that waits on a reviewer (the job queue moves a validated card there). */
export const REVIEW = "review";

/** A label that the tile hides: the job state comes from `Card.job`. */
export const isHiddenTileLabel = (l: string): boolean => l.startsWith("validation:");

/** The labels that the tile shows. */
export const tileLabels = (labels: readonly string[]): string[] => labels.filter((l) => !isHiddenTileLabel(l));

/**
 * The state chip of a card, in this order: the open job (running, merge queued,
 * queued n of m), the review column (awaiting review), then the assignee
 * agent's status (working, waiting, idle). `agentStatus` is the live status from
 * the agent store; without it, the card's `assignee_status`. Another agent
 * status (blocked, stopped, error, ...) shows as itself. `null`: no chip (a
 * done, accepted, or archived card, or a card with no job and no agent).
 */
export function cardState(c: Pick<Card, "column" | "archived" | "job" | "assignee" | "assignee_status">, agentStatus?: string): CardState | null {
  if (c.archived || OVER.has(c.column)) return null;
  const j = c.job;
  if (j?.state === "running") {
    const what = j.kind === "merge" ? "The merge job" : "The validation job";
    return { kind: "running", label: "running", tone: "info", title: `${what} runs now.` };
  }
  if (j?.state === "queued") {
    if (j.kind === "merge") return { kind: "merge_queued", label: "merge queued", tone: "ok", title: queuedTitle("The merge job", j.place, j.queued) };
    const label = j.place && j.queued ? `queued ${j.place} of ${j.queued}` : "queued";
    return { kind: "queued", label, tone: "info", title: queuedTitle("The validation job", j.place, j.queued) };
  }
  if (c.column === REVIEW) return { kind: "awaiting_review", label: "awaiting review", tone: "ok", title: "The card waits on a reviewer." };
  if (!c.assignee || c.assignee === "operator") return null;
  const status = agentStatus ?? c.assignee_status;
  switch (status) {
    case undefined:
    case "":
      return null;
    case "working":
      return { kind: "working", label: "working", tone: "info", title: "The assignee works on it now." };
    case "waiting":
      return { kind: "waiting", label: "waiting", tone: "", title: "The assignee waits on a run, a peer, or a time." };
    case "idle":
      return { kind: "idle", label: "idle", tone: "warn", title: "The assignee is idle." };
    default:
      return { kind: "agent", label: status, tone: status === "blocked" || status === "error" ? "bad" : "warn", title: `The assignee is ${status}.` };
  }
}

function queuedTitle(what: string, place?: number, queued?: number): string {
  if (!place || !queued) return `${what} waits in the project's queue.`;
  return place === 1 ? `${what} starts next (${queued} queued in the project).` : `${what} is number ${place} of ${queued} queued in the project.`;
}

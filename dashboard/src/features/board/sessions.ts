// The sessions behind a card (docs/api.md §6.6, §8): the "View session" link
// of an agent comment, and the rows of the card's Sessions section. A session
// stays after `agent rm`; its row then shows a "removed" chip.

import type { Card, Session, SessionRef } from "../../api/types";

/** The read-only session page of a session. */
export const sessionHref = (id: string): string => `/sessions/${encodeURIComponent(id)}`;

export interface SessionChip {
  label: string;
  tone: "" | "ok" | "warn";
}

/** The chips of a session row: "removed" for a deleted agent, "open" for a session that has not ended. */
export function sessionChips(s: SessionRef): SessionChip[] {
  const chips: SessionChip[] = [];
  if (s.removed) chips.push({ label: "removed", tone: "warn" });
  if (!s.ended_at) chips.push({ label: "open", tone: "ok" });
  return chips;
}

/** The time range of a session, with `fmt` for one time. An open session ends with "now". */
export function sessionRange(s: SessionRef, fmt: (iso: string) => string = (iso) => new Date(iso).toLocaleString()): string {
  return `${fmt(s.started_at)} – ${s.ended_at ? fmt(s.ended_at) : "now"}`;
}

export interface CardSessionRow {
  session: SessionRef;
  href: string;
  /** `project/agent`. */
  agent: string;
  /** The comments on the card from this session. */
  comments: number;
  chips: SessionChip[];
}

/** The rows of the card's Sessions section: the card's sessions, with the comment count of each, in the order given. */
export function cardSessionRows(sessions: SessionRef[], card: Pick<Card, "comments">): CardSessionRow[] {
  const counts = new Map<string, number>();
  for (const c of card.comments) {
    if (c.session) counts.set(c.session.id, (counts.get(c.session.id) ?? 0) + 1);
  }
  return sessions.map((s) => ({
    session: s,
    href: sessionHref(s.id),
    agent: `${s.project}/${s.agent}`,
    comments: counts.get(s.id) ?? 0,
    chips: sessionChips(s),
  }));
}

/** The sessions of the comments alone, oldest first: the fallback when `GET /api/cards/{id}/sessions` fails (an older daemon). */
export function commentSessions(card: Pick<Card, "comments">): SessionRef[] {
  const seen = new Map<string, SessionRef>();
  for (const c of card.comments) if (c.session && !seen.has(c.session.id)) seen.set(c.session.id, c.session);
  return [...seen.values()].sort((a, b) => a.started_at.localeCompare(b.started_at) || a.id.localeCompare(b.id));
}

/** A `Session` for the transcript view, from a session link. */
export function asSession(s: SessionRef & { active?: boolean }): Session {
  return {
    id: s.id,
    agent: s.agent_id,
    started_at: s.started_at,
    ...(s.ended_at ? { ended_at: s.ended_at } : {}),
    turns: 0,
    current: !s.ended_at,
    ...(s.active !== undefined ? { active: s.active } : {}),
  };
}

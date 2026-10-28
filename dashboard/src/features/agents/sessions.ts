// The agent page: its tabs in the URL, the current session of the
// Conversation tab, and the rows of the Sessions tab (docs/api.md §6.3).
// Pure; no Solid imports.

import type { Session } from "../../api/types";

export const AGENT_TABS = [
  { id: "conversation", label: "Conversation" },
  { id: "terminal", label: "Terminal" },
  { id: "screen", label: "Screen" },
  { id: "events", label: "Events" },
  { id: "sessions", label: "Sessions" },
] as const;
export type AgentTab = (typeof AGENT_TABS)[number]["id"];

/** The view of the agent page, from `?tab=&session=`. A session opens only on the Sessions tab. */
export interface AgentView {
  tab: AgentTab;
  /** The session open in the read-only viewer of the Sessions tab. */
  session: string | null;
}

export function parseAgentView(q: { tab?: string; session?: string }): AgentView {
  const tab = (AGENT_TABS.find((t) => t.id === q.tab)?.id ?? "conversation") as AgentTab;
  const session = tab === "sessions" && q.session ? q.session : null;
  return { tab, session };
}

/** The query of a view: the defaults are left out, so the plain URL is the Conversation tab. */
export function agentViewQuery(v: AgentView): { tab: string | undefined; session: string | undefined } {
  return {
    tab: v.tab === "conversation" ? undefined : v.tab,
    session: v.tab === "sessions" && v.session ? v.session : undefined,
  };
}

/**
 * The session of the Conversation tab, from the newest rows (newest first):
 * the agent's current session, else its newest one. `undefined`: the agent
 * has no session yet.
 */
export const currentSession = (rows: Session[]): Session | undefined => rows.find((s) => s.current) ?? rows[0];

/**
 * One row of the Sessions tab. A restart or a daemon restart resumes the
 * same harness session in a new session row, and each of those rows shows
 * the same session file (docs/api.md §6.3). So the rows of one harness
 * session in a row (newest first) are one conversation here.
 */
export interface SessionGroup {
  /** The newest row: the viewer opens it. Its transcript is the whole file. */
  session: Session;
  /** The rows, newest first. */
  rows: Session[];
  /** The start of the oldest row. */
  started_at: string;
  /** The turns of all the rows. */
  turns: number;
  title: string | null;
  current: boolean;
  active: boolean;
}

export function groupSessions(rows: Session[]): SessionGroup[] {
  const out: SessionGroup[] = [];
  for (const s of rows) {
    const last = out.at(-1);
    const hsid = s.harness_session_id;
    if (last && hsid && last.session.harness_session_id === hsid) {
      last.rows.push(s);
      last.started_at = s.started_at;
      last.turns += s.turns;
      last.title ??= s.title?.trim() || null;
      last.current ||= s.current;
      last.active ||= !!s.active;
      continue;
    }
    out.push({
      session: s,
      rows: [s],
      started_at: s.started_at,
      turns: s.turns,
      title: s.title?.trim() || null,
      current: s.current,
      active: !!s.active,
    });
  }
  return out;
}

/** The group that holds the session row or harness session `id`. */
export const findGroup = (groups: SessionGroup[], id: string): SessionGroup | undefined =>
  groups.find((g) => g.rows.some((s) => s.id === id || s.harness_session_id === id));

/** The label of a session: its title, else the harness session ID, else the row ID. */
export const sessionLabel = (s: Pick<Session, "id" | "title" | "harness_session_id">): string => s.title?.trim() || s.harness_session_id || s.id;

/** The state chip of a group. */
export function groupChip(g: SessionGroup): { label: string; tone: string } | null {
  if (g.active) return { label: "active", tone: "ok" };
  if (g.current) return { label: "current", tone: "info" };
  const outcome = g.session.outcome;
  return outcome ? { label: outcome, tone: "" } : null;
}

// The session link of a log row (docs/api.md §4, §7, §9): an event, a
// message, or an audit row of an agent links to the session that was active
// when the row was written. The daemon sends `session_id` only when the
// reader may read the session.

import type { LogSessionLink } from "../api/types";

export interface SessionLinkView {
  href: string;
  /** The short text of the link. */
  text: string;
  /** The full label, for the tooltip. */
  title: string;
}

/** The read-only session page of a session. */
export const sessionPage = (id: string): string => `/sessions/${encodeURIComponent(id)}`;

/** The link of a row, or null when the row has no session. */
export function sessionLinkOf(row: LogSessionLink): SessionLinkView | null {
  if (!row.session_id) return null;
  const label = row.session_label?.trim() || `session ${row.session_id}`;
  return { href: sessionPage(row.session_id), text: "session", title: `Open the session: ${label}` };
}

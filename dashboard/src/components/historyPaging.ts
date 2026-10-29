// The pure parts of EventHistory: the page query and the place of a live
// event. No Solid imports.

import type { AgentEvent, EventQuery } from "../api/types";

export const PAGE_LIMIT = 100;

/**
 * The query of one page. `since` is the lower bound in both orders: oldest
 * first reads up from it, newest first reads down to it (docs/api.md §9.1).
 */
export function historyQuery(since: string, newestFirst: boolean, cursor?: string): EventQuery {
  const q: EventQuery = { since: since.trim() || 0, cursor, limit: PAGE_LIMIT };
  return newestFirst ? { ...q, order: "desc" } : q;
}

/**
 * Where a live event goes in the list, or null when it does not go in.
 * Newest first, the list starts at the newest event: a newer event goes on
 * top. Oldest first, a newer event goes at the end, but only when no page
 * is left to load (that page has it).
 */
export function livePlace(items: AgentEvent[], e: AgentEvent, newestFirst: boolean, hasMore: boolean): "start" | "end" | null {
  if (newestFirst) {
    const top = items[0];
    return !top || e.seq > top.seq ? "start" : null;
  }
  if (hasMore) return null;
  const last = items.at(-1);
  return !last || e.seq > last.seq ? "end" : null;
}

// The event log after `?since=`, page by page (docs/api.md §9.1). Newest
// first, the list starts at the newest event, Load more adds older events
// at the bottom, and live frames go on top. Oldest first, the list starts
// at `since`, and live frames go at the end once the history has caught up.

import { createSignal, onCleanup, Show } from "solid-js";
import type { Page } from "../api/http";
import type { AgentEvent, EventQuery } from "../api/types";
import { events } from "../stores";
import { EventList } from "./EventList";
import { historyQuery, livePlace } from "./historyPaging";
import { createPagedList } from "./paged";
import { ErrorBanner, LoadMore } from "./ui";

export const RECENT_WINDOW = 300;

export function EventHistory(props: {
  /** One page of history. */
  fetch: (q: EventQuery) => Promise<Page<AgentEvent>>;
  /** A string that changes when the filters change; the list reloads. */
  filterKey: string;
  /** Which live events belong in this list. */
  match: (e: AgentEvent) => boolean;
  showAgent?: boolean;
}) {
  // Default: the last few hundred events of the log. `since` is the lower
  // bound in both orders, so the two orders show the same events.
  const recentSince = () => Math.max(0, events.status().lastSeq - RECENT_WINDOW);
  const [since, setSince] = createSignal<string>(String(recentSince()));
  const [follow, setFollow] = createSignal(true);
  const [newestFirst, setNewestFirst] = createSignal(true);

  const list = createPagedList(
    () => `${props.filterKey}|${since()}|${newestFirst()}`,
    (_k, cursor) => props.fetch(historyQuery(since(), newestFirst(), cursor)),
    (e) => e.id,
  );

  const off = events.on((e) => {
    if (!follow() || list.initial() || !props.match(e)) return;
    const where = livePlace(list.items(), e, newestFirst(), list.hasMore());
    if (where) list.upsert([e], where);
  });
  onCleanup(off);

  return (
    <div class="stack" style={{ gap: "8px" }}>
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <label class="row small">
          Since seq or event ID
          <input style={{ width: "10em" }} value={since()} onChange={(e) => setSince(e.currentTarget.value)} aria-label="Since" />
        </label>
        <button class="small" onClick={() => setSince("0")}>
          From the start
        </button>
        <button class="small" onClick={() => setSince(String(recentSince()))}>
          Recent
        </button>
        <label class="row small">
          <input type="checkbox" checked={newestFirst()} onChange={(e) => setNewestFirst(e.currentTarget.checked)} /> Newest first
        </label>
        <label class="row small">
          <input type="checkbox" checked={follow()} onChange={(e) => setFollow(e.currentTarget.checked)} /> Follow live
        </label>
        <span class="muted small">
          {list.items().length} events{list.hasMore() ? (newestFirst() ? ", older ones to load" : ", more after these") : ""} · stream {events.state()}
        </span>
      </div>
      <ErrorBanner error={list.error()} />
      <Show when={!list.initial()} fallback={<p class="muted">Loading…</p>}>
        <EventList events={list.items()} showAgent={props.showAgent} limit={100_000} />
      </Show>
      <LoadMore hasMore={list.hasMore()} loading={list.loading()} onMore={list.loadMore} />
    </div>
  );
}

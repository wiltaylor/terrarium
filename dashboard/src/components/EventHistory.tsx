import { useInstance } from "../instance";
// The event log after `?since=`, page by page (docs/api.md §9.1). Newest
// first, the list starts at the newest event, Load more adds older events
// at the bottom, and live frames go on top. Oldest first, the list starts
// at `since`, and live frames go at the end once the history has caught up.
// The live mode (components/Live.tsx) comes from the page, or the list has
// its own control.

import { createEffect, createSignal, on, onCleanup, Show } from "solid-js";
import type { Page } from "../api/http";
import type { AgentEvent, EventQuery } from "../api/types";

import { EventList } from "./EventList";
import { historyQuery, livePlace } from "./historyPaging";
import { createLive, createScrollKeeper, LiveControl, NewAbove, type Live } from "./Live";
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
  /** The live mode of the page, with its control in the page's toolbar. Absent: the list has its own. */
  live?: Live;
}) {
  const { events } = useInstance();
  // Default: the last few hundred events of the log. `since` is the lower
  // bound in both orders, so the two orders show the same events.
  const recentSince = () => Math.max(0, events.status().lastSeq - RECENT_WINDOW);
  const [since, setSince] = createSignal<string>(String(recentSince()));
  const live = props.live ?? createLive();
  const [newestFirst, setNewestFirst] = createSignal(true);

  const list = createPagedList(
    () => `${props.filterKey}|${since()}|${newestFirst()}`,
    (_k, cursor) => props.fetch(historyQuery(since(), newestFirst(), cursor)),
    (e) => e.id,
  );

  // Every matching live event goes in the list; off, it is held there.
  const off = events.on((e) => {
    if (list.initial() || !props.match(e)) return;
    const where = livePlace(list.items(), e, newestFirst(), list.hasMore());
    if (!where) return;
    live.arrive([e.id]);
    list.upsert([e], where);
  });
  onCleanup(off);
  // A new list loads every event again: nothing stays held.
  createEffect(on(() => `${props.filterKey}|${since()}|${newestFirst()}`, () => live.release(), { defer: true }));
  const keeper = createScrollKeeper(".event-row");
  const shown = () => live.shown(list.items(), (e) => e.id);

  return (
    <div class="stack" style={{ gap: "8px" }}>
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <Show when={!props.live}>
          <LiveControl live={live} count={live.state().held.length} />
        </Show>
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
        <span class="muted small">
          {shown().length} events{list.hasMore() ? (newestFirst() ? ", older ones to load" : ", more after these") : ""} · stream {events.state()}
        </span>
      </div>
      <ErrorBanner error={list.error()} />
      <NewAbove live={live} keeper={keeper} held={live.state().held.length} />
      <Show when={!list.initial()} fallback={<p class="muted">Loading…</p>}>
        <EventList events={shown()} showAgent={props.showAgent} limit={100_000} listRef={keeper.ref} />
      </Show>
      <LoadMore hasMore={list.hasMore()} loading={list.loading()} onMore={list.loadMore} />
    </div>
  );
}

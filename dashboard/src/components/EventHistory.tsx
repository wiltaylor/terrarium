// The event log from `?since=`, page by page, with live frames appended once
// the history has caught up (docs/api.md §9).

import { createSignal, onCleanup, Show } from "solid-js";
import type { Page } from "../api/http";
import type { AgentEvent, EventQuery } from "../api/types";
import { events } from "../stores";
import { EventList } from "./EventList";
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
  // Default: the last few hundred events of the log, so the newest show first.
  const recentSince = () => Math.max(0, events.status().lastSeq - RECENT_WINDOW);
  const [since, setSince] = createSignal<string>(String(recentSince()));
  const [follow, setFollow] = createSignal(true);
  const [newestFirst, setNewestFirst] = createSignal(true);

  const list = createPagedList(
    () => `${props.filterKey}|${since()}`,
    (_k, cursor) => props.fetch({ since: since().trim() || 0, cursor, limit: 100 }),
    (e) => e.id,
  );

  const off = events.on((e) => {
    if (!follow() || list.hasMore() || list.initial() || !props.match(e)) return;
    const last = list.items().at(-1);
    if (!last || e.seq > last.seq) list.upsert([e], "end");
  });
  onCleanup(off);

  const shown = () => (newestFirst() ? list.items().slice().reverse() : list.items());

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
          {list.items().length} events{list.hasMore() ? ", more after these" : ""} · stream {events.state()}
        </span>
      </div>
      <ErrorBanner error={list.error()} />
      <Show when={newestFirst() && list.hasMore()}>
        <LoadMore hasMore loading={list.loading()} onMore={list.loadMore} />
      </Show>
      <Show when={!list.initial()} fallback={<p class="muted">Loading…</p>}>
        <EventList events={shown()} showAgent={props.showAgent} limit={100_000} />
      </Show>
      <Show when={!newestFirst()}>
        <LoadMore hasMore={list.hasMore()} loading={list.loading()} onMore={list.loadMore} />
      </Show>
    </div>
  );
}

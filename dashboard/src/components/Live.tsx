// The live mode of the log tabs (Mail, Events, Audit): one control that
// looks and acts the same on each tab, and the scroll keeper that holds the
// viewport still when new rows come in on top. The state is the pure
// `live.ts`.

import { createEffect, createSignal, onCleanup, onMount, Show } from "solid-js";
import { arrive, heldCount, LIVE_START, release, setLive, shownOf, type LiveState } from "./live";

export interface Live {
  state: () => LiveState;
  live: () => boolean;
  /** New items came: shown at once (live), or held (off). */
  arrive: (keys: readonly string[]) => void;
  /** Show the held items: all, or the ones in `keys`. */
  release: (keys?: readonly string[]) => void;
  setLive: (on: boolean) => void;
  /** The list without the held items. */
  shown: <T>(items: readonly T[], key: (t: T) => string) => T[];
  /** The held items in a filtered list: the "Show N new" count. */
  held: <T>(items: readonly T[], key: (t: T) => string) => number;
}

/** Live mode, on at the start. */
export function createLive(): Live {
  const [state, setState] = createSignal<LiveState>(LIVE_START);
  return {
    state,
    live: () => state().live,
    arrive: (keys) => setState((s) => arrive(s, keys)),
    release: (keys) => setState((s) => release(s, keys)),
    setLive: (on) => setState((s) => setLive(s, on)),
    shown: (items, key) => shownOf(items, state(), key),
    held: (items, key) => heldCount(items, state(), key),
  };
}

/**
 * The live control: a "Live" switch with a pulsing dot when on, and
 * "Show N new" while held items wait. Each log tab puts it first in its
 * toolbar. `onShow` shows the counted items only (default: every held item).
 */
export function LiveControl(props: { live: Live; count: number; onShow?: () => void }) {
  return (
    <span class="row live-control">
      <label class={`live-toggle${props.live.live() ? " on" : ""}`} title={props.live.live() ? "New items come in on top as they are made" : "New items wait until you show them"}>
        <input type="checkbox" checked={props.live.live()} onChange={(e) => props.live.setLive(e.currentTarget.checked)} />
        <span class="live-dot" aria-hidden="true" />
        Live
      </label>
      <Show when={!props.live.live() && props.count > 0}>
        <button class="small" onClick={() => (props.onShow ? props.onShow() : props.live.release())}>
          Show {props.count} new
        </button>
      </Show>
    </span>
  );
}

/** The height of the sticky top bar (phone width), so a row under it counts as out of view. */
const stickyTop = (): number => {
  const bar = document.querySelector(".topbar");
  if (!bar || getComputedStyle(bar).display === "none") return 0;
  return Math.max(0, bar.getBoundingClientRect().bottom);
};

/**
 * Keep the viewport still when rows come in above it. Give `ref` the
 * element whose children are the rows; the page scrolls (the window). When
 * the first row is out of view, a new row on top moves the page down by its
 * height, so the row under the top of the viewport stays where it is. The
 * rows that came in above count in `above`, for a "N new" pill; it goes
 * back to 0 when the top of the list is in view again.
 */
export function createScrollKeeper(rowSelector: string) {
  const [list, setList] = createSignal<HTMLElement>();
  const [above, setAbove] = createSignal(0);
  const [scrolled, setScrolled] = createSignal(false);
  let anchor: { el: Element; top: number } | null = null;

  /** The first row whose bottom is under the top of the viewport. */
  const record = () => {
    const el = list();
    if (!el?.isConnected) return;
    const top = stickyTop();
    const first = el.firstElementChild;
    const row = [...el.children].find((c) => c.getBoundingClientRect().bottom > top);
    const down = !!first && !!row && row !== first && el.getBoundingClientRect().top < top;
    setScrolled(down);
    if (!down) setAbove(0);
    anchor = down && row ? { el: row, top: row.getBoundingClientRect().top } : null;
  };

  onMount(() => {
    // The page keeps the anchor itself: the browser's own scroll anchoring would move it twice.
    const root = document.documentElement;
    const before = root.style.overflowAnchor;
    root.style.overflowAnchor = "none";
    const onScroll = () => record();
    window.addEventListener("scroll", onScroll, { passive: true });
    onCleanup(() => {
      window.removeEventListener("scroll", onScroll);
      root.style.overflowAnchor = before;
    });
  });

  createEffect(() => {
    const el = list();
    if (!el) return;
    const observer = new MutationObserver((records) => {
      // A microtask after the change and before the paint: no jump shows.
      if (anchor?.el.isConnected) {
        const delta = anchor.el.getBoundingClientRect().top - anchor.top;
        if (delta) window.scrollBy(0, delta);
        const target = anchor.el;
        const added = records
          .flatMap((r) => [...r.addedNodes])
          .filter((n): n is Element => n instanceof Element && n.matches(rowSelector) && !!(n.compareDocumentPosition(target) & Node.DOCUMENT_POSITION_FOLLOWING));
        if (added.length) setAbove((n) => n + added.length);
      }
      record();
    });
    observer.observe(el, { childList: true });
    anchor = null;
    record();
    onCleanup(() => observer.disconnect());
  });

  return {
    /** Give it the element whose children are the rows. */
    ref: (el: HTMLElement) => setList(el),
    /** Rows that came in above the viewport since the top was last in view. */
    above,
    /** The top of the list is out of view. */
    scrolled,
    /** Go to the top of the page, where the newest rows and the toolbar are. */
    toTop: () => {
      // No anchor: rows that a click put in just now must not move the page back down.
      anchor = null;
      window.scrollTo(0, 0);
      setAbove(0);
    },
  };
}

/**
 * The pill over a scrolled list: "N new" (live, rows came in above) goes
 * up to them; "Show N new" (off, held rows wait) shows them and goes up.
 */
export function NewAbove(props: { live: Live; keeper: ReturnType<typeof createScrollKeeper>; held: number; onShow?: () => void }) {
  const text = () => {
    if (props.live.live()) return props.keeper.above() > 0 ? `↑ ${props.keeper.above()} new` : "";
    return props.keeper.scrolled() && props.held > 0 ? `↑ Show ${props.held} new` : "";
  };
  return (
    <div class="new-above-slot">
      <Show when={text()}>
        <button
          class="new-above"
          onClick={() => {
            if (!props.live.live()) (props.onShow ?? props.live.release)();
            props.keeper.toTop();
          }}
        >
          {text()}
        </button>
      </Show>
    </div>
  );
}

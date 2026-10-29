// Live mode of the log tabs (Mail, Events, Audit): the pure state. No Solid
// imports. Each tab puts every new item in its list. Live, the new items
// show at once. Off, the tab holds them: the list stays still, the "Show N
// new" button counts the held items that match the filters, and a click
// shows them. Live on again shows the held items too.

export interface LiveState {
  live: boolean;
  /** The keys of the new items that came while live was off. */
  held: readonly string[];
}

export const LIVE_START: LiveState = { live: true, held: [] };

/** New items came. Live: they show at once. Off: hold them. */
export function arrive(s: LiveState, keys: readonly string[]): LiveState {
  if (s.live) return s;
  const fresh = keys.filter((k) => !s.held.includes(k));
  return fresh.length ? { ...s, held: [...s.held, ...fresh] } : s;
}

/**
 * Show the held items: all of them, or the ones in `keys` (the held items
 * that match the filters; the others stay held). Live stays as it is.
 */
export function release(s: LiveState, keys?: readonly string[]): LiveState {
  if (!s.held.length) return s;
  if (!keys) return { ...s, held: [] };
  const shown = new Set(keys);
  const held = s.held.filter((k) => !shown.has(k));
  return held.length === s.held.length ? s : { ...s, held };
}

/** Turn live on or off. On shows the held items; off holds from now. */
export const setLive = (s: LiveState, live: boolean): LiveState => (live === s.live ? s : { live, held: [] });

/** The items to show: the list without the held items. */
export function shownOf<T>(items: readonly T[], s: LiveState, key: (t: T) => string): T[] {
  if (!s.held.length) return [...items];
  const held = new Set(s.held);
  return items.filter((t) => !held.has(key(t)));
}

/** The number of held items in `items`: give it the list after the filters, for "Show N new". */
export function heldCount<T>(items: readonly T[], s: LiveState, key: (t: T) => string): number {
  if (!s.held.length) return 0;
  const held = new Set(s.held);
  return items.reduce((n, t) => n + (held.has(key(t)) ? 1 : 0), 0);
}

/** The keys in `next` that are not in `prev`: the new items of a refresh. */
export function newKeys<T>(prev: readonly T[], next: readonly T[], key: (t: T) => string): string[] {
  const had = new Set(prev.map(key));
  return next.map(key).filter((k) => !had.has(k));
}

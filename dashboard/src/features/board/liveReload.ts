// The live reload of the board (TER-338): which events touch a card, and a
// trailing debounce with a max wait, so a busy project cannot keep a
// screen stale.

import type { AgentEvent } from "../../api/types";

/** The Done screen waits for 2 s of quiet before it reloads its pages. */
export const DONE_RELOAD_MS = 2000;
/** The board waits for 300 ms of quiet. */
export const BOARD_RELOAD_MS = 300;
/** A burst of events never holds a reload back for longer than this. */
export const RELOAD_MAX_WAIT_MS = 10_000;

/**
 * The card IDs that an event names: `card` and `data.card_id` of a card
 * event, `data.card` and the batch `data.cards` of a job event, and
 * `data.cards` of a test_run event.
 */
export function eventCards(e: AgentEvent): string[] {
  const out: string[] = [];
  const add = (v: unknown) => {
    if (typeof v === "string" && v) out.push(v);
  };
  add(e.card);
  add(e.data.card_id);
  add(e.data.card);
  if (Array.isArray(e.data.cards)) e.data.cards.forEach(add);
  return out;
}

/** True when the event names the card. Card IDs match without case, as the drawer does. */
export function eventTouchesCard(e: AgentEvent, card: string): boolean {
  const want = card.toUpperCase();
  return eventCards(e).some((c) => c.toUpperCase() === want);
}

/**
 * A trailing debounce with a max wait. Each `schedule(wait)` moves the run
 * to `wait` ms after the last call, but never later than `maxWait` ms after
 * the first call of the burst.
 */
export function createDebounce(fn: () => void, maxWait: number, clock: () => number = Date.now) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let first: number | undefined;
  const fire = () => {
    timer = undefined;
    first = undefined;
    fn();
  };
  return {
    schedule(wait: number) {
      const now = clock();
      first ??= now;
      clearTimeout(timer);
      timer = setTimeout(fire, Math.max(0, Math.min(wait, first + maxWait - now)));
    },
    cancel() {
      clearTimeout(timer);
      timer = undefined;
      first = undefined;
    },
  };
}

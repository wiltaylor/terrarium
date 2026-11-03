// Pinned conversations (docs/api.md §6.11): the pure parts of the side bar.
// The store (stores/pins.ts) keeps the list from the daemon; this file
// picks the open tab and the next tab after an unpin.

import type { Pin } from "../../api/types";

/** Is the agent `id` (a ULID) pinned? */
export const isPinned = (pins: readonly Pin[], id: string | undefined): boolean => !!id && pins.some((p) => p.agent === id);

/** The list without the pin of agent `id`. The same array when it has no such pin. */
export function dropPin(pins: Pin[], id: string | undefined): Pin[] {
  if (!id || !isPinned(pins, id)) return pins;
  return pins.filter((p) => p.agent !== id);
}

/** The open tab: `wanted` while it is pinned, else the first pin, else none. */
export function pickActive(pins: readonly Pin[], wanted: string | null): string | null {
  if (wanted && isPinned(pins, wanted)) return wanted;
  return pins[0]?.agent ?? null;
}

/**
 * The open tab after the unpin of `gone`. The open tab stays open; when it
 * was `gone`, its right neighbour opens, else its left neighbour, else none.
 * `before` is the list before the unpin.
 */
export function activeAfterUnpin(before: readonly Pin[], active: string | null, gone: string): string | null {
  if (active !== gone) return pickActive(before.filter((p) => p.agent !== gone), active);
  const i = before.findIndex((p) => p.agent === gone);
  if (i < 0) return pickActive(before, active);
  return before[i + 1]?.agent ?? before[i - 1]?.agent ?? null;
}

/** The tab that a key moves to in the tab strip (the WAI-ARIA tabs pattern). Null for another key. */
export function tabKeyTarget(pins: readonly Pin[], active: string | null, key: string): string | null {
  if (!pins.length) return null;
  const i = Math.max(0, pins.findIndex((p) => p.agent === active));
  switch (key) {
    case "ArrowRight":
    case "ArrowDown":
      return pins[(i + 1) % pins.length]!.agent;
    case "ArrowLeft":
    case "ArrowUp":
      return pins[(i - 1 + pins.length) % pins.length]!.agent;
    case "Home":
      return pins[0]!.agent;
    case "End":
      return pins[pins.length - 1]!.agent;
  }
  return null;
}

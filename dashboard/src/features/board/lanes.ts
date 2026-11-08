// The lanes of the Cards tab: pure helpers. No Solid imports.
//
// The board shows one plain lane with every card, or, grouped by version
// (`?group=version`), one horizontal lane for each version label on the
// board's cards (versionLabels.ts: vnext first, then the newest release) and a
// last lane "No version". Each lane shows every column. A card with several
// version labels shows in each of their lanes. The WIP limits and the column
// counts stay per column, over all lanes.

import type { CardSummary as Card } from "../../api/types";
import { boardVersions, cardVersions, versionLabel } from "./versionLabels";

/** The lane of the cards with no version label. A version lane key starts with `version:`, so it is never this word. */
export const NO_VERSION_LANE = "none";

/** The one lane of the board that is not grouped. */
export const ALL_LANE = "all";

export interface Lane {
  /** The version label (`version:1.4.0`), NO_VERSION_LANE, or ALL_LANE. */
  key: string;
  /** The version name of a version lane (`1.4.0`). */
  version?: string;
  /** The cards of the lane that are on the board. */
  cards: Card[];
}

/**
 * The lanes of the board. Not grouped: one lane ALL_LANE with every card.
 * Grouped: a lane for each version label on the cards, in version order, and
 * "No version" last. "No version" always shows: a card can go into it.
 */
export function buildLanes(cards: readonly Card[], grouped: boolean): Lane[] {
  if (!grouped) return [{ key: ALL_LANE, cards: [...cards] }];
  const lanes: Lane[] = boardVersions(cards).map((v) => ({ key: versionLabel(v), version: v, cards: cards.filter((c) => cardVersions(c.labels).includes(v)) }));
  lanes.push({ key: NO_VERSION_LANE, cards: cards.filter((c) => cardVersions(c.labels).length === 0) });
  return lanes;
}

/** The cards of one lane in one column, in column order. */
export function laneCells(lane: Lane, column: string): Card[] {
  return lane.cards.filter((c) => c.column === column).sort((a, b) => a.position - b.position);
}

/** The other version names of a card, for the "also in" note of a version lane. None in the plain lane. */
export function alsoIn(card: Pick<Card, "labels">, laneKey: string): string[] {
  if (laneKey === ALL_LANE) return [];
  return cardVersions(card.labels).filter((v) => versionLabel(v) !== laneKey);
}

/**
 * The labels of a card after a drag from one lane into another: the label of
 * the source lane goes, the label of the target lane comes. "No version" has
 * no label. Null when the labels do not change.
 */
export function laneMoveLabels(labels: readonly string[], fromLane: string, toLane: string): string[] | null {
  if (fromLane === toLane || fromLane === ALL_LANE || toLane === ALL_LANE) return null;
  let out = labels.filter((l) => l !== fromLane);
  if (toLane !== NO_VERSION_LANE && !out.includes(toLane)) out = [...out, toLane];
  return out.length === labels.length && out.every((l, i) => l === labels[i]) ? null : out;
}

/**
 * The index of a drop in the whole column from the index in a lane cell. `cell` is the lane's cards of the column
 * without the dragged card; `column` is all cards of the column without it, in order.
 */
export function columnIndex(cell: readonly Card[], column: readonly Card[], cellIndex: number): number {
  const at = cell[cellIndex];
  if (at) return Math.max(0, column.findIndex((c) => c.id === at.id));
  const last = cell.at(-1);
  return last ? column.findIndex((c) => c.id === last.id) + 1 : column.length;
}

/** The per-project state that the page keeps in localStorage. An old `rows` key in it has no effect. */
export interface LaneState {
  /** The keys of the collapsed lanes. */
  collapsed: string[];
}

export const laneStateKey = (project: string) => `terrarium.board.lanes.${project}`;

export function readLaneState(raw: string | null): LaneState {
  try {
    const v = JSON.parse(raw ?? "") as Partial<LaneState>;
    return { collapsed: Array.isArray(v.collapsed) ? v.collapsed.filter((x): x is string => typeof x === "string") : [] };
  } catch {
    return { collapsed: [] };
  }
}

export const toggleCollapsed = (s: LaneState, key: string): LaneState => ({
  collapsed: s.collapsed.includes(key) ? s.collapsed.filter((k) => k !== key) : [...s.collapsed, key],
});

/** True for the board that is not grouped: one lane, with no lane head. */
export const isPlain = (lanes: readonly Lane[]) => lanes.length === 1 && lanes[0]!.key === ALL_LANE;

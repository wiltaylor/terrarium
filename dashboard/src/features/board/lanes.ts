// The version rows ("lanes") of the Cards tab: pure helpers. No Solid imports.
//
// With `?rows=version`, the board has one horizontal lane for each version of the project that is not
// archived, and a last lane "No version". Each lane shows every column. A card in several versions shows in
// each of their lanes. A card that is only in archived versions goes to "No version". The WIP limits and the
// column counts stay per column, over all lanes.

import type { Card, Version, VersionPatch, VersionStatus } from "../../api/types";

/** The lane of the cards that are in no version (that is not archived). A version ID is a ULID, never this word. */
export const NO_VERSION_LANE = "none";

export type RowsMode = "none" | "version";

/** The order of the lanes by status; then the newest version first. */
export const LANE_STATUS_ORDER: Record<VersionStatus, number> = { draft: 0, in_review: 1, approved: 2, released: 3, rejected: 4 };

/** An empty lane of these statuses still shows: a card can go into it. */
const SHOW_EMPTY: readonly VersionStatus[] = ["draft", "in_review", "approved"];

/** A version takes card changes in these statuses only (store/versions.rs `editable`). */
export const CARD_EDIT_STATUSES: readonly VersionStatus[] = ["draft", "rejected"];
export const takesCards = (v: Pick<Version, "status">) => CARD_EDIT_STATUSES.includes(v.status);

export interface Lane {
  /** The version ID, or NO_VERSION_LANE. */
  key: string;
  version?: Version;
  /** The cards of the lane that are on the board. */
  cards: Card[];
}

/** The versions that have a lane, in lane order. */
export function laneVersions(versions: readonly Version[]): Version[] {
  return versions
    .filter((v) => !v.archived)
    .slice()
    .sort((a, b) => LANE_STATUS_ORDER[a.status] - LANE_STATUS_ORDER[b.status] || b.created_at.localeCompare(a.created_at) || b.id.localeCompare(a.id));
}

/**
 * The lanes of the board. An empty lane of a released or rejected version is hidden; an empty lane of a draft,
 * in-review, or approved version shows. The "No version" lane is always last and always shows.
 */
export function buildLanes(versions: readonly Version[], cards: readonly Card[]): Lane[] {
  const byId = new Map(cards.map((c) => [c.id, c]));
  const lanes: Lane[] = [];
  const placed = new Set<string>();
  for (const v of laneVersions(versions)) {
    const ids = new Set(v.cards.map((c) => c.id));
    const mine = [...ids].map((id) => byId.get(id)).filter((c): c is Card => !!c);
    for (const c of mine) placed.add(c.id);
    if (mine.length || SHOW_EMPTY.includes(v.status)) lanes.push({ key: v.id, version: v, cards: mine });
  }
  lanes.push({ key: NO_VERSION_LANE, cards: cards.filter((c) => !placed.has(c.id)) });
  return lanes;
}

/** The cards of one lane in one column, in column order. */
export function laneCells(lane: Lane, column: string): Card[] {
  return lane.cards.filter((c) => c.column === column).sort((a, b) => a.position - b.position);
}

/** The other lane versions of a card, for the "also in" note. */
export function alsoIn(cardId: string, laneKey: string, versions: readonly Version[]): Version[] {
  return laneVersions(versions).filter((v) => v.id !== laneKey && v.cards.some((c) => c.id === cardId));
}

/** What a drag from one lane into another does to the versions of the card. */
export interface LaneMove {
  /** The version that loses the card (the source lane), or none from "No version". */
  remove?: Version;
  /** The version that gets the card (the target lane), or none into "No version". */
  add?: Version;
  /** The card is in other lane versions too: they keep it, so the page asks first. */
  others: Version[];
  /** Why the move cannot be done, or null. */
  refused: string | null;
}

export function laneMove(cardId: string, fromLane: string, toLane: string, versions: readonly Version[]): LaneMove {
  const find = (k: string) => (k === NO_VERSION_LANE ? undefined : versions.find((v) => v.id === k));
  const remove = find(fromLane);
  const add = find(toLane);
  const others = alsoIn(cardId, fromLane, versions).filter((v) => v.id !== toLane);
  let refused: string | null = null;
  for (const v of [remove, add]) {
    if (v && !takesCards(v)) refused = `Version ${v.name} is ${v.status.replace("_", " ")}: its cards change only in draft or after a rejection.`;
  }
  return { ...(remove ? { remove } : {}), ...(add ? { add } : {}), others, refused };
}

/** The `PATCH /api/versions/{id}` calls of a drag between lanes, in order: the remove first, then the add. */
export function laneMovePatches(cardId: string, m: LaneMove): { version: string; patch: VersionPatch }[] {
  const out: { version: string; patch: VersionPatch }[] = [];
  if (m.remove) out.push({ version: m.remove.id, patch: { remove_cards: [cardId] } });
  if (m.add) out.push({ version: m.add.id, patch: { add_cards: [cardId] } });
  return out;
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

/** The rows mode from the URL, else the stored one, else none. */
export function parseRows(query: string | undefined, stored: string | null | undefined): RowsMode {
  const v = query ?? stored ?? "";
  return v === "version" ? "version" : "none";
}

/** The per-project state that the page keeps in localStorage. */
export interface LaneState {
  rows: RowsMode;
  /** The keys of the collapsed lanes. */
  collapsed: string[];
}

export const laneStateKey = (project: string) => `terrarium.board.lanes.${project}`;

export function readLaneState(raw: string | null): LaneState {
  try {
    const v = JSON.parse(raw ?? "") as Partial<LaneState>;
    return { rows: v.rows === "version" ? "version" : "none", collapsed: Array.isArray(v.collapsed) ? v.collapsed.filter((x): x is string => typeof x === "string") : [] };
  } catch {
    return { rows: "none", collapsed: [] };
  }
}

export const toggleCollapsed = (s: LaneState, key: string): LaneState => ({
  ...s,
  collapsed: s.collapsed.includes(key) ? s.collapsed.filter((k) => k !== key) : [...s.collapsed, key],
});

/** The versions of the toolbar menu: those with a lane, then (with the toggle) the archived ones, newest first. */
export function menuVersions(versions: readonly Version[], showArchived: boolean): { active: Version[]; archived: Version[] } {
  return {
    active: laneVersions(versions),
    archived: showArchived ? versions.filter((v) => v.archived).sort((a, b) => b.created_at.localeCompare(a.created_at)) : [],
  };
}

/** Find a version by ID or by name (a wikilink has the name). */
export const findVersion = (versions: readonly Version[], key: string | undefined) => (key ? (versions.find((v) => v.id === key) ?? versions.find((v) => v.name === key)) : undefined);

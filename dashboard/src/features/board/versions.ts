// The versions of the cards on the board (docs/api.md §13): pure helpers.
// No Solid imports. A card is in a version when `Version.cards` has it. A
// card can be in several versions, or in none. The Version filter of the
// Cards tab is in the URL query: `?version=<id>`, or `?version=none` for the
// cards in no version.

import type { Version, VersionStatus } from "../../api/types";
import { humanize } from "../../lib/format";

/** The chip class of a version status. */
export const VERSION_CHIP: Record<VersionStatus, string> = { draft: "", in_review: "warn", approved: "ok", rejected: "bad", released: "info" };

/** The query value of the "No version" filter. A version ID is a ULID, so it is never this word. */
export const NO_VERSION = "none";

/** The versions of each card, by card ID, in the order of the list. */
export function versionsByCard(versions: readonly Version[]): Map<string, Version[]> {
  const out = new Map<string, Version[]>();
  for (const v of versions) {
    for (const c of v.cards) {
      const list = out.get(c.id) ?? [];
      // A card that is two times in one version shows one chip.
      if (!list.includes(v)) list.push(v);
      out.set(c.id, list);
    }
  }
  return out;
}

/**
 * The filter that applies, from the `version` query value: "" (any
 * version), NO_VERSION, or a version ID. The value can also be a version
 * name. Before the versions load, and for a version that is not in the
 * list, the filter is "", so the board does not show as empty.
 */
export function parseVersionFilter(raw: string | undefined, versions: readonly Version[] | undefined): string {
  if (!raw || !versions) return "";
  if (raw === NO_VERSION) return NO_VERSION;
  return versions.find((v) => v.id === raw)?.id ?? versions.find((v) => v.name === raw)?.id ?? "";
}

/** True when the card passes the filter (parseVersionFilter). */
export function matchesVersion(cardId: string, filter: string, byCard: ReadonlyMap<string, readonly Version[]>): boolean {
  if (!filter) return true;
  const of = byCard.get(cardId) ?? [];
  if (filter === NO_VERSION) return of.length === 0;
  return of.some((v) => v.id === filter);
}

/** The options of the Version filter: any version, each version with its status, then "No version". */
export function versionOptions(versions: readonly Version[]): { value: string; label: string }[] {
  return [
    { value: "", label: "Any version" },
    ...versions.map((v) => ({ value: v.id, label: `${v.name} (${humanize(v.status)})` })),
    { value: NO_VERSION, label: "No version" },
  ];
}

/** The query value for a filter: no `version` key for any version. */
export const versionQuery = (filter: string): string | undefined => filter || undefined;

// The version labels of the cards (TER-58, TER-59): pure helpers. No Solid imports.
//
// A version is a card label, `version:vnext` for the open version or
// `version:X.Y.Z` for a release. There are no version objects. A card can
// have several version labels, or none. The board filters and groups the
// cards by these labels (lanes.ts); test runs and artifacts link to a label.

/** The prefix of a version label. */
export const VERSION_PREFIX = "version:";

/** The name of the open version: `version:vnext`. */
export const VNEXT = "vnext";

/** The filter value (`?version=none`) of the cards with no version label. A version name never has this value in a lane key. */
export const NO_VERSION = "none";

/** The version name of a label (`version:1.4.0` gives `1.4.0`), or undefined for another label. */
export function versionOf(label: string): string | undefined {
  if (!label.startsWith(VERSION_PREFIX)) return undefined;
  const name = label.slice(VERSION_PREFIX.length).trim();
  return name || undefined;
}

/** The label of a version name: `1.4.0` gives `version:1.4.0`. */
export const versionLabel = (name: string) => `${VERSION_PREFIX}${name}`;

/** The version names of a card's labels, in label order, each one time. */
export function cardVersions(labels: readonly string[]): string[] {
  const out: string[] = [];
  for (const l of labels) {
    const v = versionOf(l);
    if (v !== undefined && !out.includes(v)) out.push(v);
  }
  return out;
}

const SEMVER = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/;

/**
 * The order of the versions: vnext first, then the releases newest first
 * (a pre-release before its release is older), then other names A to Z.
 */
export function compareVersions(a: string, b: string): number {
  if (a === b) return 0;
  if (a === VNEXT) return -1;
  if (b === VNEXT) return 1;
  const x = SEMVER.exec(a);
  const y = SEMVER.exec(b);
  if (x && y) {
    for (let i = 1; i <= 3; i++) {
      const d = Number(y[i]) - Number(x[i]);
      if (d) return d;
    }
    // 1.0.0 is newer than 1.0.0-rc.1.
    if (!x[4] !== !y[4]) return x[4] ? 1 : -1;
    return (y[4] ?? "").localeCompare(x[4] ?? "", undefined, { numeric: true });
  }
  if (x) return -1;
  if (y) return 1;
  return a.localeCompare(b);
}

/** The version names on these cards, in version order. */
export function boardVersions(cards: readonly { labels: readonly string[] }[]): string[] {
  const all = new Set(cards.flatMap((c) => cardVersions(c.labels)));
  return [...all].sort(compareVersions);
}

/** Does a card pass the version filter? No filter: every card. `none`: the cards with no version label. */
export function matchesVersion(labels: readonly string[], filter: string | undefined): boolean {
  if (!filter) return true;
  const vs = cardVersions(labels);
  return filter === NO_VERSION ? vs.length === 0 : vs.includes(filter);
}

/** The board link of a label: a version label opens the board filtered by that version; another label has none. */
export function labelHref(project: string, label: string): string | undefined {
  const v = versionOf(label);
  return v === undefined ? undefined : `/p/${encodeURIComponent(project)}/board?version=${encodeURIComponent(v)}&group=version`;
}

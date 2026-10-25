// The update kind of a harness pin, the same rule as scripts/harness.py and
// the daemon (`harness_versions::update_kind`, docs/api.md GET /api/harnesses).
// The mock uses it to answer `?latest=true`; the table reads `update` from
// the daemon.

import type { HarnessUpdateKind } from "../api/types";

/** What the daemon says after a `harness.update`: the container cannot recreate itself. */
export const HARNESS_ROLLOUT = "rollout needed: run `just up` on the host";

const VERSION_RE = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/;

type Parsed = { core: [number, number, number]; pre: string };

function parse(v: string): Parsed | null {
  const m = VERSION_RE.exec(v.trim());
  return m ? { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ?? "" } : null;
}

/** Semver order: the core, then a pre-release before the release. */
function compare(a: Parsed, b: Parsed): number {
  for (let i = 0; i < 3; i++) if (a.core[i] !== b.core[i]) return a.core[i]! - b.core[i]!;
  if (a.pre === b.pre) return 0;
  if (!a.pre) return 1;
  if (!b.pre) return -1;
  return a.pre < b.pre ? -1 : 1;
}

/** The first non-zero part: it stays the same for a compatible version. */
function compatKey(p: Parsed): string {
  const [maj, min, pat] = p.core;
  if (maj) return `${maj}`;
  if (min) return `0.${min}`;
  return `0.0.${pat}`;
}

/** The bump from `pinned` to `latest`. A version that does not parse is a `minor` when the strings differ. */
export function harnessUpdateKind(pinned: string, latest: string): HarnessUpdateKind {
  const p = parse(pinned);
  const l = parse(latest);
  if (!p || !l) return pinned === latest ? "none" : "minor";
  if (compare(l, p) <= 0) return "none";
  return compatKey(p) === compatKey(l) ? "minor" : "major";
}

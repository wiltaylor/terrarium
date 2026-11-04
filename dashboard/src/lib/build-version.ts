// The build version of the Terrarium binaries (docs/api.md §4,
// `GET /api/system/version`): `<cargo version>+<git ref>`, `-dirty` for a
// build tree with changes, `+unknown` when the build did not know the ref.
// The same rules as the Rust crate `terrarium-version`.

import type { Host } from "../api/types";

export interface VersionParts {
  /** The crate version, for example `0.1.0`. */
  version: string;
  /** The commit, or null when it is not known (`+unknown`, or an older binary that sends only `0.1.0`). */
  ref: string | null;
  dirty: boolean;
}

/** Split `0.1.0+6e31754-dirty` into its parts. */
export function versionParts(full: string | null | undefined): VersionParts {
  const v = (full ?? "").trim();
  const plus = v.indexOf("+");
  const version = plus < 0 ? v : v.slice(0, plus);
  let meta = plus < 0 ? "" : v.slice(plus + 1);
  const dirty = meta.endsWith("-dirty");
  if (dirty) meta = meta.slice(0, -"-dirty".length);
  return { version, ref: meta && meta !== "unknown" ? meta : null, dirty };
}

/** The host agents that the daemon marks `stale`: built from another ref than the daemon. */
export function staleHosts(hosts: readonly Pick<Host, "name" | "stale">[] | null | undefined): string[] {
  return (hosts ?? []).filter((h) => h.stale === true).map((h) => h.name);
}

/** The tooltip of the STALE chip of a host agent. */
export function staleTitle(hostVersion: string | null | undefined, daemonVersion: string | null | undefined): string {
  const host = hostVersion || "an unknown version";
  return daemonVersion
    ? `This host agent runs ${host}, the daemon ${daemonVersion}. Reinstall it with the line of Add host (GET /install/host.sh).`
    : `This host agent runs ${host}, from another ref than the daemon. Reinstall it with the line of Add host (GET /install/host.sh).`;
}

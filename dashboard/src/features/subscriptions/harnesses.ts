// The "Harness versions" table of the Subscriptions page (GET /api/harnesses
// ?latest=true, docs/api.md §4). harnesses.toml gives the version of the first
// install; the daemon compares the installed version with the adapter's tested
// range at every agent start, and reads the latest release of each package on
// npm. An update runs npm in the container (POST /api/harnesses/{name}/update).

import { ApiError } from "../../api/http";
import type { HarnessReport, HarnessUpdateResult, HarnessVersion } from "../../api/types";

/** The status chip of one row. */
export type HarnessStatus = "unsupported" | "update available (major)" | "update available" | "latest" | "in range" | "unknown";

/**
 * The chip of one row. In order:
 * - `unsupported`: the installed version is outside the tested range (`in_range` false);
 * - `update available (major)` / `update available` / `latest`: from `update` (pin against npm);
 * - npm did not answer: `in range` when the installed version is in the range, else `unknown`.
 */
export function harnessStatus(h: HarnessVersion): { status: HarnessStatus; cls: string; title: string } {
  if (h.in_range === false)
    return {
      status: "unsupported",
      cls: "bad",
      title: `${h.installed} is outside the tested range ${h.tested}. Update it here, or run just harness::update ${h.harness}.`,
    };
  const latest = h.latest?.version;
  if (h.update === "major")
    return { status: "update available (major)", cls: "warn", title: `${current(h)} -> ${latest} is a major bump: hooks and flags may have changed.` };
  if (h.update === "minor") return { status: "update available", cls: "info", title: `${current(h)} -> ${latest} on npm.` };
  if (h.update === "none") return { status: "latest", cls: "ok", title: `${current(h)} is the latest release on npm.` };
  const why = h.latest_error ? `The latest release is not known: ${h.latest_error}` : "The latest release is not known.";
  if (h.in_range === true) return { status: "in range", cls: "ok", title: `${h.installed} is in ${h.tested}. ${why}` };
  return { status: "unknown", cls: "warn", title: h.error ?? "The daemon could not read the version." };
}

/** Does the installed version differ from the pin (in range or not)? */
export const offPin = (h: HarnessVersion) => h.installed !== null && h.installed !== h.pinned;

/** The harnesses outside their range. */
export const outOfRange = (r: HarnessReport | undefined) => (r?.harnesses ?? []).filter((h) => h.in_range === false);

/** The harnesses with a newer release on npm. */
export const withUpdate = (r: HarnessReport | undefined) => (r?.harnesses ?? []).filter((h) => h.update === "minor" || h.update === "major");

/** The version the update kind starts from: the installed one, else the pin (as the daemon does). */
export const current = (h: HarnessVersion) => h.installed ?? h.pinned;

/** The fallback line for a row with a newer release: npm in a shell in the container. Null when nothing is newer. */
export function npmCommand(h: HarnessVersion): string | null {
  if ((h.update !== "minor" && h.update !== "major") || !h.latest) return null;
  return `npm install -g ${h.package}@${h.latest.version}`;
}

/** One line for a finished update: `codex 0.164.0 → 0.165.0`. */
export const resultLine = (r: HarnessUpdateResult) => `${r.harness} ${r.from ?? "(none)"} → ${r.to ?? "(unknown)"}`;

/** What the box shows after an update call that failed. */
export interface UpdateFailure {
  message: string;
  /** The npm output of a 422 npm failure, or of the failed items of an "update all". */
  output: string | null;
  /** 409 with live agents: their names. The box then offers "Update anyway". */
  running: string[] | null;
}

/** Read an error of `harnessUpdate` or `harnessUpdateAll`. */
export function updateFailure(e: unknown): UpdateFailure {
  if (!(e instanceof ApiError)) return { message: e instanceof Error ? e.message : String(e), output: null, running: null };
  const d = (e.details ?? {}) as { output?: unknown; running?: unknown; results?: HarnessUpdateResult[] };
  const running = e.status === 409 && Array.isArray(d.running) ? d.running.map(String) : null;
  let output: string | null = typeof d.output === "string" ? d.output : null;
  if (output === null && Array.isArray(d.results))
    output = d.results.map((r) => (r.error ? `${r.harness}: ${r.error}\n${r.output}` : `${resultLine(r)}\n${r.output}`)).join("\n") || null;
  return { message: e.message, output, running };
}

/** `2026-10-25T09:30:00Z` as `2026-10-25`. */
export const publishedDay = (at: string | null | undefined) => (at ? at.slice(0, 10) : null);

/** The line under the table: what a start does outside the range. */
export function policyNote(r: HarnessReport): string {
  return r.policy === "refuse"
    ? "An agent start on a harness outside its tested range is refused (TERRARIUM_HARNESS_VERSION_POLICY=refuse)."
    : "An agent start on a harness outside its tested range goes ahead with a warning event (TERRARIUM_HARNESS_VERSION_POLICY=warn).";
}

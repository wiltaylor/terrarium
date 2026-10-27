// The "Harness versions" table of the Subscriptions page (GET /api/harnesses
// ?latest=true, docs/api.md §4). harnesses.toml gives the version of the first
// install; the daemon compares the installed version with the adapter's tested
// range at every agent start, and reads the latest release of each package on
// npm. An update runs npm in the container (POST /api/harnesses/{name}/update)
// into a new version dir and switches to it. Running agents keep their version
// until their next restart; the table names them.

import { ApiError } from "../../api/http";
import type { HarnessAgent, HarnessReport, HarnessUpdateResult, HarnessVersion } from "../../api/types";

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

/**
 * The fallback line for a row with a newer release: the CLI in a shell in the container. Null when nothing is
 * newer. A bare `npm install -g` into the tools dir would bypass the version dirs, so the line uses the daemon.
 */
export function updateCommand(h: HarnessVersion): string | null {
  if ((h.update !== "minor" && h.update !== "major") || !h.latest) return null;
  return `terrarium admin harness update ${h.harness} --version ${h.latest.version}`;
}

/** One line for a finished update: `codex 0.164.0 → 0.165.0`. */
export const resultLine = (r: HarnessUpdateResult) => `${r.harness} ${r.from ?? "(none)"} → ${r.to ?? "(unknown)"}`;

/** The note lines of a finished update: the agents that keep an older version, and the removed version dirs. */
export function resultNotes(r: HarnessUpdateResult): string[] {
  const out: string[] = [];
  if (r.note) out.push(r.note);
  if (r.removed?.length) out.push(`${r.harness}: removed the unused version${r.removed.length > 1 ? "s" : ""} ${r.removed.join(", ")}`);
  return out;
}

/** The live agents of a row that run another version than the active one. */
export const oldAgents = (h: HarnessVersion): HarnessAgent[] => (h.active ? (h.agents ?? []).filter((a) => a.version !== h.active) : []);

/** One agent on an older version: `terra/w runs 1.0.103; 1.0.110 at its next restart`. */
export const oldAgentLine = (a: HarnessAgent, active: string) => `${a.agent} runs ${a.version ?? "an older install"}; ${active} at its next restart`;

/** The version dirs on disk other than the active one (kept for a running agent, or for a quick switch back). */
export const otherVersions = (h: HarnessVersion) => (h.versions ?? []).filter((v) => v !== h.active);

/** What the box shows after an update call that failed. */
export interface UpdateFailure {
  message: string;
  /** The npm output of a 422 npm failure, or of the failed items of an "update all". */
  output: string | null;
}

/** Read an error of `harnessUpdate` or `harnessUpdateAll`. */
export function updateFailure(e: unknown): UpdateFailure {
  if (!(e instanceof ApiError)) return { message: e instanceof Error ? e.message : String(e), output: null };
  const d = (e.details ?? {}) as { output?: unknown; results?: HarnessUpdateResult[] };
  let output: string | null = typeof d.output === "string" ? d.output : null;
  if (output === null && Array.isArray(d.results))
    output = d.results.map((r) => (r.error ? `${r.harness}: ${r.error}\n${r.output}` : `${resultLine(r)}\n${r.output}`)).join("\n") || null;
  return { message: e.message, output };
}

/** `2026-10-25T09:30:00Z` as `2026-10-25`. */
export const publishedDay = (at: string | null | undefined) => (at ? at.slice(0, 10) : null);

/** The line under the table: what a start does outside the range. */
export function policyNote(r: HarnessReport): string {
  return r.policy === "refuse"
    ? "An agent start on a harness outside its tested range is refused (TERRARIUM_HARNESS_VERSION_POLICY=refuse)."
    : "An agent start on a harness outside its tested range goes ahead with a warning event (TERRARIUM_HARNESS_VERSION_POLICY=warn).";
}

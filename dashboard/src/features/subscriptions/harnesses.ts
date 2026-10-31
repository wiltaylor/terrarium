// The "Harness versions" table of the Subscriptions page (GET /api/harnesses
// ?latest=true, docs/api.md §4). harnesses.toml gives the version of the first
// install; the daemon reads the installed version of each harness, and the
// latest release of each package on npm. An update runs npm in the container (POST /api/harnesses/{name}/update)
// into a new version dir and switches to it. Running agents keep their version
// until their next restart; the table names them.

import { ApiError } from "../../api/http";
import { harnessUpdateKind } from "../../lib/harness-update";
import type { HarnessAgent, HarnessCheckItem, HarnessPinResult, HarnessReport, HarnessUpdateResult, HarnessVersion } from "../../api/types";

/** The status chip of one row. */
export type HarnessStatus = "update available (major)" | "update available" | "latest" | "installed" | "unknown";

/**
 * The chip of one row. In order:
 * - `update available (major)` / `update available` / `latest`: from `update` (pin against npm);
 * - npm did not answer: `installed` when the installed version is known, else `unknown`.
 */
export function harnessStatus(h: HarnessVersion): { status: HarnessStatus; cls: string; title: string } {
  const latest = h.latest?.version;
  if (h.update === "major")
    return { status: "update available (major)", cls: "warn", title: `${current(h)} -> ${latest} is a major bump: hooks and flags may have changed.` };
  if (h.update === "minor") return { status: "update available", cls: "info", title: `${current(h)} -> ${latest} on npm.` };
  if (h.update === "none") return { status: "latest", cls: "ok", title: `${current(h)} is the latest release on npm.` };
  const why = h.latest_error ? `The latest release is not known: ${h.latest_error}` : "The latest release is not known.";
  if (h.installed) return { status: "installed", cls: "ok", title: `${h.installed} is installed. ${why}` };
  return { status: "unknown", cls: "warn", title: h.error ?? "The daemon could not read the version." };
}

/** Does the installed version differ from the pin? */
export const offPin = (h: HarnessVersion) => h.installed !== null && h.installed !== h.pinned;

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

// ----- auto-update and pins (docs/api.md §4, "Auto-update and pins") -----

/** The policy chip of one row: `auto`, or `pinned <version>`. */
export function policyChip(h: HarnessVersion): { label: string; cls: string; title: string } {
  const a = h.auto_update;
  if (!a) return { label: "—", cls: "", title: "This daemon reports no update policy." };
  if (a.policy === "pinned")
    return { label: `pinned ${a.pinned_version ?? "?"}`, cls: "warn", title: `The auto-update does not change ${h.harness}. Unpin it to get updates again.` };
  return { label: "auto", cls: "info", title: `The auto-update installs each newer release of ${h.harness} on npm, majors too.` };
}

/** The version that "Roll back and pin" goes to: the previous version, when it is older than the active one. */
export function rollbackTarget(h: HarnessVersion): string | null {
  const p = h.auto_update?.previous ?? null;
  const active = h.active ?? h.installed;
  return p && active && harnessUpdateKind(p, active) !== "none" ? p : null;
}

/** Can "Pin current" pin this row? Only an `auto` row with a known active version. */
export const canPinCurrent = (h: HarnessVersion) => h.auto_update?.policy === "auto" && !!(h.active ?? h.installed);

/** Can "Unpin" unpin this row? */
export const canUnpin = (h: HarnessVersion) => h.auto_update?.policy === "pinned";

/** The last check of a row, in one line: `2026-10-31 10:05 · updated: 1.4.0 -> 1.5.1`. Null before the first check. */
export function lastCheckLine(h: HarnessVersion): string | null {
  const a = h.auto_update;
  if (!a?.checked_at) return null;
  const when = a.checked_at.slice(0, 16).replace("T", " ");
  return `${when} · ${a.result ?? "?"}${a.detail ? `: ${a.detail}` : ""}`;
}

/** The line about the background check: the schedule and the next check. */
export function scheduleNote(r: HarnessReport): string {
  const a = r.auto_update;
  if (!a) return "";
  if (a.schedule === "off") return "The auto-update is off (TERRARIUM_HARNESS_AUTO_UPDATE=off). Use Check now to check by hand.";
  const next = a.next_check_at ? ` The next check is at ${a.next_check_at.slice(0, 16).replace("T", " ")} UTC.` : "";
  return `The auto-update checks npm ${a.schedule === "daily" ? "daily" : `every ${a.schedule}`} and installs each newer release of a harness that is not pinned.${next}`;
}

/** One line for each harness of a check. */
export const checkLine = (i: HarnessCheckItem) => `${i.harness}: ${i.result}${i.detail ? `: ${i.detail}` : ""}${i.mailed ? " (mailed)" : ""}`;

/** One line for a pin or an unpin. */
export function pinLine(r: HarnessPinResult): string {
  if (r.policy === "auto") return `${r.harness}: unpinned; the auto-update can change it at the next check`;
  if (r.from === r.to) return `${r.harness}: pinned to ${r.to}`;
  return `${r.harness}: ${r.from ?? "(none)"} → ${r.to}, pinned. New agent starts use ${r.to}`;
}

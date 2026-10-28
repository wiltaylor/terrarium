// The config repo page model (PLAN §8.3, docs/api.md §25): the findings of
// a scan block and the conflicts of a merge from an error, the labels of
// the repo state, the buttons that the state allows, the plan summary of an
// export file, and the lines of a diff. Pure.

import { ApiError, isMissingEndpoint } from "../../api/http";
import type {
  ConfigChangeStatus,
  ConfigExportPlan,
  ConfigGrantApproval,
  ConfigScanFinding,
  ConfigSecretChange,
  ConfigSecretsImport,
  ConfigStatus,
  FleetPlan,
} from "../../api/types";

/** The note under the findings of a scan block. */
export const SCAN_NOTE = "Remove the secret, or for a false hit add `terrarium-scan: allow` to the line.";

/** The note of a daemon with no config repo. */
export const NO_REPO_NOTE = "This daemon has no config repo. Set TERRARIUM_CONFIG_REPO on the daemon to turn it on.";

const isFinding = (x: unknown): x is ConfigScanFinding =>
  typeof x === "object" && x !== null && typeof (x as ConfigScanFinding).path === "string" && typeof (x as ConfigScanFinding).line === "number" && typeof (x as ConfigScanFinding).rule === "string";

const details = (e: unknown): Record<string, unknown> | null =>
  e instanceof ApiError && typeof e.details === "object" && e.details !== null && !Array.isArray(e.details) ? (e.details as Record<string, unknown>) : null;

/** The findings of a commit or a sync that the secret scan blocked (`422`, `details.findings`), else null. */
export function scanFindings(e: unknown): ConfigScanFinding[] | null {
  if (!(e instanceof ApiError) || e.status !== 422) return null;
  const f = details(e)?.findings;
  return Array.isArray(f) && f.length ? f.filter(isFinding) : null;
}

/** The files of a merge that stopped on a conflict (`409`, `details.conflicts`), else null. */
export function mergeConflicts(e: unknown): string[] | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null;
  const c = details(e)?.conflicts;
  return Array.isArray(c) && c.length ? c.filter((x): x is string => typeof x === "string") : null;
}

/** The daemon has no config repo (`501 not_available`), or no `/api/config` routes. */
export const noConfigRepo = (e: unknown): boolean => (e instanceof ApiError && (e.status === 501 || e.code === "not_available")) || isMissingEndpoint(e);

/** `agents/lead.toml:12 anthropic-api-key`. */
export const findingLabel = (f: ConfigScanFinding) => `${f.path}:${f.line} ${f.rule}`;

const CHANGE_LABELS: Record<ConfigChangeStatus, string> = {
  added: "added",
  modified: "modified",
  deleted: "deleted",
  renamed: "renamed",
  untracked: "new",
  conflict: "conflict",
};

/** The word of a change, and the chip class. */
export function changeLabel(s: ConfigChangeStatus): { label: string; tone: "" | "ok" | "warn" | "bad" | "info" } {
  const tone = s === "conflict" ? "bad" : s === "deleted" ? "warn" : s === "added" || s === "untracked" ? "ok" : "info";
  return { label: CHANGE_LABELS[s] ?? s, tone };
}

/** "2 behind, 1 ahead", "up to date", or "no remote". */
export function aheadBehind(st: Pick<ConfigStatus, "remote" | "ahead" | "behind">): string {
  if (!st.remote) return "no remote";
  const parts = [st.behind ? `${st.behind} behind` : "", st.ahead ? `${st.ahead} ahead` : ""].filter(Boolean);
  return parts.length ? parts.join(", ") : "up to date";
}

const AUTO_SYNC: Record<ConfigStatus["auto_sync"], string> = {
  off: "off",
  pull: "pull (export changes wait for Apply export)",
  sync: "sync (commit, pull, and push)",
};

export const autoSyncLabel = (m: ConfigStatus["auto_sync"]) => AUTO_SYNC[m] ?? m;

/** A button: on, or off with the reason. */
export interface Action {
  enabled: boolean;
  reason?: string;
}

export interface Actions {
  commit: Action;
  sync: Action;
  pull: Action;
  push: Action;
  /** Pull with `apply`: apply the `export/` changes as fleets. */
  applyExport: Action;
  abort: Action;
  /** Show the Apply export button. */
  showApplyExport: boolean;
}

const on: Action = { enabled: true };
const off = (reason: string): Action => ({ enabled: false, reason });

/**
 * The buttons that the repo state allows. `pendingPlans`: the last pull
 * returned export plans. `busy`: a call is in flight.
 */
export function actions(st: ConfigStatus, o: { busy?: boolean; message?: string; pendingPlans?: boolean } = {}): Actions {
  const busy = o.busy ? off("A call is in progress.") : null;
  const noRemote = st.remote ? null : off("Set a remote first.");
  const merging = st.merging ? off("A merge waits. Resolve the conflicts and commit, or abort.") : null;
  const dirty = st.changes.length ? off("Commit the local changes first.") : null;
  const commit = busy ?? (!o.message?.trim() ? off("Write a commit message.") : !st.changes.length && !st.merging ? off("There is nothing to commit.") : on);
  const pull = busy ?? noRemote ?? merging ?? dirty ?? on;
  return {
    commit,
    sync: busy ?? noRemote ?? merging ?? on,
    pull,
    push: busy ?? noRemote ?? merging ?? (st.ahead ? on : off("The remote has every commit.")),
    applyExport: pull,
    abort: busy ?? (st.merging ? on : off("No merge waits.")),
    showApplyExport: st.export_pending || !!o.pendingPlans,
  };
}

/** "1 create, 1 update, 3 unchanged", or "no change". */
export function planSummary(p: FleetPlan): string {
  const s = p.summary;
  const parts = (["create", "update", "delete", "release", "conflict", "unchanged"] as const).filter((k) => s[k]).map((k) => `${s[k]} ${k}`);
  if (s.drift) parts.push(`${s.drift} drift`);
  return parts.length ? parts.join(", ") : "no change";
}

/** The line of an export file in a pull or sync answer. */
export function exportLine(x: ConfigExportPlan): { text: string; tone: "" | "ok" | "warn" | "bad" } {
  if (x.error) return { text: x.error, tone: "bad" };
  if (x.applied) return { text: "applied", tone: "ok" };
  if (x.plan) return { text: `${x.plan.valid ? "" : "not valid: "}${planSummary(x.plan)}`, tone: x.plan.valid && !x.plan.summary.conflict ? "" : "warn" };
  return { text: "no plan", tone: "" };
}

/** The plans that wait for Apply export: those of a plan, not of an apply. */
export const waitingPlans = (list: ConfigExportPlan[] | undefined): boolean => !!list?.some((x) => x.plan && !x.applied);

export type DiffLineKind = "add" | "del" | "hunk" | "meta" | "ctx";

/** The lines of a unified diff, with their kind. */
export function diffLines(text: string): { text: string; kind: DiffLineKind }[] {
  if (!text) return [];
  return text.split("\n").map((t) => {
    const kind: DiffLineKind = /^(diff |index |--- |\+\+\+ |new file|deleted file|similarity|rename )/.test(t)
      ? "meta"
      : t.startsWith("@@")
        ? "hunk"
        : t.startsWith("+")
          ? "add"
          : t.startsWith("-")
            ? "del"
            : "ctx";
    return { text: t, kind };
  });
}

/** The client check of a remote URL: the daemon refuses a user or a password in it. A problem, or null. */
export function remoteUrlProblem(url: string): string | null {
  const u = url.trim();
  if (!u) return "Give a URL.";
  if (/\s/.test(u)) return "A URL has no spaces.";
  const m = /^([a-z][a-z0-9+.-]*):\/\/([^/]*)/i.exec(u);
  if (m) {
    const at = m[2]!.lastIndexOf("@");
    if (at >= 0 && (m[1]!.toLowerCase().startsWith("http") || m[2]!.slice(0, at).includes(":")))
      return "Remove the user and the password from the URL. Put the token in a credential, and give its name.";
  }
  return null;
}

// ---- Secret grants that need the operator (S8, docs/api.md §25) ----

/** The warning of the confirm step of Apply with grant approval. */
export const GRANT_APPROVAL_WARNING =
  "These grants give agents the values of secrets: an agent with the read level can get the value, and an env var puts it in the agent's environment. Only approve grants that you expect.";

/** A grant change of the secrets import that has an `approval`, as the page shows it. */
export interface GrantApprovalRow {
  /** `global`, or the project. */
  scope: string;
  name: string;
  grantee: string;
  /** The level that the file gives: `use`, `read`, or "—" when the daemon did not say. */
  level: string;
  /** The env var that the file gives, or "". */
  env: string;
  /** What the grant changes: "new grant", "use → read", "new env GH_TOKEN", or "env A → B". */
  what: string;
  approval: ConfigGrantApproval;
}

/** What a grant change adds: a new grant, a wider level, or a new or changed env var. */
export function grantWhat(c: Pick<ConfigSecretChange, "level" | "env" | "was">): string {
  if (!c.was) return "new grant";
  const parts: string[] = [];
  if (c.level && c.level !== c.was.level) parts.push(`${c.was.level} → ${c.level}`);
  if (c.env && c.env !== c.was.env) parts.push(c.was.env ? `env ${c.was.env} → ${c.env}` : `new env ${c.env}`);
  return parts.length ? parts.join(", ") : "wider grant";
}

/** The grant changes of a secrets import that have an `approval` (needed, held, or approved). */
export function grantApprovals(s: ConfigSecretsImport | null | undefined): GrantApprovalRow[] {
  return (s?.changes ?? [])
    .filter((c): c is ConfigSecretChange & { approval: ConfigGrantApproval } => c.action === "grant" && !!c.approval)
    .map((c) => {
      const slash = c.secret.indexOf("/");
      return {
        scope: slash >= 0 ? c.secret.slice(0, slash) : "",
        name: slash >= 0 ? c.secret.slice(slash + 1) : c.secret,
        grantee: c.grantee ?? "—",
        level: c.level ?? "—",
        env: c.env ?? "",
        what: grantWhat(c),
        approval: c.approval,
      };
    });
}

/** True when a grant waits for the operator: a plan needs it, or an apply held it. */
export const grantsWait = (rows: GrantApprovalRow[]): boolean => rows.some((r) => r.approval === "needed" || r.approval === "held");

const APPROVAL_LABELS: Record<ConfigGrantApproval, { label: string; tone: "ok" | "warn" | "bad" }> = {
  needed: { label: "needs approval", tone: "warn" },
  held: { label: "held", tone: "bad" },
  approved: { label: "approved", tone: "ok" },
};

/** The chip of an approval. */
export const approvalLabel = (a: ConfigGrantApproval) => APPROVAL_LABELS[a] ?? { label: a, tone: "warn" as const };

/** The note above the grants: why they wait. Null when no grant waits. */
export function grantsNote(rows: GrantApprovalRow[]): string | null {
  if (rows.some((r) => r.approval === "held"))
    return "The last apply did not make these grants: they give secret values, and only the operator can approve them. The export stays pending until you approve them.";
  if (rows.some((r) => r.approval === "needed")) return "An apply of this export makes these grants only with your approval. Apply export without approval holds them.";
  return null;
}

/** The text of a refusal of `approve_grants` (`403`), else null. */
export const grantRefusal = (e: unknown): string | null => (e instanceof ApiError && e.status === 403 ? e.message : null);

/** The short form of a commit: 7 characters. */
export const shortCommit = (c: string | null | undefined) => (c ? c.slice(0, 7) : "—");

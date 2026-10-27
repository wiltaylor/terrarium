// The config repo of the mock (PLAN §8.3, docs/api.md §25), after the
// daemon's `api/config.rs` and `config_repo.rs`: a small in-memory repo with
// two changed files, one pending default, and a remote. A commit clears the
// changes. A commit (or a sync) with "secret" in the message gets the scan
// block (`422 invalid`, `details.findings`). A sync with "conflict" in the
// message stops the merge on a conflict (`409`, `details.conflicts`), so the
// merge banner can be seen. `available: false` acts as a daemon with no
// config repo (`501 not_available`).

import type { Api } from "../client";
import type { ConfigExportPlan, ConfigLogEntry, ConfigMerged, ConfigPendingDefault, ConfigScanFinding, ConfigStatus, FleetPlan } from "../types";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface ConfigMockContext {
  iso: () => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** False: a daemon with no config repo. Default true. */
  available?: boolean;
}

/** The marker that lets a line through the secret scan. */
export const SCAN_ALLOW_MARKER = "terrarium-scan: allow";

const EXPORT_FILE = "export/terrarium.toml";

/** The daemon's check of a remote URL (`check_url`): a user or a password in the URL is refused. A message, or null. */
export function remoteUrlError(url: string): string | null {
  const u = url.trim();
  if (!u || u !== url || u.startsWith("-") || /\s/.test(u)) return `\`${url}\` is not a git URL`;
  const lower = u.toLowerCase();
  if (lower.startsWith("ext::") || lower.startsWith("fd::")) return "the ext:: and fd:: transports are refused";
  const m = /^([^:/]+):\/\/(.*)$/.exec(u);
  if (m) {
    const scheme = m[1]!.toLowerCase();
    if (!["https", "http", "ssh", "git", "file"].includes(scheme)) return `the URL scheme \`${scheme}\` is not supported`;
    const authority = m[2]!.split("/")[0] ?? "";
    const at = authority.lastIndexOf("@");
    if (at >= 0 && (scheme.startsWith("http") || authority.slice(0, at).includes(":")))
      return "the URL holds a user or a password: put the token in a credential (`--credential <name>`), not in the URL";
    return null;
  }
  return u.startsWith("/") || u.includes(":") ? null : `\`${url}\` is not a git URL`;
}

/** The mock's plan of the export file: one agent to update, the rest unchanged. */
function exportPlan(): FleetPlan {
  return {
    fleet: EXPORT_FILE,
    valid: true,
    errors: [],
    warnings: [],
    entries: [
      { kind: "project", key: "terrarium", action: "unchanged" },
      { kind: "agent", key: "terrarium/lead", project: "terrarium", action: "update", changes: [{ field: "model", from: "claude-sonnet-4-5", to: "claude-opus-4-5" }] },
      { kind: "template", key: "_/worker", action: "create" },
    ],
    summary: { create: 1, update: 1, unchanged: 1, delete: 0, release: 0, conflict: 0, drift: 0 },
  };
}

/** The `Api["config"]` of the mock. */
export function createConfigMock(c: ConfigMockContext): Api["config"] {
  const { fail } = c;
  let seq = 0;
  const hash = () => `${(0x3a7f19c + ++seq * 0x1f3d).toString(16).padStart(7, "0")}e${seq}`;
  const st: ConfigStatus = {
    dir: "/etc/terrarium",
    initialized: true,
    branch: "main",
    head: "9c41e0a",
    remote: { url: "https://git.example.com/ops/terrarium-config.git", credential: "config-deploy", branch: "main" },
    ahead: 1,
    behind: 2,
    changes: [
      { path: "agents/terrarium/lead.toml", status: "modified" },
      { path: "prompts/reviewer.md", status: "untracked" },
    ],
    merging: false,
    conflicts: [],
    pending_defaults: ["prompts/leader.md"],
    export_pending: false,
    auto_sync: "off",
  };
  const diffs: Record<string, string> = {
    "agents/terrarium/lead.toml": [
      "diff --git a/agents/terrarium/lead.toml b/agents/terrarium/lead.toml",
      "--- a/agents/terrarium/lead.toml",
      "+++ b/agents/terrarium/lead.toml",
      "@@ -3,4 +3,5 @@",
      ' harness = "claude"',
      '-model = "claude-sonnet-4-5"',
      '+model = "claude-opus-4-5"',
      '+persona = "leader"',
      ' roles = ["project_admin"]',
    ].join("\n"),
    "prompts/reviewer.md": [
      "diff --git a/prompts/reviewer.md b/prompts/reviewer.md",
      "new file mode 100644",
      "--- /dev/null",
      "+++ b/prompts/reviewer.md",
      "@@ -0,0 +1,3 @@",
      "+# Reviewer",
      "+",
      "+Read each version. Write the findings on the card.",
    ].join("\n"),
  };
  const defaults: ConfigPendingDefault[] = [
    {
      path: "prompts/leader.md",
      diff: ["--- a/prompts/leader.md", "+++ b/prompts/leader.md.new", "@@ -1,3 +1,4 @@", " # Leader", " ", "-Plan the work.", "+Plan the work. Split it into cards.", "+Give each card to one worker."].join("\n"),
    },
  ];
  const log: ConfigLogEntry[] = [
    { commit: "9c41e0a", author: "operator", at: "2026-10-27T09:12:00Z", subject: "Add the reviewer template" },
    { commit: "51b7d2f", author: "operator", at: "2026-10-26T16:40:00Z", subject: "Terrarium config sync" },
    { commit: "0e2a913", author: "terrariumd", at: "2026-10-25T08:00:00Z", subject: "Terrarium config: the local files before the first pull" },
  ];

  const ensure = () => {
    if (c.available === false) fail(501, "not_available", "this daemon has no config repo (TERRARIUM_CONFIG_REPO)");
  };
  // Like the daemon: no `remote` key when there is no remote.
  const status = (): ConfigStatus => {
    const { remote, ...rest } = st;
    return structuredClone(remote ? { ...rest, remote } : rest);
  };
  const remote = () => st.remote ?? fail(422, "invalid", "the config repo has no remote: run `terrarium admin config remote set <url>`");

  const commit = (message: string, scanMessage = message) => {
    if (!message.trim()) fail(422, "invalid", "a commit needs a message");
    if (st.merging && st.conflicts.length) fail(409, "conflict", `resolve the conflicts first: ${st.conflicts.join(", ")} still have conflict markers`, { conflicts: [...st.conflicts] });
    if (/secret/i.test(scanMessage)) {
      const findings: ConfigScanFinding[] = [
        { path: "agents/terrarium/lead.toml", line: 7, rule: "anthropic-api-key" },
        { path: "prompts/reviewer.md", line: 3, rule: "generic-token" },
      ];
      fail(
        422,
        "invalid",
        `the secret scan blocked the commit (${findings.length} finding(s)): ${findings.map((f) => `${f.path}:${f.line} ${f.rule}`).join("; ")}. Remove the secret, or for a false hit add \`${SCAN_ALLOW_MARKER}\` to the line`,
        { findings },
      );
    }
    const files = st.changes.map((x) => x.path);
    if (!files.length && !st.merging) return { commit: null, files: [] };
    const commit = hash();
    log.unshift({ commit, author: "operator", at: c.iso(), subject: message.trim().split("\n")[0]! });
    st.head = commit;
    st.ahead += 1;
    st.changes = [];
    st.merging = false;
    st.conflicts = [];
    for (const f of files) delete diffs[f];
    return { commit, files };
  };

  /** Fetch and merge: the remote's two commits change `export/` and a prompt. */
  const merge = (): ConfigMerged => {
    remote();
    if (st.merging) fail(409, "conflict", "a merge waits for its conflicts: resolve them and commit, or abort");
    if (st.changes.length) fail(409, "conflict", `commit the local changes first: ${st.changes.map((x) => x.path).join(", ")}`);
    if (!st.behind) return { fetched: true, updated: false, changed: [], conflicts: [] };
    st.behind = 0;
    st.head = hash();
    log.unshift({ commit: st.head, author: "ops@example.com", at: c.iso(), subject: "Raise the lead to Opus" });
    return { fetched: true, updated: true, changed: [EXPORT_FILE, "prompts/worker.md"], conflicts: [] };
  };

  /** The export plans after a merge, as the daemon's `export_plans`. */
  const exportPlans = (m: ConfigMerged, apply: boolean): ConfigExportPlan[] => {
    if (!m.changed.some((p) => p.startsWith("export/")) && !st.export_pending) return [];
    st.export_pending = !apply;
    return [apply ? { path: EXPORT_FILE, applied: { fleet: EXPORT_FILE, created: 1, updated: 1, unchanged: 1 } } : { path: EXPORT_FILE, plan: exportPlan() }];
  };

  const push = () => {
    remote();
    const pushed = !!st.head && st.ahead > 0;
    st.ahead = 0;
    return pushed;
  };

  const takeDefault = (path: string, accept: boolean) => {
    const i = defaults.findIndex((d) => d.path === path);
    if (i < 0) fail(404, "not_found", `no new default waits for \`${path}\``);
    defaults.splice(i, 1);
    st.pending_defaults = defaults.map((d) => d.path);
    if (accept && !st.changes.some((x) => x.path === path)) {
      st.changes.push({ path, status: "modified" });
      diffs[path] = `--- a/${path}\n+++ b/${path}\n@@ -1 +1 @@\n-(the old default)\n+(the new default)`;
    }
    c.audit(accept ? "config.default.accept" : "config.default.keep", path, undefined);
    return structuredClone(defaults);
  };

  const wrap = <T>(fn: () => T) =>
    c.reply(() => {
      ensure();
      return fn();
    });

  return {
    status: () => wrap(status),
    diff: (path) =>
      wrap(() => {
        if (path) return { diff: diffs[path] ?? "" };
        return { diff: st.changes.map((x) => diffs[x.path] ?? "").filter(Boolean).join("\n") };
      }),
    log: (limit = 20) => wrap(() => log.slice(0, limit)),
    commit: (body) =>
      wrap(() => {
        if (!body.message?.trim()) fail(422, "invalid", "a commit needs a message");
        const exp = body.export === false || (st.export_pending && !body.force_export) ? [] : [EXPORT_FILE];
        if (exp.length) st.export_pending = false;
        const done = commit(body.message);
        c.audit("config.commit", done.commit ?? "", undefined, { commit: done.commit });
        return { ...done, export: exp };
      }),
    pull: (body = {}) =>
      wrap(() => {
        const m = merge();
        const exp = exportPlans(m, !!body.apply);
        c.audit("config.pull", st.branch, undefined, { changed: m.changed });
        return { ...m, export: exp };
      }),
    push: () => wrap(() => ({ pushed: push() })),
    sync: (body = {}) =>
      wrap(() => {
        remote();
        const message = body.message?.trim() || "Terrarium config sync";
        const committed = commit(message);
        if (/conflict/i.test(message) && st.behind) {
          st.merging = true;
          st.conflicts = ["agents/terrarium/lead.toml"];
          st.changes = [{ path: "agents/terrarium/lead.toml", status: "conflict" }];
          diffs["agents/terrarium/lead.toml"] = ["<<<<<<< HEAD", 'model = "claude-opus-4-5"', "=======", 'model = "claude-sonnet-4-5"', ">>>>>>> origin/main"].join("\n");
          fail(409, "conflict", "the merge stopped on a conflict in agents/terrarium/lead.toml: edit the files, then commit; or abort. No local commit is lost", { conflicts: [...st.conflicts] });
        }
        const m = merge();
        const pushed = push();
        const exp = exportPlans(m, !!body.apply);
        c.audit("config.sync", st.branch, undefined, { commit: committed.commit, changed: m.changed });
        return { committed, ...m, pushed, export: exp };
      }),
    abort: () =>
      wrap(() => {
        if (!st.merging) fail(409, "conflict", "no merge waits");
        st.merging = false;
        st.conflicts = [];
        st.changes = [];
        delete diffs["agents/terrarium/lead.toml"];
        return { aborted: true as const };
      }),
    setRemote: (body) =>
      wrap(() => {
        const e = remoteUrlError(body.url ?? "");
        if (e) fail(422, "invalid", e, { field: "url" });
        const branch = body.branch?.trim() || "main";
        if (!/^[A-Za-z0-9._/-]+$/.test(branch)) fail(422, "invalid", `bad branch \`${branch}\``);
        st.remote = { url: body.url, ...(body.credential?.trim() ? { credential: body.credential.trim() } : {}), branch };
        st.ahead = 0;
        st.behind = 0;
        c.audit("config.remote.set", body.url, undefined, { branch });
        return status();
      }),
    clearRemote: () =>
      wrap(() => {
        st.remote = undefined;
        st.ahead = 0;
        st.behind = 0;
        c.audit("config.remote.clear", "", undefined);
        return status();
      }),
    defaults: () => wrap(() => structuredClone(defaults)),
    acceptDefault: (path) => wrap(() => takeDefault(path, true)),
    keepDefault: (path) => wrap(() => takeDefault(path, false)),
  };
}

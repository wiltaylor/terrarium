import { describe, expect, it } from "vitest";
import { ApiError } from "../../api/http";
import type { ConfigSecretChange, ConfigSecretsImport, ConfigStatus, FleetPlan } from "../../api/types";
import { createMockApi } from "../../api/mock";
import {
  actions,
  aheadBehind,
  approvalLabel,
  autoSyncLabel,
  changeLabel,
  diffLines,
  exportLine,
  findingLabel,
  GRANT_APPROVAL_WARNING,
  grantApprovals,
  grantRefusal,
  grantsNote,
  grantsWait,
  grantWhat,
  mergeConflicts,
  noConfigRepo,
  planSummary,
  remoteUrlProblem,
  scanFindings,
  shortCommit,
  waitingPlans,
} from "./model";

const err = (status: number, code: string, details?: unknown) => new ApiError(status, "m", { error: { code, message: "m", details } }, code, details);

const status = (o: Partial<ConfigStatus> = {}): ConfigStatus => ({
  dir: "/etc/terrarium",
  initialized: true,
  branch: "main",
  head: "abcdef0123",
  remote: { url: "https://git.example.com/c.git", branch: "main" },
  ahead: 0,
  behind: 0,
  changes: [],
  merging: false,
  conflicts: [],
  pending_defaults: [],
  export_pending: false,
  auto_sync: "off",
  ...o,
});

const plan = (s: Partial<FleetPlan["summary"]> = {}, valid = true): FleetPlan => ({
  fleet: "export/t.toml",
  valid,
  errors: [],
  warnings: [],
  entries: [],
  summary: { create: 0, update: 0, unchanged: 0, delete: 0, release: 0, conflict: 0, drift: 0, ...s },
});

describe("config errors", () => {
  it("reads the findings of a scan block, and nothing else", () => {
    const f = [{ path: "a.toml", line: 3, rule: "aws-key" }];
    expect(scanFindings(err(422, "invalid", { findings: f }))).toEqual(f);
    expect(scanFindings(err(422, "invalid", { field: "url" }))).toBeNull();
    expect(scanFindings(err(409, "conflict", { findings: f }))).toBeNull();
    expect(scanFindings(new Error("x"))).toBeNull();
    expect(findingLabel(f[0]!)).toBe("a.toml:3 aws-key");
  });

  it("reads the conflicts of a merge", () => {
    expect(mergeConflicts(err(409, "conflict", { conflicts: ["a", "b"] }))).toEqual(["a", "b"]);
    expect(mergeConflicts(err(409, "conflict"))).toBeNull();
    expect(mergeConflicts(err(422, "invalid", { conflicts: ["a"] }))).toBeNull();
  });

  it("knows a daemon with no config repo", () => {
    expect(noConfigRepo(err(501, "not_available"))).toBe(true);
    expect(noConfigRepo(new ApiError(404, "no such API route", {}, "not_found"))).toBe(true);
    expect(noConfigRepo(err(409, "conflict"))).toBe(false);
    expect(noConfigRepo(undefined)).toBe(false);
  });
});

describe("config labels", () => {
  it("names the remote state, the auto-sync mode, the changes, and a commit", () => {
    expect(aheadBehind(status({ ahead: 1, behind: 2 }))).toBe("2 behind, 1 ahead");
    expect(aheadBehind(status())).toBe("up to date");
    expect(aheadBehind(status({ remote: undefined }))).toBe("no remote");
    expect(autoSyncLabel("off")).toBe("off");
    expect(changeLabel("untracked")).toEqual({ label: "new", tone: "ok" });
    expect(changeLabel("conflict").tone).toBe("bad");
    expect(shortCommit("abcdef0123")).toBe("abcdef0");
    expect(shortCommit(null)).toBe("—");
  });

  it("sums a plan, and names an export line", () => {
    expect(planSummary(plan({ create: 1, update: 2, unchanged: 3 }))).toBe("1 create, 2 update, 3 unchanged");
    expect(planSummary(plan())).toBe("no change");
    expect(exportLine({ path: "p", plan: plan({ update: 1 }) })).toEqual({ text: "1 update", tone: "" });
    expect(exportLine({ path: "p", plan: plan({ conflict: 1 }) }).tone).toBe("warn");
    expect(exportLine({ path: "p", plan: plan({}, false) }).text).toBe("not valid: no change");
    expect(exportLine({ path: "p", applied: {} })).toEqual({ text: "applied", tone: "ok" });
    expect(exportLine({ path: "p", error: "bad file" })).toEqual({ text: "bad file", tone: "bad" });
    expect(waitingPlans([{ path: "p", plan: plan() }])).toBe(true);
    expect(waitingPlans([{ path: "p", applied: {} }])).toBe(false);
  });

  it("marks the lines of a diff", () => {
    expect(diffLines("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n same").map((l) => l.kind)).toEqual(["meta", "meta", "hunk", "del", "add", "ctx"]);
    expect(diffLines("")).toEqual([]);
  });

  it("refuses a user or a password in the remote URL", () => {
    expect(remoteUrlProblem("https://git.example.com/c.git")).toBeNull();
    expect(remoteUrlProblem("git@github.com:o/c.git")).toBeNull();
    expect(remoteUrlProblem("ssh://git@host/c.git")).toBeNull();
    expect(remoteUrlProblem("https://user:tok@host/c.git")).not.toBeNull();
    expect(remoteUrlProblem("https://tok@host/c.git")).not.toBeNull();
    expect(remoteUrlProblem("")).not.toBeNull();
  });
});

describe("config actions", () => {
  it("commits with a message and changes, or during a merge", () => {
    const changed = status({ changes: [{ path: "a", status: "modified" }] });
    expect(actions(changed, { message: "m" }).commit.enabled).toBe(true);
    expect(actions(changed, { message: " " }).commit.enabled).toBe(false);
    expect(actions(status(), { message: "m" }).commit.enabled).toBe(false);
    expect(actions(status({ merging: true }), { message: "m" }).commit.enabled).toBe(true);
    expect(actions(changed, { message: "m", busy: true }).commit.enabled).toBe(false);
  });

  it("needs a remote to sync, pull, or push; a pull needs a clean tree; a merge stops them", () => {
    const none = actions(status({ remote: undefined, ahead: 1 }));
    expect([none.sync.enabled, none.pull.enabled, none.push.enabled]).toEqual([false, false, false]);
    expect(actions(status({ changes: [{ path: "a", status: "added" }] })).pull.reason).toMatch(/Commit the local changes/);
    expect(actions(status({ changes: [{ path: "a", status: "added" }] })).sync.enabled).toBe(true);
    expect(actions(status({ ahead: 1 })).push.enabled).toBe(true);
    expect(actions(status()).push.enabled).toBe(false);
    const merging = actions(status({ merging: true, conflicts: ["a"], ahead: 1 }));
    expect([merging.sync.enabled, merging.pull.enabled, merging.push.enabled, merging.abort.enabled]).toEqual([false, false, false, true]);
    expect(actions(status()).abort.enabled).toBe(false);
  });

  it("shows Apply export when an export waits or a pull returned plans", () => {
    expect(actions(status()).showApplyExport).toBe(false);
    expect(actions(status({ export_pending: true })).showApplyExport).toBe(true);
    expect(actions(status(), { pendingPlans: true }).showApplyExport).toBe(true);
  });
});

describe("config grant approval", () => {
  const imp = (changes: ConfigSecretChange[]): ConfigSecretsImport => ({ changes, warnings: [] });

  it("names what a grant adds: new, use → read, a new or changed env", () => {
    expect(grantWhat({ level: "read", env: "GH_TOKEN" })).toBe("new grant");
    expect(grantWhat({ level: "read", was: { level: "use" } })).toBe("use → read");
    expect(grantWhat({ level: "read", env: "GH_TOKEN", was: { level: "read" } })).toBe("new env GH_TOKEN");
    expect(grantWhat({ level: "read", env: "B", was: { level: "read", env: "A" } })).toBe("env A → B");
    expect(grantWhat({ level: "read", env: "B", was: { level: "use" } })).toBe("use → read, new env B");
    expect(grantWhat({ was: { level: "read" } })).toBe("wider grant");
  });

  it("keeps only the grants with an approval, and splits the secret into scope and name", () => {
    const rows = grantApprovals(
      imp([
        { action: "create", secret: "web/npm" },
        { action: "grant", secret: "web/npm", grantee: "agent:web/lead", level: "use" },
        { action: "revoke", secret: "global/gh", grantee: "project:old" },
        { action: "grant", secret: "global/gh", grantee: "agent:web/lead", approval: "held", level: "read", env: "GH_TOKEN" },
        { action: "grant", secret: "web/db", grantee: "project:web", approval: "needed", level: "read", was: { level: "use" } },
      ]),
    );
    expect(rows).toEqual([
      { scope: "global", name: "gh", grantee: "agent:web/lead", level: "read", env: "GH_TOKEN", what: "new grant", approval: "held" },
      { scope: "web", name: "db", grantee: "project:web", level: "read", env: "", what: "use → read", approval: "needed" },
    ]);
    expect(grantsWait(rows)).toBe(true);
    expect(grantApprovals(null)).toEqual([]);
    // An old daemon: no level and no `was`.
    expect(grantApprovals(imp([{ action: "grant", secret: "global/gh", grantee: "project:p", approval: "needed" }]))[0]).toMatchObject({ level: "—", what: "new grant" });
  });

  it("waits only for needed or held grants; the note says why", () => {
    const row = (approval: "needed" | "held" | "approved") => grantApprovals(imp([{ action: "grant", secret: "global/gh", grantee: "project:p", approval }]));
    expect(grantsWait(row("approved"))).toBe(false);
    expect(grantsNote(row("approved"))).toBeNull();
    expect(grantsNote(row("needed"))).toMatch(/only with your approval/);
    expect(grantsNote(row("held"))).toMatch(/did not make these grants/);
    expect(approvalLabel("held")).toEqual({ label: "held", tone: "bad" });
    expect(approvalLabel("needed").tone).toBe("warn");
    expect(approvalLabel("approved").tone).toBe("ok");
    expect(GRANT_APPROVAL_WARNING).toMatch(/give agents the values of secrets/);
  });

  it("reads the text of a 403 refusal, and nothing else", () => {
    expect(grantRefusal(new ApiError(403, "`approve_grants` is for the operator only", { error: { code: "forbidden", message: "x" } }, "forbidden"))).toMatch(/operator only/);
    expect(grantRefusal(err(409, "conflict"))).toBeNull();
    expect(grantRefusal(new Error("x"))).toBeNull();
  });
});

describe("config mock", () => {
  const mock = (o = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });

  it("commits the changes; a message with 'secret' gets the scan block", async () => {
    const api = mock();
    const st = await api.config.status();
    expect(st.changes.length).toBe(2);
    expect(st.pending_defaults).toEqual(["prompts/leader.md"]);
    expect(st.remote).toMatchObject({ credential: "config-deploy", branch: "main" });
    expect((await api.config.diff("agents/terrarium/lead.toml")).diff).toMatch(/^\+model/m);
    const blocked = await api.config.commit({ message: "add the secret" }).catch((e: unknown) => e);
    expect(scanFindings(blocked)?.length).toBe(2);
    expect((await api.config.status()).changes.length).toBe(2);
    await expect(api.config.commit({ message: " " })).rejects.toMatchObject({ status: 422, code: "invalid" });
    const r = await api.config.commit({ message: "Raise the lead" });
    expect(r.commit).toBeTruthy();
    expect(r.files).toEqual(["agents/terrarium/lead.toml", "prompts/reviewer.md"]);
    expect(r.export).toEqual(["export/terrarium.toml"]);
    expect((await api.config.status()).changes).toEqual([]);
    expect((await api.config.log(1))[0]!.subject).toBe("Raise the lead");
  });

  it("pulls with export plans that wait, then applies them; pushes", async () => {
    const api = mock();
    await expect(api.config.pull()).rejects.toMatchObject({ status: 409, code: "conflict" });
    await api.config.commit({ message: "local" });
    const p = await api.config.pull();
    expect(p).toMatchObject({ fetched: true, updated: true, conflicts: [] });
    expect(p.export[0]!.plan?.summary.update).toBe(1);
    expect((await api.config.status()).export_pending).toBe(true);
    const a = await api.config.pull({ apply: true });
    expect(a.export[0]!.applied).toBeDefined();
    // The held grants keep the export pending until the operator approves them.
    expect(grantsWait(grantApprovals(a.secrets))).toBe(true);
    expect((await api.config.status()).export_pending).toBe(true);
    const b = await api.config.pull({ apply: true, approve_grants: true });
    expect(grantApprovals(b.secrets).map((r) => r.approval)).toEqual(["approved", "approved"]);
    expect((await api.config.status()).export_pending).toBe(false);
    expect(await api.config.push()).toEqual({ pushed: true });
    expect(await api.config.push()).toEqual({ pushed: false });
  });

  it("stops a sync on a conflict, and aborts the merge", async () => {
    const api = mock();
    const e = await api.config.sync({ message: "make a conflict" }).catch((x: unknown) => x);
    expect(mergeConflicts(e)).toEqual(["agents/terrarium/lead.toml"]);
    expect((await api.config.status()).merging).toBe(true);
    expect(await api.config.abort()).toEqual({ aborted: true });
    expect((await api.config.status()).merging).toBe(false);
    await expect(api.config.abort()).rejects.toMatchObject({ status: 409 });
    const s = await api.config.sync({});
    expect(s).toMatchObject({ pushed: true, updated: true });
  });

  it("sets and removes the remote; refuses a password in the URL", async () => {
    const api = mock();
    await expect(api.config.setRemote({ url: "https://u:p@h/c.git" })).rejects.toMatchObject({ status: 422 });
    const st = await api.config.setRemote({ url: "git@h:o/c.git", credential: "k", branch: "prod" });
    expect(st.remote).toEqual({ url: "git@h:o/c.git", credential: "k", branch: "prod" });
    const cleared = await api.config.clearRemote();
    expect(cleared).not.toHaveProperty("remote");
    await expect(api.config.push()).rejects.toMatchObject({ status: 422 });
  });

  it("accepts or keeps a new default", async () => {
    const api = mock();
    expect((await api.config.defaults())[0]!.diff).toMatch(/Split it into cards/);
    expect(await api.config.acceptDefault("prompts/leader.md")).toEqual([]);
    expect((await api.config.status()).pending_defaults).toEqual([]);
    await expect(api.config.keepDefault("prompts/leader.md")).rejects.toMatchObject({ status: 404 });
  });

  it("answers 501 not_available without a config repo", async () => {
    const e = await mock({ configRepo: false }).config.status().catch((x: unknown) => x);
    expect(e).toMatchObject({ status: 501, code: "not_available" });
    expect(noConfigRepo(e)).toBe(true);
  });
});

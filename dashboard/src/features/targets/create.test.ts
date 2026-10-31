import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import { createBody, createCreateTargetDialog, emptyCreateForm, formFullName, GLOBAL, showsField } from "./create";

const mock = (o: { denyGlobalTargets?: boolean; maxTargetsPerProject?: number } = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });

describe("the create-target dialog default", () => {
  it("defaults the scope to the selected project", () => {
    expect(emptyCreateForm("lab").scope).toBe("lab");
  });

  it("never defaults to global: with no project, the viewer must choose", () => {
    expect(emptyCreateForm(null).scope).toBe("");
    expect(emptyCreateForm(undefined).scope).toBe("");
    expect(createBody(emptyCreateForm(null, { name: "x", kind: "host", host: "lab1" }))).toEqual({ error: "Choose a scope: a project, or global." });
  });
});

describe("createBody", () => {
  it("sends project for a project scope, and no project for global", () => {
    expect(createBody(emptyCreateForm("lab", { name: " box ", kind: "host", host: "lab1" }))).toEqual({ body: { name: "box", kind: "host", project: "lab", host: "lab1" } });
    expect(createBody(emptyCreateForm("lab", { name: "box", kind: "host", host: "lab1", scope: GLOBAL }))).toEqual({ body: { name: "box", kind: "host", host: "lab1" } });
  });

  it("sends only the fields of the kind", () => {
    const f = emptyCreateForm("lab", { name: "vm", kind: "vmlab", host: "lab1", lab: "/labs/a", machine: "win", image: "alpine:3", address: "x", lab_repo: "app", lab_path: "x", ephemeral: true, keep: true, snapshot_before_task: true });
    expect(createBody(f)).toEqual({ body: { name: "vm", kind: "vmlab", project: "lab", host: "lab1", lab: "/labs/a", machine: "win", ephemeral: true, keep: true, snapshot_before_task: true } });
    const ssh = emptyCreateForm("lab", { name: "s", kind: "ssh", address: "u@h", port: "2222", credential: "k", host: "lab1", ephemeral: true });
    expect(createBody(ssh)).toEqual({ body: { name: "s", kind: "ssh", project: "lab", address: "u@h", port: 2222, credential: "k" } });
    expect(showsField("docker", "snapshot_before_task")).toBe(false);
  });

  it("sends the fields of the display kinds (docs/api.md §17.10)", () => {
    const vnc = emptyCreateForm("lab", { name: "v", kind: "vnc", host: "lab1", address: "10.0.0.5:5900", username: "bob", domain: "LAB", credential: "pw", image: "x", display: "x11" });
    expect(createBody(vnc)).toEqual({ body: { name: "v", kind: "vnc", project: "lab", host: "lab1", address: "10.0.0.5:5900", username: "bob", credential: "pw" } });
    const rdp = emptyCreateForm("lab", { name: "r", kind: "rdp", host: "lab1", address: "win:3389", username: "bob", domain: "LAB", credential: "pw" });
    expect(createBody(rdp)).toEqual({ body: { name: "r", kind: "rdp", project: "lab", host: "lab1", address: "win:3389", username: "bob", domain: "LAB", credential: "pw" } });
    expect(createBody(emptyCreateForm("lab", { name: "r", kind: "rdp", host: "lab1" }))).toEqual({ error: "A rdp target needs an address (host:port)." });
    const desk = emptyCreateForm("lab", { name: "d", kind: "desktop", host: "lab1", display: "wayland", display_name: "wayland-1", address: "x" });
    expect(createBody(desk)).toEqual({ body: { name: "d", kind: "desktop", project: "lab", host: "lab1", display: "wayland", display_name: "wayland-1" } });
    const docker = emptyCreateForm("lab", { name: "x", kind: "docker", host: "lab1", image: "desk:1", display: "x11", display_name: ":1" });
    expect(createBody(docker)).toEqual({ body: { name: "x", kind: "docker", project: "lab", host: "lab1", image: "desk:1", display: "x11", display_name: ":1" } });
    expect(createBody(emptyCreateForm("lab", { name: "x", kind: "docker", host: "lab1", image: "desk:1", display_name: ":1" }))).toHaveProperty("error");
    expect(showsField("vmlab", "display")).toBe(false);
  });

  it("sends the resource limits of a host or docker target (docs/api.md §17.2)", async () => {
    const host = emptyCreateForm("lab", { name: "h", kind: "host", host: "lab1", cpu: "0.5", memory: "4G", pids: "256" });
    expect(createBody(host)).toEqual({ body: { name: "h", kind: "host", project: "lab", host: "lab1", limits: { cpu: 0.5, memory: "4G", pids: 256 } } });
    expect(createBody(emptyCreateForm("lab", { name: "h", kind: "host", host: "lab1", memory: "4X" }))).toHaveProperty("error");
    expect(createBody(emptyCreateForm("lab", { name: "d", kind: "docker", host: "lab1", container: "c", cpu: "1" }))).toHaveProperty("error");
    // A kind without limits drops the values that stay in the form.
    expect(createBody(emptyCreateForm("lab", { name: "s", kind: "ssh", address: "u@h", cpu: "x" }))).toEqual({ body: { name: "s", kind: "ssh", project: "lab", address: "u@h" } });
    expect(showsField("vmlab", "cpu")).toBe(false);
    const api = mock();
    const t = await createCreateTargetDialog(api, "terrarium", { name: "lim", kind: "docker", host: "lab1", image: "alpine:3", cpu: "2", memory: "1G" }).submit();
    expect(t).toMatchObject({ name: "terrarium-lim", limits: { cpu: 2, memory: "1G" }, limits_info: { summary: "2 CPU · 1G", per: "container" } });
    // The mock merges a patch into the limits, as the daemon does.
    const u = await api.targets.update("terrarium-lim", { limits: { pids: 64 } });
    expect(u.limits).toEqual({ cpu: 2, memory: "1G", pids: 64 });
    await expect(api.targets.update("terrarium-lim", { limits: { memory: "1M" } })).rejects.toMatchObject({ status: 422 });
    const cleared = await api.targets.update("terrarium-lim", { limits: null });
    expect(cleared.limits).toBeUndefined();
    expect(cleared.limits_info).toBeUndefined();
  });

  it("creates a vnc target through the mock", async () => {
    const api = mock();
    // The credential names a login secret (PLAN §16b.6a); the mock checks it as the daemon does.
    const dlg = createCreateTargetDialog(api, "terrarium", { name: "win", kind: "vnc", host: "lab1", address: "10.0.0.9", credential: "win-vnc-pw" });
    const t = await dlg.submit();
    expect(t).toMatchObject({ name: "terrarium-win", kind: "vnc", has_display: true });
  });

  it("says what is missing or bad", () => {
    expect(createBody(emptyCreateForm("lab", { kind: "host", host: "lab1" }))).toHaveProperty("error");
    expect(createBody(emptyCreateForm("lab", { name: "d", kind: "docker", host: "lab1" }))).toEqual({ error: "A docker target needs an image or a container." });
    expect(createBody(emptyCreateForm("lab", { name: "d", kind: "docker", host: "lab1", container: "c", ephemeral: true }))).toEqual({ error: "An ephemeral docker target needs an image." });
    expect(createBody(emptyCreateForm("lab", { name: "v", kind: "vmlab", host: "lab1", lab: "labs/a.wcl" }))).toEqual({ error: "The lab must be an absolute path." });
    expect(createBody(emptyCreateForm("lab", { name: "h", kind: "host", host: "-x" }))).toHaveProperty("error");
    expect(createBody(emptyCreateForm("lab", { name: "s", kind: "ssh", address: "h", port: "99999" }))).toHaveProperty("error");
  });
});

describe("createCreateTargetDialog", () => {
  it("creates a target of the selected project", async () => {
    const api = mock();
    const d = createCreateTargetDialog(api, "terrarium", { name: "box", kind: "host", host: "lab1" });
    const t = await d.submit();
    expect(t).toMatchObject({ name: "terrarium-box", scope: "project", project: "terrarium" });
    expect(d.outcome().kind).toBe("done");
  });

  it("shows the daemon's 403 apart for a global target the viewer may not define, and a change clears it", async () => {
    const api = mock({ denyGlobalTargets: true });
    const d = createCreateTargetDialog(api, "terrarium", { name: "g", kind: "host", host: "lab1", scope: GLOBAL });
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "forbidden", scope: GLOBAL });
    d.set("scope", "terrarium");
    expect(d.outcome().kind).toBe("idle");
    expect(await d.submit()).toMatchObject({ scope: "project" });
  });

  it("keeps other errors as errors, and a bad form as invalid", async () => {
    const api = mock();
    const d = createCreateTargetDialog(api, "terrarium", { name: "lab-win11", kind: "host", host: "lab1", scope: GLOBAL });
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "error" });
    d.set("host", "");
    await d.submit();
    expect(d.outcome()).toMatchObject({ kind: "invalid" });
  });
});

describe("the lab source of a vmlab target (docs/api.md §17.2)", () => {
  const vm = (o: Parameters<typeof emptyCreateForm>[1]) => emptyCreateForm("terra", { name: "lab", kind: "vmlab", host: "lab1", machine: "dev", ...o });

  it("shows the fields of each source only", () => {
    expect(emptyCreateForm("terra").labSource).toBe("dir");
    expect(["lab", "lab_repo", "lab_ref", "lab_path", "snapshot_before_task"].filter((k) => showsField("vmlab", k as never, "dir"))).toEqual(["lab", "snapshot_before_task"]);
    expect(["lab", "lab_repo", "lab_ref", "lab_path", "snapshot_before_task"].filter((k) => showsField("vmlab", k as never, "repo"))).toEqual(["lab_repo", "lab_ref", "lab_path", "snapshot_before_task"]);
    expect(["lab", "lab_repo", "lab_ref", "lab_path", "snapshot_before_task"].filter((k) => showsField("vmlab", k as never, "workspace"))).toEqual(["lab_path"]);
    expect(showsField("docker", "lab_repo", "repo")).toBe(false);
    expect(showsField("vmlab", "labSource")).toBe(true);
  });

  it("sends a lab dir, without the fields of the other sources", () => {
    expect(createBody(vm({ lab: "/labs/win11", lab_repo: "app", lab_ref: "v1", lab_path: "x" }))).toEqual({ body: { name: "lab", kind: "vmlab", project: "terra", host: "lab1", lab: "/labs/win11", machine: "dev" } });
    expect(createBody(vm({}))).toEqual({ error: "A vmlab target needs a lab directory." });
  });

  it("sends a lab repo with its ref and path", () => {
    const f = vm({ labSource: "repo", lab: "/ignored", lab_repo: "app", lab_ref: "v2", lab_path: "labs/net", snapshot_before_task: true });
    expect(createBody(f)).toEqual({ body: { name: "lab", kind: "vmlab", project: "terra", host: "lab1", machine: "dev", lab_repo: "app", lab_ref: "v2", lab_path: "labs/net", snapshot_before_task: true } });
    expect(createBody(vm({ labSource: "repo", lab_repo: "app" }))).toMatchObject({ body: { lab_repo: "app" } });
    expect(createBody(vm({ labSource: "repo" }))).toEqual({ error: "Choose the lab repo." });
    expect(createBody(vm({ labSource: "repo", lab_repo: "app", machine: "" }))).toHaveProperty("error");
    expect(createBody(vm({ labSource: "repo", lab_repo: "app", lab_ref: "-x" }))).toHaveProperty("error");
  });

  it("sends lab_from_workspace with its path, and never snapshot_before_task", () => {
    const f = vm({ labSource: "workspace", lab_path: "lab", snapshot_before_task: true, ephemeral: true });
    expect(createBody(f)).toEqual({ body: { name: "lab", kind: "vmlab", project: "terra", host: "lab1", machine: "dev", lab_path: "lab", lab_from_workspace: true, ephemeral: true } });
    expect(createBody(vm({ labSource: "workspace", machine: "" }))).toEqual({ error: "A lab repo or a lab from the workspace needs the machine that gets the workspace." });
  });

  it("refuses a repo or workspace lab for a global target, and a bad lab path", () => {
    expect(createBody(vm({ labSource: "workspace", scope: GLOBAL }))).toEqual({ error: "A lab repo or a lab from the workspace needs a project target." });
    for (const p of ["/abs", "a/../b", "..", "a b", "-x"]) expect(createBody(vm({ labSource: "workspace", lab_path: p }))).toEqual({ error: "The lab path must be a relative directory with no `..`." });
    expect(createBody(vm({ labSource: "workspace", lab_path: "labs/dev" }))).toMatchObject({ body: { lab_path: "labs/dev" } });
  });

  it("creates each source on the mock, with the project prefix", async () => {
    const api = mock();
    const repo = createCreateTargetDialog(api, "terrarium", { name: "net", kind: "vmlab", host: "lab1", machine: "r", labSource: "repo", lab_repo: "terrarium", lab_path: "labs/x" });
    expect(await repo.submit()).toMatchObject({ name: "terrarium-net", lab_repo: "terrarium", lab_path: "labs/x", scope: "project" });
    const ws = createCreateTargetDialog(api, "terrarium", { name: "terrarium-ws2", kind: "vmlab", host: "lab1", machine: "dev", labSource: "workspace" });
    expect(await ws.submit()).toMatchObject({ name: "terrarium-ws2", lab_from_workspace: true });
  });
});

describe("the full-name preview (docs/api.md §17.2, the project prefix)", () => {
  it("prefixes the name for a project scope only, and keeps a name that has the prefix", () => {
    expect(formFullName({ name: "box", scope: "terra" })).toBe("terra-box");
    expect(formFullName({ name: "terra-box", scope: "terra" })).toBe("terra-box");
    expect(formFullName({ name: "box", scope: GLOBAL })).toBe("box");
    expect(formFullName({ name: "box", scope: "" })).toBe("box");
    expect(formFullName({ name: "  ", scope: "terra" })).toBe("");
  });

  it("follows the form, and matches the name the daemon answers", async () => {
    const api = mock();
    const d = createCreateTargetDialog(api, "webshop", { kind: "host", host: "lab1" });
    expect(d.fullName()).toBe("");
    d.set("name", "cache");
    expect(d.fullName()).toBe("webshop-cache");
    d.set("scope", "terrarium");
    expect(d.fullName()).toBe("terrarium-cache");
    d.set("scope", GLOBAL);
    expect(d.fullName()).toBe("cache");
    d.set("scope", "webshop");
    const t = await d.submit();
    expect(t?.name).toBe(d.fullName());
  });
});

describe("the project cap (TERRARIUM_MAX_TARGETS_PER_PROJECT)", () => {
  it("shows the 409 with its details apart from other errors", async () => {
    const api = mock({ maxTargetsPerProject: 1 });
    const d = createCreateTargetDialog(api, "webshop", { name: "more", kind: "host", host: "lab1" });
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "cap", cap: { cap: "project", project: "webshop", targets: 1, max: 1 } });
    d.set("name", "again");
    expect(d.outcome().kind).toBe("idle");
  });
});

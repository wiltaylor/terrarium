import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import { createBody, createCreateTargetDialog, emptyCreateForm, GLOBAL, showsField } from "./create";

const mock = (o: { denyGlobalTargets?: boolean } = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });

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
    const f = emptyCreateForm("lab", { name: "vm", kind: "vmlab", host: "lab1", lab: "/labs/a.wcl", machine: "win", image: "alpine:3", address: "x", ephemeral: true, keep: true, snapshot_before_task: true });
    expect(createBody(f)).toEqual({ body: { name: "vm", kind: "vmlab", project: "lab", host: "lab1", lab: "/labs/a.wcl", machine: "win", ephemeral: true, keep: true, snapshot_before_task: true } });
    const ssh = emptyCreateForm("lab", { name: "s", kind: "ssh", address: "u@h", port: "2222", credential: "k", host: "lab1", ephemeral: true });
    expect(createBody(ssh)).toEqual({ body: { name: "s", kind: "ssh", project: "lab", address: "u@h", port: 2222, credential: "k" } });
    expect(showsField("docker", "snapshot_before_task")).toBe(false);
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
    expect(t).toMatchObject({ name: "box", scope: "project", project: "terrarium" });
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
    const d = createCreateTargetDialog(api, "terrarium", { name: "dev-vm", kind: "host", host: "lab1" });
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "error" });
    d.set("host", "");
    await d.submit();
    expect(d.outcome()).toMatchObject({ kind: "invalid" });
  });
});

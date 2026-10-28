// The secret endpoints of the mock answer in the daemon's shapes
// (docs/api.md §5.2, §17.2 `credential_info` and the target test).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import { MOCK_PASSWORD } from "./ws";

const mock = (o: Parameters<typeof createMockApi>[0] = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });
const KEY = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n";

describe("secrets (docs/api.md §5.2)", () => {
  it("lists secrets in the secret shape, with no value", async () => {
    const api = mock();
    const list = await api.secrets.list();
    expect(list.length).toBeGreaterThan(0);
    for (const s of list) {
      expect(Object.keys(s).sort()).toEqual(["created_at", "created_by", "fields", "id", "kind", "name", "scope", "secret_fields", "updated_at"]);
      for (const f of s.secret_fields) expect(s.fields).not.toHaveProperty(f);
    }
    expect(JSON.stringify(list)).not.toMatch(/mock-vnc-password|ghp_mock/);
    expect((await api.secrets.list({ scope: "global" })).every((s) => s.scope === "global")).toBe(true);
    await expect(api.secrets.list({ scope: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("creates each kind with its defaults, splits a generic into plain and secret fields, and refuses a name twice", async () => {
    const api = mock();
    const t = await api.secrets.create({ name: "gl", kind: "token", fields: { value: " glpat-1 ", hosts: "gitlab.lan, ghe.lan" } });
    expect(t).toMatchObject({ scope: "global", kind: "token", fields: { username: "x-access-token", hosts: ["gitlab.lan", "ghe.lan"] }, secret_fields: ["value"] });
    expect((await api.secrets.create({ name: "gh2", kind: "github_token", fields: { value: "x" } })).fields).toEqual({ host: "github.com" });
    const g = await api.secrets.create({ scope: "terrarium", name: "door", kind: "generic", fields: { user: "bob", pin: "1234" }, secret_fields: ["pin"] });
    expect(g).toMatchObject({ scope: "terrarium", fields: { user: "bob" }, secret_fields: ["pin"] });
    await expect(api.secrets.create({ scope: "terrarium", name: "door", kind: "api_key", fields: { value: "x" } })).rejects.toMatchObject({ status: 409 });
    await expect(api.secrets.create({ name: "x", kind: "api_key", fields: { value: "x", nope: "1" } })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.secrets.create({ name: "x", kind: "login", fields: { username: "a" } })).rejects.toMatchObject({ status: 422 });
  });

  it("derives the public key and the fingerprint of an ssh_key, and refuses a body that sets them or a PEM with a passphrase", async () => {
    const api = mock();
    const k = await api.secrets.create({ name: "k1", kind: "ssh_key", fields: { private_key: KEY } });
    expect(k.secret_fields).toEqual(["private_key"]);
    expect(k.fields.public_key).toMatch(/^ssh-ed25519 AAAA\S+ terrarium:global\/k1$/);
    expect(k.fields.fingerprint).toMatch(/^SHA256:/);
    await expect(api.secrets.create({ name: "k2", kind: "ssh_key", fields: { private_key: KEY, public_key: "x" } })).rejects.toMatchObject({ status: 422 });
    await expect(api.secrets.create({ name: "k3", kind: "ssh_key", fields: { private_key: "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n" } })).rejects.toMatchObject({ status: 422 });
  });

  it("replaces with PUT, keeping the id and created_at", async () => {
    const api = mock();
    const a = await api.secrets.create({ name: "r1", kind: "api_key", fields: { value: "one" } });
    const b = await api.secrets.put("global", "r1", { kind: "api_key", fields: { value: "two", env: "R1" } });
    expect(b).toMatchObject({ id: a.id, created_at: a.created_at, fields: { env: "R1" } });
    const r = await api.secrets.reveal("global", "r1", MOCK_PASSWORD);
    expect(r.values).toEqual({ value: "two" });
  });

  it("generates an ed25519 key by default, an rsa key with bits, 409 on a name that exists unless replace", async () => {
    const api = mock();
    const e = await api.secrets.generate("terrarium", "gen1");
    expect(e).toMatchObject({ scope: "terrarium", name: "gen1", kind: "ssh_key", secret_fields: ["private_key"] });
    expect(e.fields.public_key).toMatch(/^ssh-ed25519 .* terrarium:terrarium\/gen1$/);
    expect((await api.secrets.generate("global", "gen2", { type: "rsa", bits: 4096, comment: "me@box" })).fields.public_key).toMatch(/^ssh-rsa \S+ me@box$/);
    await expect(api.secrets.generate("terrarium", "gen1")).rejects.toMatchObject({ status: 409 });
    const again = await api.secrets.generate("terrarium", "gen1", { replace: true });
    expect(again.id).toBe(e.id);
    expect(again.fields.fingerprint).not.toBe(e.fields.fingerprint);
    await expect(api.secrets.generate("global", "gen3", { bits: 4096 })).rejects.toMatchObject({ status: 422 });
  });

  it("reveals for the operator with the password only: 403 wrong, 409 none set, 422 empty, 429 after five", async () => {
    const api = mock();
    const r = await api.secrets.reveal("global", "win-vnc-pw", MOCK_PASSWORD);
    expect(r.values).toEqual({ password: "mock-vnc-password" });
    expect(r.name).toBe("win-vnc-pw");
    expect(api.world.audit.at(-1)).toMatchObject({ action: "secret.reveal", target: "global/win-vnc-pw" });
    expect(JSON.stringify(api.world.audit.at(-1))).not.toMatch(/mock-vnc-password/);
    await expect(api.secrets.reveal("global", "win-vnc-pw", "")).rejects.toMatchObject({ status: 422 });
    for (let i = 0; i < 5; i++) await expect(api.secrets.reveal("global", "win-vnc-pw", "wrong")).rejects.toMatchObject({ status: 403 });
    await expect(api.secrets.reveal("global", "win-vnc-pw", MOCK_PASSWORD)).rejects.toMatchObject({ status: 429 });
    await expect(mock({ password: null }).secrets.reveal("global", "win-vnc-pw", "x")).rejects.toMatchObject({ status: 409 });
    await expect(mock({ scheduleCaller: { role: "sysadmin" } }).secrets.reveal("global", "win-vnc-pw", MOCK_PASSWORD)).rejects.toMatchObject({ status: 403 });
  });

  it("checks the scope rights: a project_admin manages its project, not global or another project", async () => {
    const api = mock({ scheduleCaller: { role: "project_admin", project: "webshop" } });
    expect((await api.secrets.list()).every((s) => s.scope === "webshop")).toBe(true);
    await expect(api.secrets.create({ name: "x", kind: "api_key", fields: { value: "x" } })).rejects.toMatchObject({ status: 403 });
    await expect(api.secrets.create({ scope: "terrarium", name: "x", kind: "api_key", fields: { value: "x" } })).rejects.toMatchObject({ status: 403 });
    expect((await api.secrets.create({ scope: "webshop", name: "x", kind: "api_key", fields: { value: "x" } })).scope).toBe("webshop");
    await expect(mock({ scheduleCaller: { role: "member", project: "webshop" } }).secrets.list()).rejects.toMatchObject({ status: 403 });
  });

  it("grants with the per-kind default level, shows the grantee by label, replaces a second grant, and revokes", async () => {
    const api = mock();
    const g = await api.secrets.grant("global", "github-deploy", { grantee: "project:webshop" });
    expect(g).toEqual({ scope: "global", name: "github-deploy", grantee: "project:webshop", grantee_label: "project:webshop", level: "use", created_by: "operator", created_at: g.created_at });
    const a = api.world.agents.find((x) => x.project === "terrarium")!;
    const r = await api.secrets.grant("terrarium", "openai", { grantee: `agent:terrarium/${a.name}`, env: "STRIPE_API_KEY" });
    expect(r).toMatchObject({ grantee: `agent:${a.id}`, grantee_label: `agent:terrarium/${a.name}`, level: "read", env: "STRIPE_API_KEY" });
    await api.secrets.grant("terrarium", "openai", { grantee: `agent:${a.id}`, level: "read" });
    const list = await api.secrets.grants("terrarium", "openai");
    expect(list).toHaveLength(1);
    expect(list[0]).not.toHaveProperty("env");
    await api.secrets.revoke("terrarium", "openai", `agent:terrarium/${a.name}`);
    expect(await api.secrets.grants("terrarium", "openai")).toEqual([]);
    await expect(api.secrets.revoke("terrarium", "openai", "project:terrarium")).rejects.toMatchObject({ status: 404 });
  });

  it("refuses an env var without read, a reserved env name, a project secret to another project, and an unknown grantee", async () => {
    const api = mock();
    await expect(api.secrets.grant("global", "github-deploy", { grantee: "project:webshop", level: "use", env: "GH" })).rejects.toMatchObject({ status: 422 });
    await expect(api.secrets.grant("terrarium", "openai", { grantee: "project:terrarium", env: "PATH" })).rejects.toMatchObject({ status: 422 });
    await expect(api.secrets.grant("terrarium", "openai", { grantee: "project:webshop" })).rejects.toMatchObject({ status: 403 });
    await expect(api.secrets.grant("global", "gh-bot", { grantee: "project:nope" })).rejects.toMatchObject({ status: 404 });
    await expect(api.secrets.grant("global", "gh-bot", { grantee: "agent:terrarium/nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("refuses the delete of a target login with 409 and details.targets; force deletes it and the target shows a warning", async () => {
    const api = mock();
    await expect(api.secrets.remove("global", "buildbox-key")).rejects.toMatchObject({ status: 409, code: "conflict", details: { targets: ["build-box"] } });
    await api.secrets.remove("global", "buildbox-key", { force: true });
    expect((await api.secrets.list()).some((s) => s.name === "buildbox-key")).toBe(false);
    const t = (await api.targets.list()).find((x) => x.name === "build-box")!;
    expect(t.credential).toBe("buildbox-key");
    expect(t.credential_info).toMatchObject({ name: "buildbox-key", scope: null, kind: null });
    expect(t.credential_info!.warning).toMatch(/no secret `buildbox-key`/);
    await api.secrets.remove("terrarium", "openai");
    await expect(api.secrets.get("terrarium", "openai")).rejects.toMatchObject({ status: 404 });
  });
});

describe("target credentials (docs/api.md §17.2, PLAN §16b.6a)", () => {
  it("shows credential_info with the key's scope, kind, and fingerprint, and none on a target with no credential", async () => {
    const api = mock();
    const list = await api.targets.list();
    const bb = list.find((t) => t.name === "build-box")!;
    const key = (await api.secrets.list()).find((s) => s.name === "buildbox-key")!;
    expect(bb.credential_info).toEqual({ name: "buildbox-key", scope: "global", kind: "ssh_key", fingerprint: key.fields.fingerprint });
    expect(list.find((t) => t.name === "pg-test")).not.toHaveProperty("credential_info");
  });

  it("checks the credential at create: kind and scope; a project target can use its project's key or a global one", async () => {
    const api = mock();
    await expect(api.targets.create({ name: "s1", kind: "ssh", address: "a@b", credential: "win-vnc-pw" })).rejects.toMatchObject({ status: 422 });
    await expect(api.targets.create({ name: "s2", kind: "ssh", address: "a@b", credential: "lab-key" })).rejects.toMatchObject({ status: 422 });
    const t = await api.targets.create({ name: "s3", kind: "ssh", project: "terrarium", address: "a@b", credential: "lab-key" });
    expect(t.credential_info).toMatchObject({ scope: "terrarium", kind: "ssh_key" });
    expect((await api.targets.create({ name: "s4", kind: "ssh", project: "webshop", address: "a@b", credential: "buildbox-key" })).credential_info?.scope).toBe("global");
  });

  it("changes the credential with PATCH, removes it with null, and refuses a file target", async () => {
    const api = mock();
    const t = await api.targets.create({ name: "s5", kind: "ssh", project: "terrarium", address: "a@b" });
    expect((await api.targets.update(t.name, { credential: "lab-key" })).credential).toBe("lab-key");
    await expect(api.targets.update(t.name, { credential: "win-vnc-pw" })).rejects.toMatchObject({ status: 422 });
    const cleared = await api.targets.update(t.name, { credential: null });
    expect(cleared).not.toHaveProperty("credential");
    expect(cleared).not.toHaveProperty("credential_info");
    await expect(api.targets.update("build-box", { credential: null })).rejects.toMatchObject({ status: 409, code: "file_managed" });
  });

  it("tests an ssh target: the host key is added first, known after; a refused key, an unreachable host; 422 for another kind", async () => {
    const api = mock();
    const first = await api.targets.test("build-box");
    expect(first).toMatchObject({ target: "build-box", kind: "ssh", address: "build@buildbox.lan", result: "ok", host_key: "added", credential: "buildbox-key", exit_code: 0 });
    expect(Object.keys(first).sort()).toEqual(["address", "credential", "elapsed_ms", "exit_code", "fingerprint", "host_key", "host_key_policy", "kind", "message", "result", "stderr", "target"]);
    expect((await api.targets.test("build-box")).host_key).toBe("known");
    const refused = await api.targets.create({ name: "refuse-box", kind: "ssh", address: "u@refuse.lan", credential: "buildbox-key" });
    expect((await api.targets.test(refused.name)).result).toBe("auth_refused");
    const down = await api.targets.create({ name: "down-box", kind: "ssh", address: "u@down.lan", credential: "buildbox-key" });
    expect((await api.targets.test(down.name)).result).toBe("unreachable");
    await expect(api.targets.test("pg-test")).rejects.toMatchObject({ status: 422 });
  });
});

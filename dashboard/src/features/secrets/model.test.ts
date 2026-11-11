import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../../api/http";
import type { Secret, SecretKind } from "../../api/types";
import {
  authorizedKeysLine,
  createBody,
  createRevealHold,
  credentialCell,
  credentialChoices,
  defaultLevel,
  deleteConflict,
  emptyForm,
  emptyGrant,
  envAllowed,
  envError,
  filterSecrets,
  grantBody,
  granteeOptions,
  NEEDS_GRANT_HINT,
  plainFields,
  projectSecrets,
  putBody,
  replaceForm,
  revealError,
  REVEAL_MS,
  TOKEN_USE_NOTE,
  tokenKind,
  type SecretForm,
} from "./model";

const secret = (scope: string, name: string, kind: SecretKind, fields: Record<string, unknown> = {}, secret_fields = ["value"]): Secret => ({
  id: `${scope}-${name}`,
  scope,
  name,
  kind,
  fields,
  secret_fields,
  created_at: "2026-10-28T09:00:00.000Z",
  updated_at: "2026-10-28T09:00:00.000Z",
  created_by: "operator",
});

const form = (kind: SecretKind, values: Record<string, string>, extra: Partial<SecretForm> = {}): SecretForm => ({ ...emptyForm("global", kind), name: "s1", values, ...extra });

describe("the request body of each kind", () => {
  it("api_key: the value and the optional env, empty optionals left out", () => {
    expect(createBody(form("api_key", { value: "sk-1", env: " OPENAI_API_KEY " }))).toEqual({ body: { scope: "global", name: "s1", kind: "api_key", fields: { value: "sk-1", env: "OPENAI_API_KEY" } } });
    expect(createBody(form("api_key", { value: "sk-1", env: "  " }))).toEqual({ body: { scope: "global", name: "s1", kind: "api_key", fields: { value: "sk-1" } } });
  });

  it("token: hosts as a list from a comma list", () => {
    const b = createBody(form("token", { value: "t", hosts: "github.com, gitlab.lan ,", username: "" }));
    expect(b).toEqual({ body: { scope: "global", name: "s1", kind: "token", fields: { value: "t", hosts: ["github.com", "gitlab.lan"] } } });
  });

  it("github_token and login: the secret field keeps its spaces; plain fields are trimmed", () => {
    expect(createBody(form("github_token", { value: "ghp", host: " ghe.lan " }))).toMatchObject({ body: { fields: { value: "ghp", host: "ghe.lan" } } });
    expect(createBody(form("login", { username: " admin ", password: " pass word " }))).toMatchObject({ body: { kind: "login", fields: { username: "admin", password: " pass word " } } });
  });

  it("ssh_key: the private key and the passphrase; never a public key", () => {
    const b = createBody(form("ssh_key", { private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n-----END OPENSSH PRIVATE KEY-----\n" }));
    expect("body" in b && Object.keys(b.body.fields)).toEqual(["private_key"]);
  });

  it("generic: each row secret or plain, with secret_fields", () => {
    const f = form("generic", {}, {
      rows: [
        { key: "user", value: " bob ", secret: false },
        { key: "pin", value: "1234", secret: true },
        { key: "", value: "", secret: true },
      ],
    });
    expect(createBody(f)).toEqual({ body: { scope: "global", name: "s1", kind: "generic", fields: { user: "bob", pin: "1234" }, secret_fields: ["pin"] } });
  });

  it("refuses a missing required field, a bad name, a bad or doubled field name", () => {
    expect(createBody(form("api_key", {}))).toEqual({ error: "Value is required." });
    expect(createBody({ ...form("api_key", { value: "x" }), name: "Bad Name" })).toHaveProperty("error");
    expect(createBody(form("generic", {}, { rows: [{ key: "9x", value: "1", secret: true }] }))).toHaveProperty("error");
    expect(createBody(form("generic", {}, { rows: [{ key: "a", value: "1", secret: true }, { key: "a", value: "2", secret: false }] }))).toHaveProperty("error");
    expect(createBody(form("generic", {}, { rows: [{ key: "", value: "", secret: true }] }))).toEqual({ error: "Add at least one field." });
  });

  it("a replace: the body has the kind and the fields, no scope or name; the form keeps the plain fields only", () => {
    const s = secret("ops", "door", "generic", { user: "bob" }, ["pin"]);
    const f = replaceForm(s);
    expect(f.rows).toEqual([{ key: "user", value: "bob", secret: false }, { key: "pin", value: "", secret: true }]);
    f.rows[1]!.value = "9999";
    expect(putBody(f)).toEqual({ body: { kind: "generic", fields: { user: "bob", pin: "9999" }, secret_fields: ["pin"] } });
    const t = replaceForm(secret("global", "gh", "token", { username: "x-access-token", hosts: ["github.com", "ghe.lan"] }));
    expect(t.values).toEqual({ username: "x-access-token", hosts: "github.com, ghe.lan" });
  });
});

describe("grants", () => {
  it("defaults to use for the broker kinds and read for the others", () => {
    expect((["token", "github_token", "ssh_key"] as const).map(defaultLevel)).toEqual(["use", "use", "use"]);
    expect((["api_key", "login", "generic"] as const).map(defaultLevel)).toEqual(["read", "read", "read"]);
    expect(emptyGrant({ kind: "ssh_key" }).level).toBe("use");
  });

  it("allows an env var only with read, and drops it from the body otherwise", () => {
    expect(envAllowed("read")).toBe(true);
    expect(envAllowed("use")).toBe(false);
    expect(grantBody({ grantee: "project:ops", level: "use", env: "DB" })).toEqual({ body: { grantee: "project:ops", level: "use" } });
    expect(grantBody({ grantee: "agent:ops/b", level: "read", env: " DATABASE_URL " })).toEqual({ body: { grantee: "agent:ops/b", level: "read", env: "DATABASE_URL" } });
    expect(grantBody({ grantee: "", level: "read", env: "" })).toHaveProperty("error");
  });

  it("refuses an env name that is not POSIX or that the sandbox sets", () => {
    expect(envError("1X")).not.toBeNull();
    expect(envError("PATH")).not.toBeNull();
    expect(envError("TERRARIUM_X")).not.toBeNull();
    expect(envError("LD_PRELOAD")).not.toBeNull();
    expect(envError("GH_TOKEN")).toBeNull();
    expect(envError("DB_URL")).toBeNull();
    // TER-577: the deny prefixes of TER-541 (OPENSSL_, KRB5, DBUS_).
    for (const bad of ["db_url", "_X", "NODE_OPTIONS", "HTTPS_PROXY", "GIT_DIR", "BASH_ENV", "OPENAI_API_KEY", "PYTHONPATH", "KRB5CCNAME", "KRB5_CONFIG", "DBUS_SESSION_BUS_ADDRESS", "OPENSSL_CONF", "OPENSSL_MODULES"])
      expect(envError(bad), bad).not.toBeNull();
    expect(grantBody({ grantee: "project:ops", level: "read", env: "HOME" })).toHaveProperty("error");
  });

  it("offers any project or agent for a global secret, and the project and its agents for a project secret", () => {
    const projects = [{ name: "ops" }, { name: "web" }];
    const agents = [{ project: "web", name: "z" }, { project: "ops", name: "b" }];
    expect(granteeOptions({ scope: "global" }, projects, agents).map((o) => o.value)).toEqual(["project:ops", "project:web", "agent:ops/b", "agent:web/z"]);
    expect(granteeOptions({ scope: "ops" }, projects, agents).map((o) => o.value)).toEqual(["project:ops", "agent:ops/b"]);
  });
});

describe("the list", () => {
  const list = [secret("global", "gh", "token"), secret("global", "db", "generic"), secret("ops", "db", "generic"), secret("web", "k", "api_key")];

  it("filters by scope: all, global, or one project", () => {
    expect(filterSecrets(list, "all")).toHaveLength(4);
    expect(filterSecrets(list, "global").map((s) => s.name)).toEqual(["gh", "db"]);
    expect(filterSecrets(list, "ops").map((s) => s.scope)).toEqual(["ops"]);
  });

  it("shows a project's own secrets first, then the global ones, marked; a project secret hides the global one of its name", () => {
    const rows = projectSecrets(list, "ops");
    expect(rows.map((r) => [r.secret.scope, r.secret.name, r.global, r.hidden])).toEqual([
      ["ops", "db", false, false],
      ["global", "db", true, true],
      ["global", "gh", true, false],
    ]);
  });

  it("shows the plain fields without the public key and the fingerprint, and the authorized_keys line", () => {
    const k = secret("global", "deploy", "ssh_key", { public_key: "ssh-ed25519 AAA c", fingerprint: "SHA256:x" }, ["private_key"]);
    expect(plainFields(k)).toEqual([]);
    expect(plainFields(secret("global", "gh", "token", { hosts: ["a", "b"] }))).toEqual([["hosts", "a, b"]]);
    expect(authorizedKeysLine("ssh-ed25519 AAA it's")).toBe("echo 'ssh-ed25519 AAA it'\\''s' >> ~/.ssh/authorized_keys");
  });
});

describe("target credentials", () => {
  const list = [
    secret("global", "bb", "ssh_key", { fingerprint: "SHA256:aaaaaaaaaaaaaaaaaaaaaaa" }, ["private_key"]),
    secret("ops", "bb", "ssh_key", { fingerprint: "SHA256:bbbbbbbbbbbbbbbbbbbbbbb" }, ["private_key"]),
    secret("ops", "lab", "ssh_key", {}, ["private_key"]),
    secret("web", "web-key", "ssh_key", {}, ["private_key"]),
    secret("global", "vnc", "login", { username: "a" }, ["password"]),
  ];

  it("an ssh project target: its project's keys and the global ones; the project key wins a name", () => {
    const c = credentialChoices(list, "ssh", "ops");
    expect(c.map((x) => `${x.scope}/${x.name}`)).toEqual(["ops/bb", "ops/lab"]);
    expect(c[0]!.fingerprint).toBe("SHA256:bbbbbbbbbbbbbbbbbbbbbbb");
  });

  it("a project target: a global secret with no grant to the project keeps its place, with the hint (PLAN §16b.6a)", () => {
    const c = credentialChoices([...list, secret("global", "gk", "ssh_key", {}, ["private_key"])], "ssh", "ops", new Set(["bb"]));
    const gk = c.find((x) => x.name === "gk")!;
    expect(gk.needsGrant).toBe(true);
    expect(gk.label).toContain(NEEDS_GRANT_HINT);
    // A granted global secret, a project secret, and a global target: no hint.
    const granted = credentialChoices([...list, secret("global", "gk", "ssh_key", {}, ["private_key"])], "ssh", "ops", new Set(["gk"]));
    expect(granted.some((x) => x.needsGrant)).toBe(false);
    expect(credentialChoices(list, "ssh", "global", new Set()).some((x) => x.needsGrant)).toBe(false);
    // No grant list yet: no hint.
    expect(credentialChoices(list, "rdp", "web").some((x) => x.needsGrant)).toBe(false);
  });

  it("use = read for a token or a github_token; an ssh_key stays hidden (PLAN §16b.3)", () => {
    expect(tokenKind("token")).toBe(true);
    expect(tokenKind("github_token")).toBe(true);
    expect(tokenKind("ssh_key")).toBe(false);
    expect(TOKEN_USE_NOTE).toContain("use = read for tokens");
  });

  it("a global target: the global secrets only; vnc and rdp take logins; other kinds take none", () => {
    expect(credentialChoices(list, "ssh", "global").map((x) => `${x.scope}/${x.name}`)).toEqual(["global/bb"]);
    expect(credentialChoices(list, "rdp", "web").map((x) => x.name)).toEqual(["vnc"]);
    expect(credentialChoices(list, "docker", "ops")).toEqual([]);
  });

  it("shows the key name and the fingerprint in the list cell", () => {
    expect(credentialCell({ credential: "bb", credential_info: { name: "bb", scope: "global", kind: "ssh_key", fingerprint: "SHA256:x" } })).toBe("bb · SHA256:x");
    expect(credentialCell({})).toBeNull();
  });
});

describe("errors", () => {
  it("reads the targets of a refused delete", () => {
    expect(deleteConflict(new ApiError(409, "used", undefined, "conflict", { targets: ["box"] }))).toEqual(["box"]);
    expect(deleteConflict(new ApiError(404, "no", undefined, "not_found"))).toBeNull();
  });

  it("says why a reveal failed", () => {
    expect(revealError(new ApiError(403, "the operator password is wrong", undefined, "forbidden"))).toMatch(/password is wrong/);
    expect(revealError(new ApiError(403, "a reveal is for the operator only", undefined, "forbidden"))).toMatch(/Only the operator/);
    expect(revealError(new ApiError(409, "no password", undefined, "conflict"))).toMatch(/No operator password is set/);
    expect(revealError(new ApiError(429, "slow down", undefined, "rate_limited"))).toMatch(/Too many/);
  });
});

describe("the reveal timer", () => {
  afterEach(() => vi.useRealTimers());

  it("shows the values for 30 seconds, counts down, then drops them", () => {
    vi.useFakeTimers();
    const seen: ({ values: Record<string, string>; left: number } | null)[] = [];
    const hold = createRevealHold((v) => seen.push(v));
    hold.show({ value: "s3cret" });
    expect(seen.at(-1)).toEqual({ values: { value: "s3cret" }, left: 30 });
    expect(hold.active()).toBe(true);
    vi.advanceTimersByTime(29_000);
    expect(seen.at(-1)).toEqual({ values: { value: "s3cret" }, left: 1 });
    vi.advanceTimersByTime(1_000);
    expect(seen.at(-1)).toBeNull();
    expect(hold.active()).toBe(false);
    expect(REVEAL_MS).toBe(30_000);
  });

  it("hides at once on hide, and a second show starts the time again", () => {
    vi.useFakeTimers();
    const seen: unknown[] = [];
    const hold = createRevealHold((v) => seen.push(v));
    hold.show({ a: "1" });
    vi.advanceTimersByTime(20_000);
    hold.show({ a: "2" });
    vi.advanceTimersByTime(20_000);
    expect(seen.at(-1)).toEqual({ values: { a: "2" }, left: 10 });
    hold.hide();
    expect(seen.at(-1)).toBeNull();
    vi.advanceTimersByTime(60_000);
    expect(seen.at(-1)).toBeNull();
  });
});

// The secret manager of the mock (PLAN §16b, docs/api.md §5.2), after the
// daemon's `api/secrets.rs`, `api/secrets_generate.rs`, and
// `api/secret_grants.rs`: the kinds and their fields, the scope rights, the
// reveal with the operator password, the SSH key generate, the grants, and
// the delete guard of a target login (`409` with `details.targets`, unless
// `force`). It also serves the target side of PLAN §16b.6a: the
// `credential_info` of a target view, the save-time check of a target
// `credential`, and `POST /api/targets/{name}/test`.
//
// The state lives in this module's closure, not in `MockWorld`. The values
// stay here; no answer holds one, except a reveal.

import type { Api } from "../client";
import type { Agent, CredentialInfo, GrantLevel, NewSecret, RevealedSecret, Secret, SecretGrant, SecretKind, Target, TargetKind, TargetTestReport } from "../types";
import { envNameError } from "../envNames";
import { SECRET_KINDS } from "../types";
import type { MockWorld } from "./data";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface SecretMockContext {
  w: MockWorld;
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** The operator password; null: none is set (a reveal is `409`). */
  password: () => string | null;
  caller?: ScheduleCaller;
}

export const GLOBAL = "global";
const NAME_RE = /^[a-z0-9_-]{1,63}$/;
const ULID_RE = /^[0-9A-Z]{26}$/;
const ENV_RE = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;

/** The fields of each kind: the secret ones, and the plain ones (docs/api.md §5.2). `generic` takes any. */
const KIND_FIELDS: Record<Exclude<SecretKind, "generic">, { secret: string[]; required: string; plain: string[] }> = {
  api_key: { secret: ["value"], required: "value", plain: ["env"] },
  token: { secret: ["value"], required: "value", plain: ["username", "hosts"] },
  github_token: { secret: ["value"], required: "value", plain: ["host"] },
  login: { secret: ["password"], required: "password", plain: ["username", "url", "domain"] },
  ssh_key: { secret: ["private_key", "passphrase"], required: "private_key", plain: [] },
};

/** The default level of a grant: `use` for the kinds that a broker serves, else `read`. */
export const defaultGrantLevel = (k: SecretKind): GrantLevel => (k === "token" || k === "github_token" || k === "ssh_key" ? "use" : "read");

/** The secret kind that the `credential` of a target kind names (PLAN §16b.6a). */
export const credentialKind = (k: TargetKind): SecretKind | null => (k === "ssh" ? "ssh_key" : k === "vnc" || k === "rdp" ? "login" : null);

interface Row {
  info: Secret;
  values: Record<string, string>;
}
interface GrantRow extends SecretGrant {
  secret: string;
}

/** A stable fake base64 text of `n` characters from `seed`. */
function fakeB64(seed: string, n: number): string {
  const abc = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let h = 2166136261;
  for (let i = 0; i < seed.length; i++) h = Math.imul(h ^ seed.charCodeAt(i), 16777619) >>> 0;
  let out = "";
  for (let i = 0; i < n; i++) {
    h = Math.imul(h ^ (h >>> 13), 0x5bd1e995) >>> 0;
    h = (h + i * 2654435761) >>> 0;
    out += abc[h % 64];
  }
  return out;
}

/** The public key and fingerprint of a (fake) key pair. */
function keyPair(type: "ed25519" | "rsa", seed: string, comment: string) {
  const blob = type === "rsa" ? `AAAAB3NzaC1yc2EAAAADAQABAAABgQ${fakeB64(seed, 160)}` : `AAAAC3NzaC1lZDI1NTE5AAAAI${fakeB64(seed, 43)}`;
  return {
    public_key: `${type === "rsa" ? "ssh-rsa" : "ssh-ed25519"} ${blob}${comment ? ` ${comment}` : ""}`,
    fingerprint: `SHA256:${fakeB64(`fp:${seed}`, 43)}`,
  };
}

const fakePrivateKey = (seed: string) => `-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ${fakeB64(seed, 120)}\n-----END OPENSSH PRIVATE KEY-----\n`;

/** The seed: one secret of each kind, in the global scope and in the two projects. */
function seed(now: number): Row[] {
  const at = (min: number) => new Date(now - min * 60_000).toISOString();
  const row = (scope: string, name: string, kind: SecretKind, fields: Record<string, unknown>, values: Record<string, string>, min: number): Row => ({
    info: { id: `01JBSEC${fakeB64(`${scope}/${name}`, 19).replace(/[^A-Z0-9]/gi, "0").toUpperCase()}`, scope, name, kind, fields, secret_fields: Object.keys(values), created_at: at(min), updated_at: at(min / 2), created_by: "operator" },
    values,
  });
  const bb = keyPair("ed25519", "global/buildbox-key", "terrarium:global/buildbox-key");
  const lab = keyPair("ed25519", "terrarium/lab-key", "terrarium:terrarium/lab-key");
  return [
    row(GLOBAL, "github-deploy", "token", { username: "x-access-token", hosts: ["github.com"] }, { value: "ghp_mockDeployToken0000000000000000" }, 60 * 24 * 6),
    row(GLOBAL, "buildbox-key", "ssh_key", bb, { private_key: fakePrivateKey("bb") }, 60 * 24 * 5),
    row(GLOBAL, "win-vnc-pw", "login", { username: "Administrator" }, { password: "mock-vnc-password" }, 60 * 24 * 4),
    row(GLOBAL, "gh-bot", "github_token", { host: "github.com" }, { value: "github_pat_mockBotToken000000000000" }, 60 * 24 * 3),
    row("terrarium", "openai", "api_key", { env: "OPENAI_API_KEY" }, { value: "sk-mock-openai-key-0000000000" }, 60 * 24 * 2),
    row("terrarium", "lab-key", "ssh_key", lab, { private_key: fakePrivateKey("lab") }, 60 * 30),
    row("webshop", "db", "generic", { user: "shop", host: "db.lan" }, { password: "mock-db-password" }, 60 * 12),
  ];
}

/** The `Api["secrets"]` of the mock, with the target hooks of PLAN §16b.6a. */
export function createSecretMock(c: SecretMockContext) {
  const { w, fail } = c;
  const rows: Row[] = seed(Date.now());
  const grants: GrantRow[] = [];
  let failures = 0;
  const caller = () => c.caller ?? { role: "operator" as const };
  const get = (scope: string, name: string) => rows.find((r) => r.info.scope === scope && r.info.name === name);
  const view = (r: Row): Secret => structuredClone(r.info);
  const project = (scope: string) => scope === GLOBAL || w.projects.some((p) => p.name === scope) || fail(404, "not_found", `no project \`${scope}\``);
  /** `policy::manage_secrets`: global needs a sysadmin or the operator; a project, its project_admin too. */
  const manage = (scope: string) => {
    project(scope);
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if (scope === GLOBAL) fail(403, "forbidden", "a global secret needs sysadmin or the operator");
    if (who.role !== "project_admin" || who.project !== scope) fail(403, "forbidden", `the secrets of \`${scope}\` need project_admin of \`${scope}\`, sysadmin, or the operator`);
  };
  const find = (scope: string, name: string): Row => {
    manage(scope);
    return get(scope, name) ?? fail(404, "not_found", `no secret \`${scope}/${name}\``);
  };
  const auditTarget = (r: Pick<Secret, "scope" | "name">) => `${r.scope}/${r.name}`;
  const auditProject = (scope: string) => (scope === GLOBAL ? undefined : scope);

  /** The daemon's check of `fields`: the plain fields and the secret values, by kind. */
  const split = (kind: SecretKind, fields: Record<string, unknown>, secretFields: string[] = []) => {
    if (!SECRET_KINDS.includes(kind)) fail(422, "invalid", `unknown kind \`${String(kind)}\``);
    const plain: Record<string, unknown> = {};
    const values: Record<string, string> = {};
    const bad = (f: string, why: string): never => fail(422, "invalid", `field \`${f}\`: ${why}`, { field: f });
    const plainText = (f: string, v: unknown) => {
      if (typeof v !== "string" || !v || v.length > 1024 || /\p{Cc}/u.test(v)) bad(f, "1 to 1024 bytes with no control characters");
      return v as string;
    };
    const secretText = (f: string, v: unknown) => {
      if (typeof v !== "string" || !v || v.length > 64 * 1024) bad(f, "1 to 64 KiB");
      return v as string;
    };
    if (kind === "generic") {
      const names = Object.keys(fields);
      if (!names.length) fail(422, "invalid", "a generic secret needs at least one field");
      if (names.length > 32) fail(422, "invalid", "a generic secret has at most 32 fields");
      for (const s of secretFields) if (!names.includes(s)) fail(422, "invalid", `secret_fields names \`${s}\`, which is not in fields`);
      for (const [k, v] of Object.entries(fields)) {
        if (!/^[A-Za-z_][A-Za-z0-9_.-]{0,63}$/.test(k)) bad(k, "a field name is 1-64 of A-Z, a-z, 0-9, `_`, `-`, `.`, starting with a letter or `_`");
        if (secretFields.includes(k)) values[k] = secretText(k, v);
        else plain[k] = plainText(k, v);
      }
      return { plain, values };
    }
    if (secretFields.length) fail(422, "invalid", "`secret_fields` is for a generic secret only");
    const spec = KIND_FIELDS[kind];
    for (const [k, v] of Object.entries(fields)) {
      if (spec.secret.includes(k)) values[k] = secretText(k, v);
      else if (spec.plain.includes(k)) plain[k] = k === "hosts" ? hosts(v) : plainText(k, v);
      else if (kind === "ssh_key" && (k === "public_key" || k === "fingerprint")) fail(422, "invalid", `\`${k}\`: the daemon derives it from the private key`);
      else fail(422, "invalid", `a ${kind} secret has no field \`${k}\``);
    }
    if (!values[spec.required]) fail(422, "invalid", `a ${kind} secret needs \`${spec.required}\``, { field: spec.required });
    if (kind === "api_key" || kind === "token" || kind === "github_token") {
      const v = values.value!.trim();
      if (/[\r\n]/.test(v)) bad("value", "one line");
      values.value = v;
    }
    if (kind === "api_key" && plain.env !== undefined && !ENV_RE.test(plain.env as string)) bad("env", "an env var name");
    if (kind === "token") plain.username ??= "x-access-token";
    if (kind === "github_token") plain.host ??= "github.com";
    if (kind === "login" && (/[\r\n]/.test(values.password!) || values.password!.length > 1024)) bad("password", "one line of 1 to 1024 bytes");
    return { plain, values };
  };
  const hosts = (v: unknown): string[] => {
    const list = Array.isArray(v) ? v : typeof v === "string" ? v.split(",") : fail(422, "invalid", "field `hosts`: a list of git host names");
    const out = list.map((h) => String(h).trim()).filter(Boolean);
    if (out.some((h) => !/^[A-Za-z0-9.-]+(:\d+)?$/.test(h))) fail(422, "invalid", "field `hosts`: a list of git host names");
    return out;
  };
  /** An `ssh_key`: the mock takes an OpenSSH or an unencrypted PEM key, and derives the public key. */
  const sshKey = (scope: string, name: string, values: Record<string, string>) => {
    const k = values.private_key!;
    if (!/-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(k)) fail(422, "invalid", "field `private_key`: not a private key in the OpenSSH or PEM format", { field: "private_key" });
    if (/ENCRYPTED/.test(k)) fail(422, "invalid", "field `private_key`: a PEM key with a passphrase: write it again in the OpenSSH format with `ssh-keygen -p -f <file>`", { field: "private_key" });
    const type = /RSA/.test(k) ? "rsa" : "ed25519";
    return keyPair(type, k, `terrarium:${scope}/${name}`);
  };

  const store = (mode: "create" | "upsert", scope: string, rawName: unknown, kind: SecretKind, fields: Record<string, unknown> = {}, secretFields: string[] = []): { info: Secret; created: boolean } => {
    manage(scope);
    const name = typeof rawName === "string" ? rawName : "";
    if (!NAME_RE.test(name)) fail(422, "invalid", `bad secret name \`${String(rawName)}\`: use 1-63 of a-z, 0-9, \`-\`, \`_\``, { field: "name" });
    const old = get(scope, name);
    if (old && mode === "create") fail(409, "conflict", `secret \`${scope}/${name}\` exists`);
    const { plain, values } = split(kind, fields, secretFields);
    if (kind === "ssh_key") Object.assign(plain, sshKey(scope, name, values));
    const now = c.iso();
    const info: Secret = { id: old?.info.id ?? c.ulid("sec"), scope, name, kind, fields: plain, secret_fields: Object.keys(values), created_at: old?.info.created_at ?? now, updated_at: now, created_by: old?.info.created_by ?? "operator" };
    if (old) Object.assign(old, { info, values });
    else rows.push({ info, values });
    c.audit("secret.put", auditTarget(info), auditProject(scope), { scope, name, kind, created: !old, plain_fields: Object.keys(plain), secret_fields: info.secret_fields });
    return { info: structuredClone(info), created: !old };
  };

  /** The targets that use `scope/name` as their credential (`targets_using_secret`). */
  const usedBy = (scope: string, name: string) =>
    w.targets
      .filter((t) => t.credential === name && (!t.project ? scope === GLOBAL : scope === GLOBAL ? !get(t.project, name) : t.project === scope))
      .map((t) => t.name);

  /** `agent:<project>/<name>`, `agent:<id>`, or `project:<name>`, as the daemon stores and shows it. */
  const grantee = (raw: unknown): { grantee: string; label: string; project: string } => {
    if (typeof raw !== "string") return fail(422, "invalid", "`grantee`: use agent:<project>/<name>, agent:<id>, or project:<name>");
    if (raw.startsWith("project:")) {
      const p = raw.slice(8);
      if (!w.projects.some((x) => x.name === p)) fail(404, "not_found", `no project \`${p}\``);
      return { grantee: raw, label: raw, project: p };
    }
    if (raw.startsWith("agent:")) {
      const key = raw.slice(6);
      let a: Agent | undefined;
      if (ULID_RE.test(key)) a = w.agents.find((x) => x.id === key);
      else if (key.includes("/")) {
        const [p, n] = key.split("/", 2);
        a = w.agents.find((x) => x.project === p && x.name === n);
      } else fail(422, "invalid", "`grantee`: an agent is agent:<project>/<name> or agent:<id>");
      if (!a) fail(404, "not_found", `no agent \`${key}\``);
      return { grantee: `agent:${a!.id}`, label: `agent:${a!.project}/${a!.name}`, project: a!.project };
    }
    return fail(422, "invalid", "`grantee`: use agent:<project>/<name>, agent:<id>, or project:<name>");
  };
  /** `policy::grant_secret`: the rights of a manage. */
  const mayGrant = manage;

  const api: Api["secrets"] = {
    list: (q = {}) =>
      c.reply(() => {
        const who = caller();
        let scope: string | undefined = q.scope;
        if (scope) manage(scope);
        else if (who.role === "project_admin") scope = who.project;
        else if (who.role === "member") fail(403, "forbidden", "listing secrets needs project_admin, sysadmin, or the operator");
        return rows
          .filter((r) => !scope || r.info.scope === scope)
          .sort((a, b) => (a.info.scope === GLOBAL ? -1 : 0) - (b.info.scope === GLOBAL ? -1 : 0) || a.info.scope.localeCompare(b.info.scope) || a.info.name.localeCompare(b.info.name))
          .map(view);
      }),
    get: (scope, name) => c.reply(() => view(find(scope, name))),
    create: (body: NewSecret) => c.reply(() => store("create", body.scope ?? GLOBAL, body.name, body.kind, body.fields, body.secret_fields).info),
    put: (scope, name, body) => c.reply(() => store("upsert", scope, name, body.kind, body.fields, body.secret_fields).info),
    remove: (scope, name, o = {}) =>
      c.reply(() => {
        const r = find(scope, name);
        const users = usedBy(scope, name);
        if (users.length && !o.force)
          fail(409, "conflict", `secret \`${scope}/${name}\` is the credential of target ${users.map((u) => `\`${u}\``).join(", ")}; change the target first, or delete with \`?force=true\` (the target then keeps a name with no secret)`, { targets: users });
        rows.splice(rows.indexOf(r), 1);
        for (let i = grants.length - 1; i >= 0; i--) if (grants[i]!.secret === r.info.id) grants.splice(i, 1);
        c.audit("secret.delete", auditTarget(r.info), auditProject(scope), { scope, name, kind: r.info.kind, targets: users });
      }),
    generate: (scope, name, body = {}) =>
      c.reply(() => {
        manage(scope);
        const type = body.type ?? "ed25519";
        if (type !== "ed25519" && type !== "rsa") fail(422, "invalid", "`type`: use ed25519 or rsa");
        if (body.bits !== undefined && (type !== "rsa" || (body.bits !== 3072 && body.bits !== 4096))) fail(422, "invalid", "`bits` is for rsa only: 3072 or 4096");
        if (typeof name !== "string" || !NAME_RE.test(name)) fail(422, "invalid", `bad secret name \`${String(name)}\``, { field: "name" });
        const old = get(scope, name);
        if (old && !body.replace) fail(409, "conflict", `secret \`${scope}/${name}\` exists; send \`"replace": true\` to make a new key`);
        const comment = body.comment ?? `terrarium:${scope}/${name}`;
        const pair = keyPair(type, `${scope}/${name}/${c.ulid("key")}`, comment);
        const now = c.iso();
        const info: Secret = { id: old?.info.id ?? c.ulid("sec"), scope, name, kind: "ssh_key", fields: pair, secret_fields: ["private_key"], created_at: old?.info.created_at ?? now, updated_at: now, created_by: "operator" };
        const values = { private_key: fakePrivateKey(pair.fingerprint) };
        if (old) Object.assign(old, { info, values });
        else rows.push({ info, values });
        c.audit("secret.put", auditTarget(info), auditProject(scope), { scope, name, kind: "ssh_key", created: !old, generated: true, type, bits: type === "rsa" ? (body.bits ?? 3072) : null, fingerprint: pair.fingerprint });
        return structuredClone(info);
      }),
    reveal: (scope, name, password) =>
      c.reply((): RevealedSecret => {
        if (caller().role !== "operator") fail(403, "forbidden", "a reveal is for the operator only");
        if (typeof password !== "string" || !password) fail(422, "invalid", "a reveal needs the operator password as `password`");
        const set = c.password();
        if (set === null) fail(409, "conflict", "no operator password is set: set one with `terrariumd password set`");
        if (failures >= 5) fail(429, "rate_limited", "too many wrong passwords; wait 30 s");
        if (password !== set) {
          failures++;
          fail(403, "forbidden", "the operator password is wrong");
        }
        failures = 0;
        const r = get(scope, name) ?? fail(404, "not_found", `no secret \`${scope}/${name}\``);
        c.audit("secret.reveal", auditTarget(r.info), auditProject(scope), { scope, name, kind: r.info.kind, secret_fields: r.info.secret_fields, ip: "127.0.0.1" });
        return { ...view(r), values: { ...r.values } };
      }),
    grants: (scope, name) =>
      c.reply(() => {
        mayGrant(scope);
        const r = find(scope, name);
        return grants.filter((g) => g.secret === r.info.id).map(({ secret: _s, ...g }) => structuredClone(g));
      }),
    grant: (scope, name, body) =>
      c.reply((): SecretGrant => {
        mayGrant(scope);
        const r = find(scope, name);
        const who = grantee(body.grantee);
        if (scope !== GLOBAL && who.project !== scope)
          fail(403, "forbidden", `secret \`${scope}/${name}\` is a secret of project \`${scope}\`; it goes only to that project or its agents, not to ${who.label}`);
        const level = body.level ?? defaultGrantLevel(r.info.kind);
        if (level !== "use" && level !== "read") fail(422, "invalid", "`level`: use `use` or `read`");
        const env = body.env || undefined;
        if (env !== undefined) {
          if (level !== "read") fail(422, "invalid", "`env` puts the value into the agent's environment, so it needs the level `read`", { field: "env" });
          const why = envNameError(env);
          if (why) fail(422, "invalid", `\`env\`: ${why}`, { field: "env" });
        }
        const old = grants.find((g) => g.secret === r.info.id && g.grantee === who.grantee);
        const g: GrantRow = { secret: r.info.id, scope, name, grantee: who.grantee, grantee_label: who.label, level, ...(env ? { env } : {}), created_by: "operator", created_at: c.iso() };
        if (old) grants.splice(grants.indexOf(old), 1, g);
        else grants.push(g);
        c.audit("secret.grant", auditTarget(r.info), auditProject(scope), { scope, name, kind: r.info.kind, grantee: g.grantee, grantee_label: g.grantee_label, level, env: env ?? null, created: !old });
        const { secret: _s, ...out } = g;
        return out;
      }),
    revoke: (scope, name, raw) =>
      c.reply(() => {
        mayGrant(scope);
        const r = find(scope, name);
        const who = grantee(raw);
        const i = grants.findIndex((g) => g.secret === r.info.id && g.grantee === who.grantee);
        if (i < 0) fail(404, "not_found", `no grant of \`${scope}/${name}\` to ${who.label}`);
        grants.splice(i, 1);
        c.audit("secret.revoke", auditTarget(r.info), auditProject(scope), { scope, name, kind: r.info.kind, grantee: who.grantee });
      }),
  };

  /** The secret that the target's `credential` names: the project's first, then the global one. */
  const credentialSecret = (t: Pick<Target, "credential" | "project">) => (t.credential ? ((t.project ? get(t.project, t.credential) : undefined) ?? get(GLOBAL, t.credential)) : undefined);
  /** Why the credential does not work (`credential_problem`), or null. */
  const credentialProblem = (t: Pick<Target, "credential" | "project" | "kind">): string | null => {
    const want = credentialKind(t.kind);
    if (!t.credential || !want) return null;
    const s = credentialSecret(t);
    if (!s) {
      let m = `credential \`${t.credential}\`: there is no secret \`${t.credential}\` (${t.project ? `project \`${t.project}\` or global` : "global"})`;
      const others = rows.filter((r) => r.info.name === t.credential && r.info.scope !== GLOBAL).map((r) => `\`${r.info.scope}\``);
      if (!t.project && others.length) m += `; a global target can use a global secret only, and \`${t.credential}\` is a secret of project ${others.join(", ")}`;
      return `${m}; ${t.kind} targets need kind ${want}`;
    }
    return s.info.kind === want ? null : `credential \`${t.credential}\`: secret \`${s.info.scope}/${t.credential}\` is of kind ${s.info.kind}; ${t.kind} targets need kind ${want}`;
  };
  const knownHosts = new Set<string>();

  return {
    api,
    /**
     * `credential_info` of a target view; undefined with no `credential`.
     * The mock's viewer is the operator, so each target and repo of the mock
     * counts as set by an admin (`credential_admin`, PLAN §16b.6a): a global
     * secret needs no grant, and the daemon's "has no grant" warning never
     * shows here.
     */
    credentialInfo(t: Target): CredentialInfo | undefined {
      if (!t.credential) return undefined;
      const s = credentialSecret(t);
      const warning = credentialProblem(t);
      const fp = s?.info.kind === "ssh_key" ? (s.info.fields.fingerprint as string | undefined) : undefined;
      return { name: t.credential, scope: s?.info.scope ?? null, kind: s?.info.kind ?? null, ...(fp ? { fingerprint: fp } : {}), ...(warning ? { warning } : {}) };
    },
    /** The save-time check (`check_credential`): `422` on a credential that does not work. A change that keeps kind, project, and credential passes. */
    checkCredential(def: Pick<Target, "credential" | "project" | "kind">, before?: Pick<Target, "credential" | "project" | "kind">) {
      if (before && before.kind === def.kind && before.project === def.project && before.credential === def.credential) return;
      const m = credentialProblem(def);
      if (m) fail(422, "invalid", m, { field: "credential" });
    },
    /** `POST /api/targets/{name}/test`. The mock picks the result from the address: `refuse`, `mismatch`, `unknown`, `down`, or `fail` in it. */
    test(t: Target): TargetTestReport {
      if (t.kind !== "ssh") fail(422, "invalid", `target \`${t.name}\` is a ${t.kind} target: a test is not supported; use the status verb`);
      const problem = credentialProblem(t);
      if (problem) fail(422, "invalid", problem, { field: "credential" });
      const a = t.address ?? "";
      const first = !knownHosts.has(a);
      const result: TargetTestReport["result"] = /refuse/.test(a) ? "auth_refused" : /mismatch/.test(a) ? "host_key_mismatch" : /unknown/.test(a) ? "host_key_unknown" : /down/.test(a) ? "unreachable" : /fail/.test(a) ? "error" : "ok";
      const hostKey: TargetTestReport["host_key"] = result === "host_key_mismatch" ? "mismatch" : result === "host_key_unknown" ? "unknown" : result === "unreachable" ? "not_checked" : first ? "added" : "known";
      if (hostKey === "added") knownHosts.add(a);
      const MESSAGES: Record<TargetTestReport["result"], string> = {
        ok: "the login works",
        auth_refused: "the host refused the key: add the public key to ~/.ssh/authorized_keys of the user",
        host_key_mismatch: `the host key of \`${a}\` changed since the first connection: remove the old line with ssh-keygen -R <host> -f <state>/ssh/known_hosts`,
        host_key_unknown: "the host key is not known, and ssh did not record it",
        unreachable: `cannot reach \`${a}\`: connection refused, a timeout, no route, or no DNS name`,
        error: "ssh failed: read stderr",
      };
      const STDERR: Record<TargetTestReport["result"], string> = {
        ok: hostKey === "added" ? `Warning: Permanently added '${a.split("@").pop()}' (ED25519) to the list of known hosts.` : "",
        auth_refused: `${a}: Permission denied (publickey).`,
        host_key_mismatch: "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @",
        host_key_unknown: "Host key verification failed.",
        unreachable: `ssh: connect to host ${a.split("@").pop()} port ${t.port ?? 22}: Connection refused`,
        error: "kex_exchange_identification: read: Connection reset by peer",
      };
      const info = this.credentialInfo(t);
      c.audit("target.test", t.name, t.project, { result, host_key: hostKey, exit_code: result === "ok" ? 0 : 255 });
      return {
        target: t.name, kind: "ssh", address: a, result, message: MESSAGES[result], host_key: hostKey,
        host_key_policy: "trust on first use (StrictHostKeyChecking=accept-new): the first connection records the host key in <state>/ssh/known_hosts",
        credential: t.credential ?? null, fingerprint: info?.fingerprint ?? null, exit_code: result === "ok" ? 0 : 255, stderr: STDERR[result], elapsed_ms: result === "unreachable" ? 10_000 : 212,
      };
    },
  };
}

export type SecretMock = ReturnType<typeof createSecretMock>;

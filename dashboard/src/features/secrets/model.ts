// The pure logic of the Secrets page (PLAN §16b, docs/api.md §5.2): the form
// of each kind and its request body, the default level of a grant, the env
// rule, the scope filter, the grantee choices, the credential choices of a
// target, the error sentences, and the reveal timer. No JSX; no value goes
// into a store or into storage.

import { ApiError } from "../../api/http";
import type { Agent, GrantLevel, NewSecret, NewSecretGrant, Secret, SecretKind, SecretPut, Target, TargetKind, TargetTestResult } from "../../api/types";

export const GLOBAL = "global";

/** One field of a typed kind. */
export interface FieldSpec {
  name: string;
  label: string;
  /** A secret field: a password input (or a textarea), never shown after a save. */
  secret: boolean;
  required: boolean;
  /** A textarea: a private key. */
  multiline?: boolean;
  placeholder?: string;
  help?: string;
}

export interface KindSpec {
  label: string;
  help: string;
  fields: FieldSpec[];
}

/** The fields of each typed kind (docs/api.md §5.2 "Kinds"). `generic` has rows of its own. */
export const KINDS: Record<SecretKind, KindSpec> = {
  api_key: {
    label: "API key",
    help: "One value. A grant with the level read can put it into an env var.",
    fields: [
      { name: "value", label: "Value", secret: true, required: true, placeholder: "sk-…" },
      { name: "env", label: "Suggested env var", secret: false, required: false, placeholder: "OPENAI_API_KEY" },
    ],
  },
  token: {
    label: "Token",
    help: "A git token. The git credential helper answers for its hosts.",
    fields: [
      { name: "username", label: "User name", secret: false, required: false, placeholder: "x-access-token" },
      { name: "value", label: "Token", secret: true, required: true },
      { name: "hosts", label: "Git hosts (a comma list)", secret: false, required: false, placeholder: "github.com, gitlab.example.com" },
    ],
  },
  github_token: {
    label: "GitHub token",
    help: "The git credential helper answers for the host; a read grant with no env var sets GH_TOKEN.",
    fields: [
      { name: "value", label: "Token", secret: true, required: true, placeholder: "github_pat_…" },
      { name: "host", label: "Host", secret: false, required: false, placeholder: "github.com" },
    ],
  },
  login: {
    label: "Login",
    help: "A user name and a password. The login of a vnc or rdp target.",
    fields: [
      { name: "username", label: "User name", secret: false, required: false },
      { name: "password", label: "Password", secret: true, required: true },
      { name: "url", label: "URL", secret: false, required: false },
      { name: "domain", label: "Domain", secret: false, required: false },
    ],
  },
  ssh_key: {
    label: "SSH key",
    help: "A private key in the OpenSSH format, or in PEM with no passphrase. The daemon derives the public key. Or use Generate SSH key.",
    fields: [
      { name: "private_key", label: "Private key", secret: true, required: true, multiline: true, placeholder: "-----BEGIN OPENSSH PRIVATE KEY-----" },
      { name: "passphrase", label: "Passphrase", secret: true, required: false },
    ],
  },
  generic: {
    label: "Generic",
    help: "Named fields. Mark each field secret or plain. A plain field shows in each list.",
    fields: [],
  },
};

/** One row of a `generic` secret. */
export interface GenericRow {
  key: string;
  value: string;
  secret: boolean;
}

export interface SecretForm {
  /** `global` or a project. */
  scope: string;
  name: string;
  kind: SecretKind;
  /** The values of a typed kind, by field name. */
  values: Record<string, string>;
  /** The rows of a `generic` secret. */
  rows: GenericRow[];
}

export const emptyForm = (scope: string, kind: SecretKind = "api_key"): SecretForm => ({
  scope,
  name: "",
  kind,
  values: {},
  rows: [{ key: "", value: "", secret: true }],
});

/** The form of a Replace: the kind and the plain fields of the secret; the secret fields stay empty. */
export function replaceForm(s: Secret): SecretForm {
  const f = emptyForm(s.scope, s.kind);
  f.name = s.name;
  if (s.kind === "generic") {
    f.rows = [
      ...Object.entries(s.fields).map(([key, v]) => ({ key, value: String(v), secret: false })),
      ...s.secret_fields.map((key) => ({ key, value: "", secret: true })),
    ];
  } else {
    for (const spec of KINDS[s.kind].fields) {
      const v = s.fields[spec.name];
      if (!spec.secret && v !== undefined) f.values[spec.name] = Array.isArray(v) ? v.join(", ") : String(v);
    }
  }
  return f;
}

export const NAME_RE = /^[a-z0-9_-]{1,63}$/;
const FIELD_RE = /^[A-Za-z_][A-Za-z0-9_.-]{0,63}$/;

/** `fields` and `secret_fields` of the form, or the reason the form is not complete. */
export function fieldsOf(f: SecretForm): { fields: Record<string, unknown>; secret_fields?: string[] } | { error: string } {
  const fields: Record<string, unknown> = {};
  if (f.kind === "generic") {
    const rows = f.rows.filter((r) => r.key.trim() || r.value);
    if (!rows.length) return { error: "Add at least one field." };
    if (rows.length > 32) return { error: "A generic secret has at most 32 fields." };
    const secret: string[] = [];
    for (const r of rows) {
      const key = r.key.trim();
      if (!FIELD_RE.test(key)) return { error: `The field name "${key}" is not valid: 1-64 of A-Z, a-z, 0-9, _, -, ., starting with a letter or _.` };
      if (key in fields) return { error: `The field "${key}" is there two times.` };
      if (!r.value) return { error: `Give the field "${key}" a value.` };
      fields[key] = r.secret ? r.value : r.value.trim();
      if (r.secret) secret.push(key);
    }
    return { fields, secret_fields: secret };
  }
  for (const spec of KINDS[f.kind].fields) {
    const raw = f.values[spec.name] ?? "";
    // A secret value keeps its spaces (a password); the daemon trims a one-line token.
    const v = spec.secret ? raw : raw.trim();
    if (!v.trim()) {
      if (spec.required) return { error: `${spec.label} is required.` };
      continue;
    }
    fields[spec.name] = spec.name === "hosts" ? v.split(",").map((h) => h.trim()).filter(Boolean) : v;
  }
  return { fields };
}

/** The body of `POST /api/secrets`. */
export function createBody(f: SecretForm): { body: NewSecret } | { error: string } {
  const name = f.name.trim();
  if (!NAME_RE.test(name)) return { error: "The name is 1-63 of a-z, 0-9, -, _." };
  if (!f.scope) return { error: "Choose a scope: global, or a project." };
  const r = fieldsOf(f);
  if ("error" in r) return r;
  return { body: { scope: f.scope, name, kind: f.kind, ...r } };
}

/** The body of `PUT /api/secrets/{scope}/{name}`. */
export function putBody(f: SecretForm): { body: SecretPut } | { error: string } {
  const r = fieldsOf(f);
  if ("error" in r) return r;
  return { body: { kind: f.kind, ...r } };
}

// ----- grants -----

/** The default level of a grant: `use` for the kinds that a broker serves, else `read` (docs/api.md §5.2). */
export const defaultLevel = (k: SecretKind): GrantLevel => (k === "token" || k === "github_token" || k === "ssh_key" ? "use" : "read");

/** An env var puts the value where the agent can print it, so it needs `read`. */
export const envAllowed = (level: GrantLevel): boolean => level === "read";

const ENV_RE = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const RESERVED_ENV = ["PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "TZ", "SSH_AUTH_SOCK"];
const RESERVED_PREFIX = ["LD_", "LC_", "TERRARIUM_", "GIT_AUTHOR_", "GIT_COMMITTER_", "GIT_CONFIG"];

/** Why an env name is refused, or null. The daemon checks it too (and knows the harness home vars). */
export function envError(env: string): string | null {
  if (!env) return null;
  if (!ENV_RE.test(env)) return "An env var name is a letter or _, then letters, digits, or _.";
  if (RESERVED_ENV.includes(env) || RESERVED_PREFIX.some((p) => env.startsWith(p))) return `The sandbox sets ${env}.`;
  return null;
}

export interface GrantForm {
  grantee: string;
  level: GrantLevel;
  env: string;
}

export const emptyGrant = (s: Pick<Secret, "kind">): GrantForm => ({ grantee: "", level: defaultLevel(s.kind), env: "" });

/** The body of `POST .../grants`. The env name goes only with `read`. */
export function grantBody(g: GrantForm): { body: NewSecretGrant } | { error: string } {
  if (!g.grantee) return { error: "Choose an agent or a project." };
  const env = envAllowed(g.level) ? g.env.trim() : "";
  const bad = envError(env);
  if (bad) return { error: bad };
  return { body: { grantee: g.grantee, level: g.level, ...(env ? { env } : {}) } };
}

export interface GranteeOption {
  value: string;
  label: string;
  group: "Projects" | "Agents";
}

/** Who may get the secret: any project or agent for a global secret; the project and its agents for a project secret. */
export function granteeOptions(s: Pick<Secret, "scope">, projects: readonly { name: string }[], agents: readonly Pick<Agent, "project" | "name">[]): GranteeOption[] {
  const reach = (p: string) => s.scope === GLOBAL || s.scope === p;
  return [
    ...projects.filter((p) => reach(p.name)).map((p) => ({ value: `project:${p.name}`, label: `project ${p.name} (each agent)`, group: "Projects" as const })),
    ...agents
      .filter((a) => reach(a.project))
      .slice()
      .sort((a, b) => a.project.localeCompare(b.project) || a.name.localeCompare(b.name))
      .map((a) => ({ value: `agent:${a.project}/${a.name}`, label: `${a.project}/${a.name}`, group: "Agents" as const })),
  ];
}

// ----- the list -----

/** A secret in the list of a project: `global` is a global secret that the project can use; `hidden` is a global one that a project secret of the same name hides. */
export interface ListedSecret {
  secret: Secret;
  global: boolean;
  hidden: boolean;
}

/** The global page's filter: `all`, `global`, or a project name. */
export function filterSecrets(list: readonly Secret[], scope: string): Secret[] {
  if (scope === "all") return [...list];
  return list.filter((s) => s.scope === scope);
}

/** The project page: the project's secrets, then the global ones, marked. */
export function projectSecrets(list: readonly Secret[], project: string): ListedSecret[] {
  const own = list.filter((s) => s.scope === project).sort((a, b) => a.name.localeCompare(b.name));
  const names = new Set(own.map((s) => s.name));
  const global = list.filter((s) => s.scope === GLOBAL).sort((a, b) => a.name.localeCompare(b.name));
  return [...own.map((secret) => ({ secret, global: false, hidden: false })), ...global.map((secret) => ({ secret, global: true, hidden: names.has(secret.name) }))];
}

/** The plain fields to show, without the public key and the fingerprint (they show apart). */
export function plainFields(s: Secret): [string, string][] {
  return Object.entries(s.fields)
    .filter(([k]) => !(s.kind === "ssh_key" && (k === "public_key" || k === "fingerprint")))
    .map(([k, v]) => [k, Array.isArray(v) ? v.join(", ") : String(v)]);
}

export const publicKey = (s: Secret): string | null => (s.kind === "ssh_key" && typeof s.fields.public_key === "string" ? s.fields.public_key : null);
export const fingerprint = (s: Secret): string | null => (typeof s.fields.fingerprint === "string" ? s.fields.fingerprint : null);

/** The shell line that adds the public key to `~/.ssh/authorized_keys` of the user on the target. */
export const authorizedKeysLine = (pub: string): string => `echo '${pub.replace(/'/g, "'\\''")}' >> ~/.ssh/authorized_keys`;

// ----- target credentials (PLAN §16b.6a) -----

/** The secret kind that the `credential` of a target kind names. */
export const credentialKind = (k: TargetKind): SecretKind | null => (k === "ssh" ? "ssh_key" : k === "vnc" || k === "rdp" ? "login" : null);

export interface CredentialChoice {
  name: string;
  scope: string;
  fingerprint: string | null;
  label: string;
}

/**
 * The secrets that a target can name in `credential`: the right kind; for a
 * project target, its project's and the global ones (a project secret hides
 * a global one of the same name); for a global target, the global ones only.
 * `scope` is a project name or `global`.
 */
export function credentialChoices(list: readonly Secret[], kind: TargetKind, scope: string): CredentialChoice[] {
  const want = credentialKind(kind);
  if (!want) return [];
  const ok = list.filter((s) => s.kind === want && (s.scope === GLOBAL || (scope !== GLOBAL && s.scope === scope)));
  const byName = new Map<string, Secret>();
  for (const s of ok) {
    const had = byName.get(s.name);
    if (!had || had.scope === GLOBAL) byName.set(s.name, s);
  }
  return [...byName.values()]
    .sort((a, b) => (a.scope === GLOBAL ? 1 : 0) - (b.scope === GLOBAL ? 1 : 0) || a.name.localeCompare(b.name))
    .map((s) => {
      const fp = fingerprint(s);
      return { name: s.name, scope: s.scope, fingerprint: fp, label: `${s.name} (${s.scope === GLOBAL ? "global" : s.scope})${fp ? ` · ${fp.slice(0, 19)}…` : ""}` };
    });
}

/** The chip class and the label of each test result (docs/api.md §17.2). */
export const TEST_RESULT: Record<TargetTestResult, { chip: string; label: string }> = {
  ok: { chip: "ok", label: "login works" },
  auth_refused: { chip: "bad", label: "key refused" },
  host_key_mismatch: { chip: "bad", label: "host key changed" },
  host_key_unknown: { chip: "warn", label: "host key unknown" },
  unreachable: { chip: "bad", label: "unreachable" },
  error: { chip: "bad", label: "error" },
};

// ----- errors -----

/** The targets of the `409` of a secret delete, or null for another error. */
export function deleteConflict(e: unknown): string[] | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null;
  const t = (e.details as { targets?: unknown } | undefined)?.targets;
  return Array.isArray(t) ? t.map(String) : [];
}

/** The sentence for a refused reveal. */
export function revealError(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 403) return /password/i.test(e.message) ? "The operator password is wrong." : "Only the operator can reveal a value.";
    if (e.status === 409) return "No operator password is set. Set one with `terrariumd password set`, then try again.";
    if (e.status === 429) return "Too many wrong passwords. Wait, then try again.";
    return e.message;
  }
  return e instanceof Error ? e.message : String(e);
}

// ----- the reveal timer -----

export const REVEAL_MS = 30_000;

/**
 * Hold revealed values for `ms`, then drop them. `onChange` gets the values
 * and the seconds left at each tick, and `null` at the end. The values live
 * only in this closure and in the dialog that shows them.
 */
export function createRevealHold(onChange: (v: { values: Record<string, string>; left: number } | null) => void, ms = REVEAL_MS, timers: Pick<typeof globalThis, "setInterval" | "clearInterval"> = globalThis) {
  let handle: ReturnType<typeof setInterval> | undefined;
  let values: Record<string, string> | null = null;
  const stop = () => {
    if (handle !== undefined) timers.clearInterval(handle);
    handle = undefined;
    values = null;
    onChange(null);
  };
  return {
    show(v: Record<string, string>) {
      if (handle !== undefined) timers.clearInterval(handle);
      values = { ...v };
      let left = Math.ceil(ms / 1000);
      onChange({ values, left });
      handle = timers.setInterval(() => {
        left--;
        if (left <= 0 || !values) stop();
        else onChange({ values, left });
      }, 1000);
    },
    hide: stop,
    active: () => values !== null,
  };
}

/** The credential of a target, for a list cell: the name and the short fingerprint. */
export const credentialCell = (t: Pick<Target, "credential" | "credential_info">): string | null => {
  if (!t.credential) return null;
  const fp = t.credential_info?.fingerprint;
  return fp ? `${t.credential} · ${fp}` : t.credential;
};

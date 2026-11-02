// The agent form model: AgentDef fields as editable strings, and the
// conversions to a create body and a patch. Pure.

import type { Agent, AgentDef, AgentPatch, AgentPolicy, Harness, RestartPolicy, Role, SpawnPolicy } from "../../api/types";
import { sameSpawn, spawnProblem } from "./spawn";

export interface AgentFormValues {
  name: string;
  project: string;
  harness: Harness;
  subscription: string;
  /** The subscriptions a swap may pick. Empty: no pool. */
  subscription_pool: string[];
  model: string;
  prompt: string;
  /** The text as typed. Empty: the harness's default system prompt. */
  system_prompt: string;
  /** `leader`, `worker`, `none`, or a prompt file name. Empty: by role. */
  persona: string;
  roles: Role[];
  targets: string[];
  /** Comma or newline separated. */
  mail_to: string;
  cross_project: string;
  restart: RestartPolicy;
  /** The explicit skill names, or `["*"]` for all. Empty: none. */
  skills: string[];
  /** The skills that the agent does not get (a default-on skill, for example). */
  skills_off: string[];
  /** False: no default-on skill. */
  default_skills: boolean;
  /** `off`, `0`, or a duration such as `30m` or `1h30m`. Empty: the global default. */
  idle_stop: string;
  /** `on` or `off`. Empty: the global default. */
  compact_on_idle: CompactSwitch;
  /** `off`, `0`, or a duration such as `24h`. Empty: the global default. */
  compact_max_age: string;
  /** The spawn rights (`policy.spawn`, docs/api.md §6.10). Null: none. Optional: a template form has none. */
  spawn?: SpawnPolicy | null;
}

/** The form value of `compact_on_idle`: empty is the global default. */
export type CompactSwitch = "" | "on" | "off";

/** `true` -> `on`, `false` -> `off`, absent -> empty (the global default). */
export const switchOf = (b: boolean | undefined): CompactSwitch => (b === undefined ? "" : b ? "on" : "off");

/** "a, b\nc" -> ["a", "b", "c"]. */
export const parseList = (s: string): string[] =>
  s
    .split(/[,\n]/)
    .map((x) => x.trim())
    .filter(Boolean);

export function emptyAgentForm(project = ""): AgentFormValues {
  return { name: "", project, harness: "claude", subscription: "", subscription_pool: [], model: "", prompt: "", system_prompt: "", persona: "", roles: [], targets: [], mail_to: "*", cross_project: "", restart: "on-failure", skills: [], skills_off: [], default_skills: true, idle_stop: "", compact_on_idle: "", compact_max_age: "" };
}

export function agentToForm(a: Agent): AgentFormValues {
  return {
    name: a.name,
    project: a.project,
    harness: a.harness,
    subscription: a.subscription ?? "",
    subscription_pool: a.subscription_pool ?? [],
    model: a.model ?? "",
    prompt: a.prompt ?? "",
    system_prompt: a.system_prompt ?? "",
    persona: a.persona ?? "",
    roles: a.roles.filter((r) => r !== "member"),
    spawn: a.policy?.spawn ?? null,
    targets: a.policy?.targets ?? [],
    mail_to: (a.policy?.mail_to ?? ["*"]).join(", "),
    cross_project: (a.policy?.cross_project ?? []).join(", "),
    restart: a.restart ?? "on-failure",
    skills: a.skills ?? [],
    skills_off: a.skills_off ?? [],
    default_skills: a.default_skills ?? true,
    idle_stop: a.idle_stop ?? "",
    compact_on_idle: switchOf(a.compact_on_idle),
    compact_max_age: a.compact_max_age ?? "",
  };
}

/** The daemon's limit on `system_prompt`: 32 KiB of UTF-8. */
export const SYSTEM_PROMPT_MAX_BYTES = 32 * 1024;

/** The problem of a system prompt over the limit, or null. */
export function systemPromptProblem(s: string): string | null {
  const bytes = new TextEncoder().encode(s).length;
  return bytes > SYSTEM_PROMPT_MAX_BYTES ? `System prompt: ${bytes} bytes; the limit is ${SYSTEM_PROMPT_MAX_BYTES} (32 KiB).` : null;
}

/** The personas that every daemon has. A prompt file name is also a persona. */
export const PERSONAS = ["leader", "worker", "none"] as const;

/** The daemon's check of a persona: letters, digits, `-`, and `_`, at most 64 bytes. Empty is "by role". */
export function personaProblem(s: string): string | null {
  const v = s.trim();
  return v && !/^[A-Za-z0-9_-]{1,64}$/.test(v) ? `Persona "${v}": use leader, worker, none, or a prompt file name (letters, digits, - and _).` : null;
}

/** The persona that the agent gets: its own, else the one of its roles. */
export function effectivePersona(a: Pick<Agent, "persona" | "roles">): { persona: string; byRole: boolean } {
  if (a.persona) return { persona: a.persona, byRole: false };
  const leader = a.roles.some((r) => r === "project_admin" || r === "sysadmin");
  return { persona: leader ? "leader" : "worker", byRole: true };
}

/** One or more `<n><unit>` parts, units d, h, m, s: `90s`, `30m`, `1h30m`, `2d`. */
const DURATION_RE = /^(\d+[dhms])+$/;

/** The problem of an idle stop, or null. Empty is the global default; `off` and `0` never stop the agent. */
export function idleStopProblem(s: string): string | null {
  const v = s.trim();
  if (!v || v === "off" || v === "0" || DURATION_RE.test(v)) return null;
  return `Idle stop "${v}": use off, 0, or a duration such as 30m, 1h30m, or 90s (units d, h, m, s).`;
}

/** The idle stop as the agent page shows it. `fallback` is the global default from the health. */
export function idleStopText(value: string | undefined, fallback: string | undefined): string {
  const v = value?.trim();
  if (!v) return fallback ? `default (${fallback === "off" ? "never" : fallback})` : "default";
  return v === "off" || v === "0" ? "never" : `after ${v} idle`;
}

/** The problem of a compact max age, or null. Empty is the global default; `off` and `0` turn it off. */
export function compactMaxAgeProblem(s: string): string | null {
  const v = s.trim();
  if (!v || v === "off" || v === "0" || DURATION_RE.test(v)) return null;
  return `Compact max age "${v}": use off, 0, or a duration such as 24h or 12h30m (units d, h, m, s).`;
}

/**
 * The auto compact settings as the agent page shows them, one line.
 * `defaults` come from the health. An agent of a harness that cannot
 * compact gets the note instead.
 */
export function autoCompactText(
  a: Pick<Agent, "harness" | "compact_on_idle" | "compact_max_age" | "compact_supported">,
  defaults: { on_idle?: boolean; max_age?: string },
): string {
  if (a.compact_supported === false) return `auto compact not supported for ${a.harness}`;
  const onIdle = a.compact_on_idle ?? defaults.on_idle ?? false;
  const own = a.compact_max_age?.trim();
  const age = own || defaults.max_age || "off";
  const parts = [onIdle ? "on idle" : "not on idle", age === "off" || age === "0" ? "no max age" : `max age ${age}`];
  const isDefault = a.compact_on_idle === undefined && !own;
  return `${parts.join(", ")}${isDefault ? " (default)" : ""}`;
}

/** Problems the daemon would refuse with 422. Empty when the form is fine. */
export function validateAgentForm(v: AgentFormValues): string[] {
  const out: string[] = [];
  if (!/^[a-z0-9][a-z0-9_-]{0,62}$/.test(v.name)) out.push("Name: 1 to 63 of a-z, 0-9, - and _, starting with a letter or a digit.");
  if (!v.project) out.push("Pick a project.");
  for (const x of parseList(v.cross_project)) if (!/^[^/\s]+\/[^/\s]+$/.test(x)) out.push(`Cross-project entry "${x}" is not project/agent.`);
  const sp = systemPromptProblem(v.system_prompt);
  if (sp) out.push(sp);
  const pp = personaProblem(v.persona);
  if (pp) out.push(pp);
  const ip = idleStopProblem(v.idle_stop);
  if (ip) out.push(ip);
  const cp = compactMaxAgeProblem(v.compact_max_age);
  if (cp) out.push(cp);
  const spawnErr = v.spawn ? spawnProblem(v.spawn) : null;
  if (spawnErr) out.push(spawnErr);
  return out;
}

const opt = (s: string) => (s.trim() ? s.trim() : undefined);
/** The system prompt keeps its text as typed; only blank is empty. */
const optText = (s: string) => (s.trim() ? s : undefined);

export function formToDef(v: AgentFormValues): AgentDef {
  const def: AgentDef = {
    name: v.name.trim(),
    project: v.project,
    harness: v.harness,
    roles: v.roles,
    policy: policyOf(v),
    restart: v.restart,
  };
  const subscription = opt(v.subscription);
  const model = opt(v.model);
  const prompt = opt(v.prompt);
  if (subscription) def.subscription = subscription;
  if (v.subscription_pool.length) def.subscription_pool = v.subscription_pool;
  if (model) def.model = model;
  if (prompt) def.prompt = prompt;
  const systemPrompt = optText(v.system_prompt);
  if (systemPrompt) def.system_prompt = systemPrompt;
  const persona = opt(v.persona);
  if (persona) def.persona = persona;
  if (v.skills.length) def.skills = v.skills;
  if (v.skills_off.length) def.skills_off = v.skills_off;
  if (!v.default_skills) def.default_skills = false;
  const idleStop = opt(v.idle_stop);
  if (idleStop) def.idle_stop = idleStop;
  if (v.compact_on_idle) def.compact_on_idle = v.compact_on_idle === "on";
  const maxAge = opt(v.compact_max_age);
  if (maxAge) def.compact_max_age = maxAge;
  return def;
}

export const sameList = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/** The policy of the form: the lists, and `spawn` when the agent has spawn rights. */
export function policyOf(v: AgentFormValues): AgentPolicy {
  // Plain copies: the values can come from a store proxy, and a client may clone the body.
  const p: AgentPolicy = { targets: [...v.targets], mail_to: parseList(v.mail_to), cross_project: parseList(v.cross_project) };
  if (v.spawn) p.spawn = { ...v.spawn, templates: [...v.spawn.templates] };
  return p;
}

/** A changed optional field: the new value, `null` to clear it, or `undefined` for no change. */
const changed = (now: string, before: string): string | null | undefined => {
  const v = now.trim();
  if (v === before) return undefined;
  return v || null;
};

/**
 * Only the fields that changed. `PATCH` cannot change name, project, or
 * roles. An emptied subscription, model, prompt, system prompt, persona, or idle stop goes as
 * `null`, which clears it (docs/api.md §6.1).
 */
export function formToPatch(original: Agent, v: AgentFormValues): AgentPatch {
  return formDiff(agentToForm(original), v);
}

/**
 * The fields of `v` that differ from `before`, in the shape of a patch:
 * `null` clears an optional, `[]` clears a list, and `policy` comes whole
 * when one of its keys changed. Name, project, and roles are not in it.
 */
export function formDiff(before: AgentFormValues, v: AgentFormValues): AgentPatch {
  const patch: AgentPatch = {};
  if (v.harness !== before.harness) patch.harness = v.harness;
  const subscription = changed(v.subscription, before.subscription);
  const model = changed(v.model, before.model);
  const prompt = changed(v.prompt, before.prompt);
  if (subscription !== undefined) patch.subscription = subscription;
  // `[]` clears the pool.
  if (!sameList(v.subscription_pool, before.subscription_pool)) patch.subscription_pool = v.subscription_pool;
  if (model !== undefined) patch.model = model;
  if (prompt !== undefined) patch.prompt = prompt;
  const systemPrompt = optText(v.system_prompt) ?? null;
  if (systemPrompt !== (optText(before.system_prompt) ?? null)) patch.system_prompt = systemPrompt;
  // An emptied persona goes as null: back to "by role".
  const persona = changed(v.persona, before.persona);
  if (persona !== undefined) patch.persona = persona;
  if (v.restart !== before.restart) patch.restart = v.restart;
  // `[]` clears the skills.
  if (!sameList(v.skills, before.skills)) patch.skills = v.skills;
  if (!sameList(v.skills_off, before.skills_off)) patch.skills_off = v.skills_off;
  if (v.default_skills !== before.default_skills) patch.default_skills = v.default_skills;
  // An emptied idle stop goes as null: back to the global default.
  const idleStop = changed(v.idle_stop, before.idle_stop);
  if (idleStop !== undefined) patch.idle_stop = idleStop;
  // An emptied auto compact setting goes as null: back to the global default.
  if (v.compact_on_idle !== before.compact_on_idle) patch.compact_on_idle = v.compact_on_idle ? v.compact_on_idle === "on" : null;
  const maxAge = changed(v.compact_max_age, before.compact_max_age);
  if (maxAge !== undefined) patch.compact_max_age = maxAge;
  // The policy goes whole, with its spawn block: a policy without `spawn` removes the spawn rights.
  const policy = policyOf(v);
  const oldPolicy = policyOf(before);
  if (!sameList(policy.targets, oldPolicy.targets) || !sameList(policy.mail_to, oldPolicy.mail_to) || !sameList(policy.cross_project ?? [], oldPolicy.cross_project ?? []) || !sameSpawn(policy.spawn, oldPolicy.spawn)) patch.policy = policy;
  return patch;
}

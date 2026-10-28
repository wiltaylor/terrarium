// The agent form model: AgentDef fields as editable strings, and the
// conversions to a create body and a patch. Pure.

import type { Agent, AgentDef, AgentPatch, Harness, RestartPolicy, Role } from "../../api/types";

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
  /** Skill names, or `["*"]` for all. Empty: none. */
  skills: string[];
}

/** "a, b\nc" -> ["a", "b", "c"]. */
export const parseList = (s: string): string[] =>
  s
    .split(/[,\n]/)
    .map((x) => x.trim())
    .filter(Boolean);

export function emptyAgentForm(project = ""): AgentFormValues {
  return { name: "", project, harness: "claude", subscription: "", subscription_pool: [], model: "", prompt: "", system_prompt: "", persona: "", roles: [], targets: [], mail_to: "*", cross_project: "", restart: "on-failure", skills: [] };
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
    targets: a.policy?.targets ?? [],
    mail_to: (a.policy?.mail_to ?? ["*"]).join(", "),
    cross_project: (a.policy?.cross_project ?? []).join(", "),
    restart: a.restart ?? "on-failure",
    skills: a.skills ?? [],
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
    policy: { targets: v.targets, mail_to: parseList(v.mail_to), cross_project: parseList(v.cross_project) },
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
  return def;
}

const sameList = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/** A changed optional field: the new value, `null` to clear it, or `undefined` for no change. */
const changed = (now: string, before: string): string | null | undefined => {
  const v = now.trim();
  if (v === before) return undefined;
  return v || null;
};

/**
 * Only the fields that changed. `PATCH` cannot change name, project, or
 * roles. An emptied subscription, model, prompt, system prompt, or persona goes as
 * `null`, which clears it (docs/api.md §6.1).
 */
export function formToPatch(original: Agent, v: AgentFormValues): AgentPatch {
  const before = agentToForm(original);
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
  const policy = { targets: v.targets, mail_to: parseList(v.mail_to), cross_project: parseList(v.cross_project) };
  const oldPolicy = { targets: before.targets, mail_to: parseList(before.mail_to), cross_project: parseList(before.cross_project) };
  if (!sameList(policy.targets, oldPolicy.targets) || !sameList(policy.mail_to, oldPolicy.mail_to) || !sameList(policy.cross_project, oldPolicy.cross_project)) patch.policy = policy;
  return patch;
}

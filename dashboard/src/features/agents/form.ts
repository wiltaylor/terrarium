// The agent form model: AgentDef fields as editable strings, and the
// conversions to a create body and a patch. Pure.

import type { Agent, AgentDef, AgentPatch, Harness, RestartPolicy, Role } from "../../api/types";

export interface AgentFormValues {
  name: string;
  project: string;
  harness: Harness;
  subscription: string;
  model: string;
  prompt: string;
  roles: Role[];
  targets: string[];
  /** Comma or newline separated. */
  mail_to: string;
  cross_project: string;
  restart: RestartPolicy;
}

/** "a, b\nc" -> ["a", "b", "c"]. */
export const parseList = (s: string): string[] =>
  s
    .split(/[,\n]/)
    .map((x) => x.trim())
    .filter(Boolean);

export function emptyAgentForm(project = ""): AgentFormValues {
  return { name: "", project, harness: "claude", subscription: "", model: "", prompt: "", roles: [], targets: [], mail_to: "*", cross_project: "", restart: "on-failure" };
}

export function agentToForm(a: Agent): AgentFormValues {
  return {
    name: a.name,
    project: a.project,
    harness: a.harness,
    subscription: a.subscription ?? "",
    model: a.model ?? "",
    prompt: a.prompt ?? "",
    roles: a.roles.filter((r) => r !== "member"),
    targets: a.policy?.targets ?? [],
    mail_to: (a.policy?.mail_to ?? ["*"]).join(", "),
    cross_project: (a.policy?.cross_project ?? []).join(", "),
    restart: a.restart ?? "on-failure",
  };
}

/** Problems the daemon would refuse with 422. Empty when the form is fine. */
export function validateAgentForm(v: AgentFormValues): string[] {
  const out: string[] = [];
  if (!/^[a-z0-9][a-z0-9_-]{0,62}$/.test(v.name)) out.push("Name: 1 to 63 of a-z, 0-9, - and _, starting with a letter or a digit.");
  if (!v.project) out.push("Pick a project.");
  for (const x of parseList(v.cross_project)) if (!/^[^/\s]+\/[^/\s]+$/.test(x)) out.push(`Cross-project entry "${x}" is not project/agent.`);
  return out;
}

const opt = (s: string) => (s.trim() ? s.trim() : undefined);

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
  if (model) def.model = model;
  if (prompt) def.prompt = prompt;
  return def;
}

const sameList = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * Only the fields that changed. `PATCH` cannot change name, project, or
 * roles, and cannot clear subscription, model, or prompt (an absent field
 * means "no change"), so an emptied field is left out.
 */
export function formToPatch(original: Agent, v: AgentFormValues): AgentPatch {
  const before = agentToForm(original);
  const patch: AgentPatch = {};
  if (v.harness !== before.harness) patch.harness = v.harness;
  if (v.subscription.trim() && v.subscription.trim() !== before.subscription) patch.subscription = v.subscription.trim();
  if (v.model.trim() && v.model.trim() !== before.model) patch.model = v.model.trim();
  if (v.prompt.trim() && v.prompt.trim() !== before.prompt) patch.prompt = v.prompt.trim();
  if (v.restart !== before.restart) patch.restart = v.restart;
  const policy = { targets: v.targets, mail_to: parseList(v.mail_to), cross_project: parseList(v.cross_project) };
  const oldPolicy = { targets: before.targets, mail_to: parseList(before.mail_to), cross_project: parseList(before.cross_project) };
  if (!sameList(policy.targets, oldPolicy.targets) || !sameList(policy.mail_to, oldPolicy.mail_to) || !sameList(policy.cross_project, oldPolicy.cross_project)) patch.policy = policy;
  return patch;
}

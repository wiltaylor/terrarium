// The agent template model (docs/api.md §21): a template's spec as the
// agent form values (no name, no project), the template form, the key that
// picks a template for an agent, and the order of the template chooser. Pure.

import type { AgentCreate, AgentDef, AgentPolicy, AgentTemplate, NewTemplate, TemplatePatch, TemplateResolved, TemplateSpec } from "../../api/types";
import { emptyAgentForm, formDiff, formToDef, idleStopProblem, parseList, personaProblem, sameList, systemPromptProblem, type AgentFormValues } from "../agents/form";

/** The template form: the template's own fields and the agent fields of its spec. */
export interface TemplateFormValues {
  name: string;
  /** A project name, or "" for a global template. */
  scope: string;
  description: string;
  agent: AgentFormValues;
}

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;

/** The agent form values of a spec. Name and project stay as given. */
export function specToForm(spec: TemplateSpec, project = "", name = ""): AgentFormValues {
  return {
    ...emptyAgentForm(project),
    name,
    harness: spec.harness,
    subscription: spec.subscription ?? "",
    subscription_pool: spec.subscription_pool ?? [],
    model: spec.model ?? "",
    prompt: spec.prompt ?? "",
    system_prompt: spec.system_prompt ?? "",
    persona: spec.persona ?? "",
    roles: (spec.roles ?? []).filter((r) => r !== "member"),
    targets: spec.policy?.targets ?? [],
    mail_to: (spec.policy?.mail_to ?? ["*"]).join(", "),
    cross_project: (spec.policy?.cross_project ?? []).join(", "),
    restart: spec.restart ?? "on-failure",
    skills: spec.skills ?? [],
    idle_stop: spec.idle_stop ?? "",
  };
}

/** The spec of the agent form values: the create body without name and project. `base` keeps the fields the form does not show. */
export function formToSpec(v: AgentFormValues, base?: TemplateSpec): TemplateSpec {
  const def: Partial<AgentDef> = formToDef(v);
  delete def.name;
  delete def.project;
  const spec = def as TemplateSpec;
  if (base?.new_session_on_prompt_change === false) spec.new_session_on_prompt_change = false;
  return spec;
}

export function emptyTemplateForm(scope = ""): TemplateFormValues {
  return { name: "", scope, description: "", agent: emptyAgentForm(scope) };
}

export function templateToForm(t: AgentTemplate): TemplateFormValues {
  return { name: t.name, scope: t.project ?? "", description: t.description, agent: specToForm(t.spec, t.project ?? "") };
}

/** Problems the daemon would refuse with 422. */
export function validateTemplateForm(v: TemplateFormValues): string[] {
  const out: string[] = [];
  if (!NAME_RE.test(v.name)) out.push("Name: 1 to 63 of a-z, 0-9, - and _, starting with a letter or a digit.");
  for (const x of v.agent.cross_project.split(/[,\n]/).map((s) => s.trim()).filter(Boolean)) if (!/^[^/\s]+\/[^/\s]+$/.test(x)) out.push(`Cross-project entry "${x}" is not project/agent.`);
  const sp = systemPromptProblem(v.agent.system_prompt);
  if (sp) out.push(sp);
  const pp = personaProblem(v.agent.persona);
  if (pp) out.push(pp);
  const ip = idleStopProblem(v.agent.idle_stop);
  if (ip) out.push(ip);
  return out;
}

export function formToNewTemplate(v: TemplateFormValues): NewTemplate {
  return { name: v.name.trim(), project: v.scope || null, description: v.description.trim(), spec: formToSpec(v.agent) };
}

/** Only the fields that changed. The scope of a template does not change. */
export function formToTemplatePatch(t: AgentTemplate, v: TemplateFormValues): TemplatePatch {
  const patch: TemplatePatch = {};
  if (v.name.trim() !== t.name) patch.name = v.name.trim();
  if (v.description.trim() !== t.description) patch.description = v.description.trim();
  const spec = formToSpec(v.agent, t.spec);
  if (JSON.stringify(sortKeys(spec)) !== JSON.stringify(sortKeys(formToSpec(specToForm(t.spec), t.spec)))) patch.spec = spec;
  return patch;
}

const sortKeys = (o: object): unknown => Object.fromEntries(Object.entries(o).sort(([a], [b]) => a.localeCompare(b)));

/** "terrarium" or "global". */
export const scopeLabel = (t: Pick<AgentTemplate, "project">) => t.project ?? "global";

/** The path key of a template: `project/name`, or `_/name` for a global one. */
export const pathKey = (t: Pick<AgentTemplate, "name" | "project">) => `${t.project ?? "_"}/${t.name}`;

/**
 * The `template` key of an agent create for an agent of `project`: the
 * name for a template of that project, `global:<name>` for a global one
 * (so that a project template with the same name does not win).
 */
export const useKey = (t: Pick<AgentTemplate, "name" | "project">) => (t.project === null ? `global:${t.name}` : t.name);

/** The templates an agent of `project` may use: the project's first, then the global ones, each by name. */
export function selectable(list: AgentTemplate[], project: string): AgentTemplate[] {
  const byName = (a: AgentTemplate, b: AgentTemplate) => a.name.localeCompare(b.name);
  return [...list.filter((t) => t.project === project).sort(byName), ...list.filter((t) => t.project === null).sort(byName)];
}

/** The agent form after a pick: the resolved fields, with the name the user typed. */
export function resolvedToForm(r: TemplateResolved, name: string): AgentFormValues {
  return specToForm(r.agent, r.agent.project, name);
}

/** A template pick for an agent create: the `template` key, and the form values that the template gives. */
export interface TemplatePick {
  key: string;
  base: AgentFormValues;
}

/**
 * The create body. Without a template: the full definition. With a
 * template: `template` and only the fields that differ from what the
 * template gives (docs/api.md §21.3), in the shape of `formDiff`. An
 * emptied optional goes as `null` and an emptied list as `[]`, which clear
 * the template's value. `policy` merges per key, so it has only the keys
 * that changed.
 */
export function formToCreate(v: AgentFormValues, pick: TemplatePick | null): AgentCreate {
  if (!pick) return formToDef(v);
  const { policy, ...diff } = formDiff(pick.base, v);
  const body: AgentCreate = { name: v.name.trim(), project: v.project, template: pick.key, ...diff };
  if (!sameList(v.roles, pick.base.roles)) body.roles = v.roles;
  if (policy) {
    const was = { targets: pick.base.targets, mail_to: parseList(pick.base.mail_to), cross_project: parseList(pick.base.cross_project) };
    const keys = (["targets", "mail_to", "cross_project"] as const).filter((k) => !sameList(policy[k] ?? [], was[k]));
    body.policy = Object.fromEntries(keys.map((k) => [k, policy[k]])) as Partial<AgentPolicy>;
  }
  return body;
}

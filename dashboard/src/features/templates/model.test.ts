import { describe, expect, it } from "vitest";
import type { AgentTemplate } from "../../api/types";
import { applyTemplate } from "../../api/mock/templates";
import { emptyTemplateForm, formToCreate, formToNewTemplate, formToSpec, formToTemplatePatch, pathKey, resolvedToForm, scopeLabel, selectable, specToForm, templateToForm, useKey, validateTemplateForm } from "./model";

const tpl = (name: string, project: string | null, extra: Partial<AgentTemplate> = {}): AgentTemplate => ({
  id: `T-${project ?? "_"}-${name}`,
  name,
  project,
  description: "",
  spec: { harness: "codex", model: "gpt", prompt: "worker", policy: { targets: ["t1"], mail_to: ["lead"] }, restart: "on-failure" },
  revision: 1,
  created_by: "operator",
  source: "api",
  created_at: "",
  updated_at: "",
  ...extra,
});

describe("template model", () => {
  it("turns a spec into form values and back", () => {
    const t = tpl("w", "p");
    const form = specToForm(t.spec, "p", "w1");
    expect(form).toMatchObject({ name: "w1", project: "p", harness: "codex", model: "gpt", targets: ["t1"], mail_to: "lead", restart: "on-failure" });
    expect(formToSpec(form)).toEqual({ harness: "codex", model: "gpt", prompt: "worker", roles: [], policy: { targets: ["t1"], mail_to: ["lead"], cross_project: [] }, restart: "on-failure" });
    // A field the form does not show stays.
    expect(formToSpec(form, { ...t.spec, new_session_on_prompt_change: false }).new_session_on_prompt_change).toBe(false);
  });

  it("orders the select: the project's templates first, then global, never another project's", () => {
    const list = [tpl("b", null), tpl("z", "p"), tpl("a", "p"), tpl("a", null), tpl("q", "other")];
    expect(selectable(list, "p").map((t) => `${scopeLabel(t)}:${t.name}`)).toEqual(["p:a", "p:z", "global:a", "global:b"]);
  });

  it("builds the keys: global: forces the global template; the path key has _ for global", () => {
    expect(useKey(tpl("w", null))).toBe("global:w");
    expect(useKey(tpl("w", "p"))).toBe("w");
    expect(pathKey(tpl("w", null))).toBe("_/w");
    expect(pathKey(tpl("w", "p"))).toBe("p/w");
  });

  it("sends the template and only the fields that differ from it", () => {
    const t = tpl("w", "p");
    const base = resolvedToForm({ template: { id: t.id, name: "w", project: "p", revision: 1 }, agent: { project: "p", ...t.spec } }, "");
    const pick = { key: "w", base };
    const form = { ...base, name: "w1" };
    // No change: the name, the project, and the template only.
    expect(formToCreate(form, pick)).toEqual({ name: "w1", project: "p", template: "w" });
    // An emptied model goes as null, which clears the template's model.
    const body = formToCreate({ ...form, model: "" }, pick);
    expect(body).toEqual({ name: "w1", project: "p", template: "w", model: null });
    expect(applyTemplate(t.spec, body).model).toBeUndefined();
    // The policy has only the keys that changed; the daemon merges per key.
    const targets = formToCreate({ ...form, targets: [] }, pick);
    expect(targets).toEqual({ name: "w1", project: "p", template: "w", policy: { targets: [] } });
    expect(applyTemplate(t.spec, targets).policy).toEqual({ targets: [], mail_to: ["lead"] });
    expect(formToCreate({ ...form, mail_to: "*", cross_project: "ops/lead" }, pick)).toMatchObject({ policy: { mail_to: ["*"], cross_project: ["ops/lead"] } });
    // The idle stop: a new one overrides; no change sends nothing.
    expect(formToCreate({ ...form, idle_stop: "1h" }, pick)).toEqual({ name: "w1", project: "p", template: "w", idle_stop: "1h" });
    expect(specToForm({ ...t.spec, idle_stop: "off" }).idle_stop).toBe("off");
    expect(validateTemplateForm({ ...emptyTemplateForm(), name: "t", agent: { ...emptyTemplateForm().agent, idle_stop: "5x" } })).toEqual([expect.stringContaining("Idle stop")]);
    // Roles and the pool are overrides too.
    expect(formToCreate({ ...form, roles: ["project_admin"] }, pick)).toEqual({ name: "w1", project: "p", template: "w", roles: ["project_admin"] });
    expect(formToCreate({ ...form, subscription: "a", subscription_pool: ["a", "b"] }, pick)).toEqual({ name: "w1", project: "p", template: "w", subscription: "a", subscription_pool: ["a", "b"] });
    expect(specToForm({ harness: "claude", subscription: "a", subscription_pool: ["a", "b"] }).subscription_pool).toEqual(["a", "b"]);
    expect(formToSpec({ ...form, subscription: "a", subscription_pool: ["a", "b"] }).subscription_pool).toEqual(["a", "b"]);
    // The daemon's merge of the body gives what the form shows.
    const changed = { ...form, harness: "claude" as const, model: "", prompt: "lead", persona: "leader", skills: ["*"], targets: [], restart: "always" as const };
    expect(specToForm(applyTemplate(t.spec, formToCreate(changed, pick)), "p", "w1")).toEqual(changed);
    // Without a template the body is the plain definition.
    expect(formToCreate(form, null)).not.toHaveProperty("template");
  });

  it("merges a partial body: policy per key, null clears", () => {
    const def = applyTemplate(tpl("w", "p").spec, { name: "w1", project: "p", template: "w", policy: { mail_to: ["*"] } as never, prompt: null });
    expect(def).toMatchObject({ name: "w1", project: "p", harness: "codex", model: "gpt", policy: { targets: ["t1"], mail_to: ["*"] } });
    expect(def.prompt).toBeUndefined();
    expect(def).not.toHaveProperty("template");
  });

  it("makes a create body and a patch of only what changed", () => {
    const t = tpl("w", "p", { description: "d" });
    const v = templateToForm(t);
    expect(formToTemplatePatch(t, v)).toEqual({});
    expect(formToTemplatePatch(t, { ...v, description: "e" })).toEqual({ description: "e" });
    expect(formToTemplatePatch(t, { ...v, agent: { ...v.agent, model: "" } }).spec).not.toHaveProperty("model");
    expect(formToNewTemplate({ ...emptyTemplateForm(), name: "g" })).toMatchObject({ name: "g", project: null, spec: { harness: "claude" } });
    expect(validateTemplateForm({ ...emptyTemplateForm(), name: "Bad" })).toHaveLength(1);
    expect(validateTemplateForm({ ...emptyTemplateForm(), name: "ok" })).toEqual([]);
  });

  it("puts the system prompt in the spec only when set; an emptied one clears the key", () => {
    const t = tpl("w", "p", { spec: { ...tpl("w", "p").spec, system_prompt: "Be brief." } });
    const v = templateToForm(t);
    expect(v.agent.system_prompt).toBe("Be brief.");
    expect(formToSpec(v.agent).system_prompt).toBe("Be brief.");
    expect(formToSpec({ ...v.agent, system_prompt: "" })).not.toHaveProperty("system_prompt");
    expect(formToNewTemplate({ ...emptyTemplateForm(), name: "g" }).spec).not.toHaveProperty("system_prompt");
    expect(formToTemplatePatch(t, v)).toEqual({});
    const patch = formToTemplatePatch(t, { ...v, agent: { ...v.agent, system_prompt: "" } });
    expect(patch.spec).toBeDefined();
    expect(patch.spec).not.toHaveProperty("system_prompt");
    expect(formToTemplatePatch(tpl("w", "p"), { ...templateToForm(tpl("w", "p")), agent: { ...templateToForm(tpl("w", "p")).agent, system_prompt: "New." } }).spec?.system_prompt).toBe("New.");
    expect(validateTemplateForm({ ...v, agent: { ...v.agent, system_prompt: "a".repeat(32769) } })).toHaveLength(1);
  });

  it("fills the system prompt from a picked template, and the create body keeps or clears it", () => {
    const spec = { ...tpl("w", "p").spec, system_prompt: "Be brief." };
    const form = resolvedToForm({ template: { id: "T", name: "w", project: "p", revision: 1 }, agent: { project: "p", ...spec } }, "w1");
    expect(form.system_prompt).toBe("Be brief.");
    const pick = { key: "w", base: { ...form, name: "" } };
    // Not changed: not in the body, so the agent gets the template's.
    expect(formToCreate(form, pick)).not.toHaveProperty("system_prompt");
    expect(formToCreate({ ...form, system_prompt: "Other." }, pick)).toMatchObject({ system_prompt: "Other." });
    // Emptied in the form: null, which clears the template's value.
    const body = formToCreate({ ...form, system_prompt: "" }, pick);
    expect(body).toMatchObject({ system_prompt: null });
    expect(applyTemplate(spec, body)).not.toHaveProperty("system_prompt");
    expect(applyTemplate(spec, { name: "w1", project: "p", template: "w", system_prompt: "Other." }).system_prompt).toBe("Other.");
    expect(applyTemplate(spec, { name: "w1", project: "p", template: "w" }).system_prompt).toBe("Be brief.");
    // Without a template, an empty one is left out.
    expect(formToCreate({ ...form, system_prompt: "" }, null)).not.toHaveProperty("system_prompt");
  });

  it("round-trips the persona: in the spec when set, cleared when emptied, null in a create with a template", () => {
    const t = tpl("w", "p", { spec: { ...tpl("w", "p").spec, persona: "leader" } });
    const v = templateToForm(t);
    expect(v.agent.persona).toBe("leader");
    expect(formToSpec(v.agent).persona).toBe("leader");
    expect(formToTemplatePatch(t, v)).toEqual({});
    const patch = formToTemplatePatch(t, { ...v, agent: { ...v.agent, persona: "" } });
    expect(patch.spec).toBeDefined();
    expect(patch.spec).not.toHaveProperty("persona");
    expect(validateTemplateForm({ ...v, agent: { ...v.agent, persona: "bad name" } })).toHaveLength(1);
    // A create with the template: no persona when not changed, or null for "by role".
    const pick = { key: "w", base: v.agent };
    expect(formToCreate(v.agent, pick)).not.toHaveProperty("persona");
    expect(formToCreate({ ...v.agent, persona: "none" }, pick)).toMatchObject({ persona: "none" });
    const body = formToCreate({ ...v.agent, persona: "" }, pick);
    expect(body).toMatchObject({ persona: null });
    expect(applyTemplate(t.spec, body)).not.toHaveProperty("persona");
    expect(applyTemplate(t.spec, { name: "w1", project: "p", template: "w" }).persona).toBe("leader");
  });

  it("round-trips the skills: in the spec when set, [] in a create with a template so the form wins", () => {
    const t = tpl("w", "p", { spec: { ...tpl("w", "p").spec, skills: ["fern-check"] } });
    const form = specToForm(t.spec, "p", "w1");
    expect(form.skills).toEqual(["fern-check"]);
    expect(formToSpec(form).skills).toEqual(["fern-check"]);
    expect(formToSpec({ ...form, skills: [] })).not.toHaveProperty("skills");
    expect(specToForm(tpl("w", "p").spec).skills).toEqual([]);
    // A template form: a changed skill list is a spec change.
    const tf = templateToForm(t);
    expect(formToTemplatePatch(t, tf)).toEqual({});
    expect(formToTemplatePatch(t, { ...tf, agent: { ...tf.agent, skills: ["*"] } }).spec?.skills).toEqual(["*"]);
    expect(formToNewTemplate({ ...emptyTemplateForm(), name: "n", agent: { ...emptyTemplateForm().agent, skills: ["a"] } }).spec.skills).toEqual(["a"]);
    // The create body: no list when not changed, a new list, or [] to clear the template's.
    const pick = { key: "w", base: form };
    expect(formToCreate(form, pick)).not.toHaveProperty("skills");
    expect(formToCreate({ ...form, skills: ["*"] }, pick).skills).toEqual(["*"]);
    const cleared = formToCreate({ ...form, skills: [] }, pick);
    expect(cleared.skills).toEqual([]);
    expect(applyTemplate(t.spec, cleared).skills).toEqual([]);
    expect(applyTemplate(t.spec, { name: "w1", project: "p", template: "w" }).skills).toEqual(["fern-check"]);
    expect(formToCreate({ ...form, skills: [] }, null)).not.toHaveProperty("skills");
  });
});

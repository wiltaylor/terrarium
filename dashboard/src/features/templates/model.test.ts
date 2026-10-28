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

  it("sends every form field as an override, so the agent equals the form", () => {
    const t = tpl("w", "p");
    const form = resolvedToForm({ template: { id: t.id, name: "w", project: "p", revision: 1 }, agent: { project: "p", ...t.spec } }, "w1");
    const body = formToCreate({ ...form, model: "" }, "w");
    expect(body).toMatchObject({ name: "w1", project: "p", template: "w", model: null, subscription: null, prompt: "worker" });
    // The daemon's merge of that body gives what the form shows.
    const def = applyTemplate(t.spec, body);
    expect(def.model).toBeUndefined();
    expect(def.policy).toEqual({ targets: ["t1"], mail_to: ["lead"], cross_project: [] });
    // The pool goes as an override too: `[]` clears the template's pool.
    expect(body.subscription_pool).toEqual([]);
    expect(formToCreate({ ...form, subscription: "a", subscription_pool: ["a", "b"] }, "w")).toMatchObject({ subscription: "a", subscription_pool: ["a", "b"] });
    expect(specToForm({ harness: "claude", subscription: "a", subscription_pool: ["a", "b"] }).subscription_pool).toEqual(["a", "b"]);
    expect(formToSpec({ ...form, subscription: "a", subscription_pool: ["a", "b"] }).subscription_pool).toEqual(["a", "b"]);
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
    expect(formToCreate(form, "w")).toMatchObject({ system_prompt: "Be brief." });
    // Emptied in the form: null, which clears the template's value.
    const body = formToCreate({ ...form, system_prompt: "" }, "w");
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
    // A create with the template: the form's persona, or null for "by role".
    expect(formToCreate(v.agent, "w")).toMatchObject({ persona: "leader" });
    const body = formToCreate({ ...v.agent, persona: "" }, "w");
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
    // The create body: the form's list, or [] to clear the template's.
    expect(formToCreate(form, "w").skills).toEqual(["fern-check"]);
    const cleared = formToCreate({ ...form, skills: [] }, "w");
    expect(cleared.skills).toEqual([]);
    expect(applyTemplate(t.spec, cleared).skills).toEqual([]);
    expect(applyTemplate(t.spec, { name: "w1", project: "p", template: "w" }).skills).toEqual(["fern-check"]);
    expect(formToCreate({ ...form, skills: [] }, null)).not.toHaveProperty("skills");
  });
});

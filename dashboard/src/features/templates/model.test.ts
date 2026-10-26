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
});

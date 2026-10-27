import { describe, expect, it } from "vitest";
import type { Agent } from "../../api/types";
import { agentToForm, effectivePersona, emptyAgentForm, formToDef, formToPatch, parseList, personaProblem, validateAgentForm } from "./form";

const agent: Agent = {
  id: "A1", name: "worker", project: "p", harness: "pi", model: "m1", workspace: "/w", status: "idle", roles: ["project_admin"],
  policy: { targets: ["t1"], mail_to: ["*"], cross_project: [] }, restart: "on-failure", created_at: "",
};

describe("agent form", () => {
  it("parses comma and newline lists", () => {
    expect(parseList(" a, b\nc ,,")).toEqual(["a", "b", "c"]);
  });

  it("builds a full definition and leaves empty optionals out", () => {
    const def = formToDef({ ...emptyAgentForm("p"), name: "w", harness: "codex", model: "gpt", cross_project: "ops/triage, ops/lead", targets: ["t1"], roles: ["sysadmin"], restart: "always" });
    expect(def).toEqual({ name: "w", project: "p", harness: "codex", model: "gpt", roles: ["sysadmin"], policy: { targets: ["t1"], mail_to: ["*"], cross_project: ["ops/triage", "ops/lead"] }, restart: "always" });
  });

  it("patches only what changed, never name, project, or roles", () => {
    const v = agentToForm(agent);
    expect(formToPatch(agent, v)).toEqual({});
    expect(formToPatch(agent, { ...v, model: "m2", restart: "never", name: "x", roles: [] })).toEqual({ model: "m2", restart: "never" });
    expect(formToPatch(agent, { ...v, mail_to: "lead" })).toEqual({ policy: { targets: ["t1"], mail_to: ["lead"], cross_project: [] } });
    // An emptied field goes as null, which clears it.
    expect(formToPatch(agent, { ...v, model: "" })).toEqual({ model: null });
    expect(formToPatch({ ...agent, subscription: "zai-key" }, { ...v, subscription: "" })).toEqual({ subscription: null });
    expect(formToPatch(agent, { ...v, subscription: " zai-key " })).toEqual({ subscription: "zai-key" });
    // A harness change that cleared an incompatible subscription.
    expect(formToPatch({ ...agent, subscription: "zai-key" }, { ...v, harness: "codex", subscription: "" })).toEqual({ harness: "codex", subscription: null });
  });

  it("validates the name and cross-project entries", () => {
    expect(validateAgentForm({ ...emptyAgentForm("p"), name: "ok-1" })).toEqual([]);
    expect(validateAgentForm({ ...emptyAgentForm(""), name: "Bad Name", cross_project: "nope" })).toHaveLength(3);
  });

  it("sends the system prompt as typed, and leaves a blank one out", () => {
    const base = { ...emptyAgentForm("p"), name: "w" };
    expect(formToDef({ ...base, system_prompt: "  You review code.\n" }).system_prompt).toBe("  You review code.\n");
    expect(formToDef({ ...base, system_prompt: "" })).not.toHaveProperty("system_prompt");
    expect(formToDef({ ...base, system_prompt: " \n " })).not.toHaveProperty("system_prompt");
  });

  it("patches the system prompt: a new text, null when cleared, nothing when the same", () => {
    const withSp: Agent = { ...agent, system_prompt: "Be brief." };
    expect(agentToForm(withSp).system_prompt).toBe("Be brief.");
    expect(agentToForm(agent).system_prompt).toBe("");
    expect(formToPatch(withSp, agentToForm(withSp))).toEqual({});
    expect(formToPatch(withSp, { ...agentToForm(withSp), system_prompt: "" })).toEqual({ system_prompt: null });
    expect(formToPatch(withSp, { ...agentToForm(withSp), system_prompt: "Be terse." })).toEqual({ system_prompt: "Be terse." });
    expect(formToPatch(agent, { ...agentToForm(agent), system_prompt: "New." })).toEqual({ system_prompt: "New." });
    expect(formToPatch(agent, { ...agentToForm(agent), system_prompt: "  " })).toEqual({});
  });

  it("refuses a system prompt over 32 KiB of UTF-8", () => {
    const base = { ...emptyAgentForm("p"), name: "ok" };
    expect(validateAgentForm({ ...base, system_prompt: "a".repeat(32768) })).toEqual([]);
    expect(validateAgentForm({ ...base, system_prompt: "a".repeat(32769) })).toHaveLength(1);
    // Bytes, not characters: "é" is 2 bytes.
    expect(validateAgentForm({ ...base, system_prompt: "é".repeat(16385) })).toHaveLength(1);
  });

  it("carries the subscription pool: in the definition when set, and in the patch when changed", () => {
    const pooled: Agent = { ...agent, harness: "claude", subscription: "a", subscription_pool: ["a", "b"] };
    const v = agentToForm(pooled);
    expect(v.subscription_pool).toEqual(["a", "b"]);
    expect(formToDef(v).subscription_pool).toEqual(["a", "b"]);
    expect(formToDef({ ...v, subscription_pool: [] })).not.toHaveProperty("subscription_pool");
    expect(formToPatch(pooled, v)).toEqual({});
    // `[]` clears the pool.
    expect(formToPatch(pooled, { ...v, subscription_pool: [] })).toEqual({ subscription_pool: [] });
    expect(formToPatch(agent, { ...agentToForm(agent), subscription_pool: ["a", "c"] })).toEqual({ subscription_pool: ["a", "c"] });
  });

  it("carries the persona: in the definition when set, null on a patch that clears it", () => {
    const base = { ...emptyAgentForm("p"), name: "w" };
    expect(formToDef({ ...base, persona: " leader " }).persona).toBe("leader");
    expect(formToDef(base)).not.toHaveProperty("persona");
    const withP: Agent = { ...agent, persona: "reviewer" };
    expect(agentToForm(withP).persona).toBe("reviewer");
    expect(agentToForm(agent).persona).toBe("");
    expect(formToPatch(withP, agentToForm(withP))).toEqual({});
    expect(formToPatch(withP, { ...agentToForm(withP), persona: "" })).toEqual({ persona: null });
    expect(formToPatch(withP, { ...agentToForm(withP), persona: "none" })).toEqual({ persona: "none" });
    expect(formToPatch(agent, { ...agentToForm(agent), persona: "worker" })).toEqual({ persona: "worker" });
    expect(formToPatch(agent, { ...agentToForm(agent), persona: " " })).toEqual({});
  });

  it("checks the persona name, and names the persona by role when none is set", () => {
    expect(personaProblem("")).toBeNull();
    expect(personaProblem("my_persona-2")).toBeNull();
    expect(personaProblem("../x")).not.toBeNull();
    expect(validateAgentForm({ ...emptyAgentForm("p"), name: "ok", persona: "a b" })).toHaveLength(1);
    expect(effectivePersona(agent)).toEqual({ persona: "leader", byRole: true });
    expect(effectivePersona({ ...agent, roles: [] })).toEqual({ persona: "worker", byRole: true });
    expect(effectivePersona({ ...agent, persona: "none" })).toEqual({ persona: "none", byRole: false });
  });
});

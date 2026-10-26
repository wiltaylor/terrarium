import { describe, expect, it } from "vitest";
import type { Agent } from "../../api/types";
import { agentToForm, emptyAgentForm, formToDef, formToPatch, parseList, validateAgentForm } from "./form";

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
});

import { describe, expect, it } from "vitest";
import { agentToForm, formToDef, formToPatch } from "./form";
import { afterSave, applyChoice, byRoleName, choiceLabel, choiceOf, choiceValue, missingPersonaWarning, parseChoice, promptNameProblem } from "./systemPrompt";
import type { Agent } from "../../api/types";

const f = (persona: string, system_prompt = "") => ({ persona, system_prompt });

describe("the system prompt selector model", () => {
  it("maps persona and system_prompt to one choice; the custom text wins", () => {
    expect(choiceOf(f(""))).toEqual({ kind: "role" });
    expect(choiceOf(f("none"))).toEqual({ kind: "none" });
    expect(choiceOf(f("reviewer"))).toEqual({ kind: "library", name: "reviewer" });
    expect(choiceOf(f("reviewer", "Own text."))).toEqual({ kind: "custom" });
    expect(choiceOf(f(""), true)).toEqual({ kind: "custom" });
    expect(choiceOf(f("", "  \n"))).toEqual({ kind: "role" });
  });

  it("round-trips the select values", () => {
    for (const c of [{ kind: "role" }, { kind: "none" }, { kind: "custom" }, { kind: "library", name: "a-b_c" }] as const) expect(parseChoice(choiceValue(c))).toEqual(c);
  });

  it("maps a choice back to the fields: a library entry clears the text", () => {
    const own = f("reviewer", "Own.");
    expect(applyChoice(own, { kind: "library", name: "terse" })).toEqual(f("terse"));
    expect(applyChoice(own, { kind: "role" })).toEqual(f(""));
    expect(applyChoice(own, { kind: "none" })).toEqual(f("none"));
    // Custom keeps the text, else starts from the seed; the persona goes back to by role, except none.
    expect(applyChoice(own, { kind: "custom" })).toEqual(f("", "Own."));
    expect(applyChoice(f("terse"), { kind: "custom" }, "Terse text.")).toEqual(f("", "Terse text."));
    expect(applyChoice(f("none"), { kind: "custom" })).toEqual(f("none", ""));
  });

  it("selects the saved entry after Save as new, and the patch has both fields", () => {
    const agent = { id: "A", name: "w", project: "p", harness: "claude", roles: ["member"], system_prompt: "Mine." } as unknown as Agent;
    const before = agentToForm(agent);
    const after = { ...before, ...afterSave("mine") };
    expect(choiceOf(after)).toEqual({ kind: "library", name: "mine" });
    expect(formToPatch(agent, after)).toEqual({ persona: "mine", system_prompt: null });
    expect(formToDef({ ...after, name: "w", project: "p" })).toMatchObject({ persona: "mine" });
    expect(formToDef({ ...after, name: "w", project: "p" })).not.toHaveProperty("system_prompt");
  });

  it("warns on a persona that the library does not have, only once it loaded", () => {
    expect(missingPersonaWarning("gone", ["leader", "worker"])).toMatch(/not in the system prompt library/);
    expect(missingPersonaWarning("gone", undefined)).toBeNull();
    expect(missingPersonaWarning("worker", ["leader", "worker"])).toBeNull();
    expect(missingPersonaWarning("none", [])).toBeNull();
    expect(missingPersonaWarning("", [])).toBeNull();
  });

  it("checks a new name as the daemon does, and labels a choice", () => {
    expect(promptNameProblem("reviewer-2")).toBeNull();
    for (const bad of ["", "none", "Bad", "-x", "a b"]) expect(promptNameProblem(bad)).not.toBeNull();
    expect(byRoleName(["project_admin"])).toBe("leader");
    expect(byRoleName([])).toBe("worker");
    expect(choiceLabel({ kind: "role" }, ["sysadmin"])).toBe("leader (by role)");
    expect(choiceLabel({ kind: "none" }, [])).toBe("harness default");
  });
});

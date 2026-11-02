import { describe, expect, it } from "vitest";
import type { Agent } from "../../api/types";
import { agentToForm, autoCompactText, compactMaxAgeProblem, effectivePersona, emptyAgentForm, formToDef, formToPatch, idleStopProblem, idleStopText, parseList, personaProblem, validateAgentForm } from "./form";

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

  it("carries the skills: in the definition when set, [] in the patch to clear them", () => {
    const base = { ...emptyAgentForm("p"), name: "w" };
    expect(base.skills).toEqual([]);
    expect(formToDef(base)).not.toHaveProperty("skills");
    expect(formToDef({ ...base, skills: ["fern-check"] }).skills).toEqual(["fern-check"]);
    expect(formToDef({ ...base, skills: ["*"] }).skills).toEqual(["*"]);
    const withS: Agent = { ...agent, skills: ["fern-check", "release-notes"] };
    expect(agentToForm(withS).skills).toEqual(["fern-check", "release-notes"]);
    expect(agentToForm(agent).skills).toEqual([]);
    expect(formToPatch(withS, agentToForm(withS))).toEqual({});
    expect(formToPatch(withS, { ...agentToForm(withS), skills: [] })).toEqual({ skills: [] });
    expect(formToPatch(withS, { ...agentToForm(withS), skills: ["*"] })).toEqual({ skills: ["*"] });
    expect(formToPatch(agent, { ...agentToForm(agent), skills: ["fern-check"] })).toEqual({ skills: ["fern-check"] });
  });

  it("carries skills_off and default_skills: only when not the default, and in the patch when changed", () => {
    const base = { ...emptyAgentForm("p"), name: "w" };
    expect(base.skills_off).toEqual([]);
    expect(base.default_skills).toBe(true);
    expect(formToDef(base)).not.toHaveProperty("skills_off");
    expect(formToDef(base)).not.toHaveProperty("default_skills");
    expect(formToDef({ ...base, skills_off: ["terrarium-db"], default_skills: false })).toMatchObject({ skills_off: ["terrarium-db"], default_skills: false });
    const off: Agent = { ...agent, skills_off: ["terrarium-db"], default_skills: false };
    expect(agentToForm(off)).toMatchObject({ skills_off: ["terrarium-db"], default_skills: false });
    expect(agentToForm(agent)).toMatchObject({ skills_off: [], default_skills: true });
    expect(formToPatch(off, agentToForm(off))).toEqual({});
    expect(formToPatch(off, { ...agentToForm(off), skills_off: [], default_skills: true })).toEqual({ skills_off: [], default_skills: true });
  });

  it("takes off, 0, or a duration of d, h, m, and s parts as the idle stop; empty is the default", () => {
    for (const ok of ["", "  ", "off", "0", "90s", "30m", "2h", "1h30m", "1d2h3m4s", " 10m "]) expect(idleStopProblem(ok)).toBeNull();
    for (const bad of ["10", "m", "1.5h", "10 m", "1h 30m", "-5m", "10min", "OFF", "never", "1w"]) expect(idleStopProblem(bad)).toMatch(/Idle stop/);
    expect(validateAgentForm({ ...emptyAgentForm("p"), name: "w", idle_stop: "10min" })).toEqual([expect.stringContaining("Idle stop")]);
  });

  it("sends the idle stop only when set or changed; an emptied one goes as null", () => {
    expect(formToDef({ ...emptyAgentForm("p"), name: "w" }).idle_stop).toBeUndefined();
    expect(formToDef({ ...emptyAgentForm("p"), name: "w", idle_stop: " 30m " }).idle_stop).toBe("30m");
    const v = agentToForm(agent);
    expect(v.idle_stop).toBe("");
    expect(formToPatch(agent, { ...v, idle_stop: "off" })).toEqual({ idle_stop: "off" });
    expect(formToPatch({ ...agent, idle_stop: "1h" }, { ...v, idle_stop: "1h" })).toEqual({});
    expect(formToPatch({ ...agent, idle_stop: "1h" }, { ...v, idle_stop: "" })).toEqual({ idle_stop: null });
  });

  it("takes on, off, or empty for compact on idle, and the idle stop form for the compact max age", () => {
    const v = agentToForm(agent);
    expect([v.compact_on_idle, v.compact_max_age]).toEqual(["", ""]);
    expect(agentToForm({ ...agent, compact_on_idle: false, compact_max_age: "24h" })).toMatchObject({ compact_on_idle: "off", compact_max_age: "24h" });
    expect(formToDef({ ...emptyAgentForm("p"), name: "w" })).not.toHaveProperty("compact_on_idle");
    expect(formToDef({ ...emptyAgentForm("p"), name: "w", compact_on_idle: "on", compact_max_age: " 24h " })).toMatchObject({ compact_on_idle: true, compact_max_age: "24h" });
    expect(formToPatch(agent, { ...v, compact_on_idle: "off" })).toEqual({ compact_on_idle: false });
    expect(formToPatch({ ...agent, compact_on_idle: true }, { ...agentToForm({ ...agent, compact_on_idle: true }), compact_on_idle: "" })).toEqual({ compact_on_idle: null });
    expect(formToPatch({ ...agent, compact_max_age: "1d" }, { ...v, compact_max_age: "" })).toEqual({ compact_max_age: null });
    for (const ok of ["", "off", "0", "24h", "12h30m"]) expect(compactMaxAgeProblem(ok)).toBeNull();
    expect(validateAgentForm({ ...emptyAgentForm("p"), name: "w", compact_max_age: "1 day" })).toEqual([expect.stringContaining("Compact max age")]);
  });

  it("shows the auto compact settings, the defaults, and the note for a harness that cannot compact", () => {
    const d = { on_idle: false, max_age: "off" };
    expect(autoCompactText({ harness: "claude", compact_supported: true }, d)).toBe("not on idle, no max age (default)");
    expect(autoCompactText({ harness: "claude", compact_on_idle: true }, d)).toBe("on idle, no max age");
    expect(autoCompactText({ harness: "codex", compact_max_age: "24h" }, { on_idle: true })).toBe("on idle, max age 24h");
    expect(autoCompactText({ harness: "pi", compact_supported: false, compact_on_idle: true }, d)).toBe("auto compact not supported for pi");
  });

  it("shows the idle stop with the global default", () => {
    expect(idleStopText(undefined, "10m")).toBe("default (10m)");
    expect(idleStopText(undefined, "off")).toBe("default (never)");
    expect(idleStopText(undefined, undefined)).toBe("default");
    expect(idleStopText("off", "10m")).toBe("never");
    expect(idleStopText("0", "10m")).toBe("never");
    expect(idleStopText("1h30m", "10m")).toBe("after 1h30m idle");
  });
});

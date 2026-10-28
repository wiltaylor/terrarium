import { describe, expect, it } from "vitest";
import { ApiError } from "../../api/http";
import { descriptionProblem, filterSkills, nameProblem, noSkills, previewBody, skillOptions, skillProblems, skillsLabel, toggleSkill, unknownSkills, usedBy, usedByText } from "./model";

describe("skills model", () => {
  it("checks a new name: kebab-case, at most 64", () => {
    expect(nameProblem("fern-check")).toBeNull();
    expect(nameProblem("a1")).toBeNull();
    expect(nameProblem(" ")).toMatch(/Give a name/);
    for (const bad of ["Fern", "fern_check", "-fern", "fern-", "fern--check", "a b", "a".repeat(65)]) expect(nameProblem(bad), bad).not.toBeNull();
    expect(nameProblem("a".repeat(64))).toBeNull();
  });

  it("checks a description: not empty, at most 1024 characters", () => {
    expect(descriptionProblem("Do a thing.")).toBeNull();
    expect(descriptionProblem("  ")).not.toBeNull();
    expect(descriptionProblem("é".repeat(1024))).toBeNull();
    expect(descriptionProblem("a".repeat(1025))).toMatch(/1025/);
  });

  it("reads the problems of a 422 from details.problems, and nothing else", () => {
    expect(skillProblems(new ApiError(422, "x", undefined, "invalid", { problems: ["a", 2, "b"] }))).toEqual(["a", "b"]);
    expect(skillProblems(new ApiError(422, "x", undefined, "invalid", { problems: [] }))).toBeNull();
    expect(skillProblems(new ApiError(422, "x", undefined, "invalid", { field: "name" }))).toBeNull();
    expect(skillProblems(new ApiError(409, "x", undefined, "conflict", { problems: ["a"] }))).toBeNull();
    expect(skillProblems(new Error("x"))).toBeNull();
  });

  it("knows a daemon with no skills: 501, not_available, or no route", () => {
    expect(noSkills(new ApiError(501, "x", undefined, "not_available"))).toBe(true);
    expect(noSkills(new ApiError(404, "no such API route"))).toBe(true);
    expect(noSkills(new ApiError(403, "x", undefined, "forbidden"))).toBe(false);
    expect(noSkills(undefined)).toBe(false);
  });

  it("lists the options by name, and keeps a chosen name that the list does not have", () => {
    expect(skillOptions([{ name: "b" }, { name: "a" }])).toEqual(["a", "b"]);
    expect(skillOptions([{ name: "b" }], ["*", "gone"])).toEqual(["b", "gone"]);
    expect(unknownSkills(["*", "b", "gone"], [{ name: "b" }])).toEqual(["gone"]);
  });

  it("keeps * and the names apart in a pick", () => {
    expect(toggleSkill([], "a", true)).toEqual(["a"]);
    expect(toggleSkill(["a"], "b", true)).toEqual(["a", "b"]);
    expect(toggleSkill(["a"], "a", true)).toEqual(["a"]);
    expect(toggleSkill(["a", "b"], "a", false)).toEqual(["b"]);
    expect(toggleSkill(["a", "b"], "*", true)).toEqual(["*"]);
    expect(toggleSkill(["*"], "a", true)).toEqual(["a"]);
    expect(toggleSkill(["*"], "*", false)).toEqual([]);
  });

  it("labels a skill list", () => {
    expect(skillsLabel(undefined)).toBe("none");
    expect(skillsLabel([])).toBe("none");
    expect(skillsLabel(["*"])).toBe("all skills");
    expect(skillsLabel(["a", "b"])).toBe("a, b");
  });

  it("counts the users of a skill", () => {
    const u = { agents: [{ id: "1", name: "p/a", all: false }, { id: "2", name: "p/b", all: true }], templates: [{ id: "3", name: "worker", all: false }] };
    expect(usedBy(u)).toEqual({ agents: 2, templates: 1, all: 1, total: 3 });
    expect(usedByText(u)).toBe("2 agents, 1 template");
    expect(usedByText({ agents: [], templates: [] })).toBe("nothing");
    expect(usedByText({ agents: [u.agents[0]!], templates: [] })).toBe("1 agent");
  });

  it("previews the body of SKILL.md without the front matter", () => {
    expect(previewBody("---\nname: a\ndescription: b\n---\n\n# A\n")).toBe("\n# A\n");
    expect(previewBody("# No front matter")).toBe("# No front matter");
  });

  it("finds a skill by name or description", () => {
    const list = [
      { name: "fern-check", description: "Deployment checklist code" },
      { name: "release-notes", description: "Notes" },
    ];
    expect(filterSkills(list, "").length).toBe(2);
    expect(filterSkills(list, "FERN").map((s) => s.name)).toEqual(["fern-check"]);
    expect(filterSkills(list, "checklist").map((s) => s.name)).toEqual(["fern-check"]);
  });
});

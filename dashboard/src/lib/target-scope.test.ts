import { describe, expect, it } from "vitest";
import type { Target } from "../api/types";
import { filterByScope, groupByScope, scopeLabel, targetScope, withScope } from "./target-scope";

const t = (name: string, project?: string, scope?: Target["scope"]) => ({ name, project, scope });

describe("targetScope", () => {
  it("uses the daemon's scope when it is there", () => {
    expect(targetScope({ scope: "global", project: undefined })).toBe("global");
    expect(targetScope({ scope: "project", project: "lab" })).toBe("project");
  });

  it("derives the scope from project when the field is absent", () => {
    expect(targetScope({ project: undefined })).toBe("global");
    expect(targetScope({ project: null as unknown as undefined })).toBe("global");
    expect(targetScope({ project: "lab" })).toBe("project");
    expect(withScope({ name: "x", project: "lab" })).toEqual({ name: "x", project: "lab", scope: "project" });
    const bare: { name: string; project?: string } = { name: "y" };
    expect(withScope(bare)).toEqual({ name: "y", scope: "global" });
  });

  it("labels a target with its project name, or global", () => {
    expect(scopeLabel({ project: "lab" })).toBe("lab");
    expect(scopeLabel({})).toBe("global");
  });
});

describe("filterByScope", () => {
  const list = [t("vm", "lab"), t("dbox"), t("vlab", undefined, "global"), t("other", "web")];
  it("keeps all, the project's own, or the global ones", () => {
    expect(filterByScope(list, "all", "lab").map((x) => x.name)).toEqual(["vm", "dbox", "vlab", "other"]);
    expect(filterByScope(list, "project", "lab").map((x) => x.name)).toEqual(["vm"]);
    expect(filterByScope(list, "global", "lab").map((x) => x.name)).toEqual(["dbox", "vlab"]);
  });

  it("keeps the targets of any project for 'project' when no project is given", () => {
    expect(filterByScope(list, "project").map((x) => x.name)).toEqual(["vm", "other"]);
  });
});

describe("groupByScope", () => {
  it("puts the global group first, then one group per project by name, each by target name", () => {
    const g = groupByScope([t("z", "web"), t("vlab"), t("a", "lab"), t("dbox"), t("b", "lab")]);
    expect(g.map((x) => [x.key, x.label, x.targets.map((y) => y.name)])).toEqual([
      ["global", "Global", ["dbox", "vlab"]],
      ["lab", "Project lab", ["a", "b"]],
      ["web", "Project web", ["z"]],
    ]);
  });

  it("answers no groups for no targets", () => {
    expect(groupByScope([])).toEqual([]);
  });
});

import { describe, expect, it } from "vitest";
import { ApiError } from "../api/http";
import type { Target } from "../api/types";
import { capError, capSentence, fullTargetName, holderRows, instanceActions, labSource, runningLabel, runningOnHost, sourceLabel, workspacePath } from "./target-runtime";

describe("fullTargetName (docs/api.md §17.2, the project prefix)", () => {
  it("prefixes a project target with <project>-", () => {
    expect(fullTargetName("box", "terra")).toBe("terra-box");
    expect(fullTargetName(" box ", "terra")).toBe("terra-box");
  });

  it("keeps a name that has the prefix already, and a global name", () => {
    expect(fullTargetName("terra-box", "terra")).toBe("terra-box");
    expect(fullTargetName("terrabox", "terra")).toBe("terra-terrabox");
    expect(fullTargetName("box", null)).toBe("box");
    expect(fullTargetName("", "terra")).toBe("");
  });
});

describe("the lab source of a vmlab target", () => {
  it("names the source, and shows the defaults of ref and path", () => {
    expect(labSource({ kind: "docker", lab: "/x" })).toBeNull();
    expect(sourceLabel({ kind: "vmlab", lab: "/labs/win11" })).toBe("/labs/win11");
    expect(labSource({ kind: "vmlab", lab_repo: "app" })).toBe("repo");
    expect(sourceLabel({ kind: "vmlab", lab_repo: "app", lab_ref: "v2", lab_path: "labs/net" })).toBe("app@v2:labs/net");
    expect(sourceLabel({ kind: "vmlab", lab_repo: "app" })).toBe("app@default:/");
    expect(sourceLabel({ kind: "vmlab", lab_from_workspace: true, lab_path: "lab" })).toBe("workspace:lab");
    expect(sourceLabel({ kind: "vmlab" })).toBeNull();
  });

  it("puts an attached workspace at /work/<project>/<agent>", () => {
    expect(workspacePath("terra/w-ter-3")).toBe("/work/terra/w-ter-3");
  });
});

describe("capError", () => {
  it("reads the 409 project cap from its details", () => {
    const e = new ApiError(409, "project `terra` has 8 targets", undefined, "conflict", { project: "terra", targets: 8, max: 8 });
    const c = capError(e);
    expect(c).toEqual({ cap: "project", message: e.message, project: "terra", targets: 8, max: 8 });
    expect(capSentence(c!)).toBe("Project terra has 8 targets, the most it may have (8). Delete one first.");
  });

  it("reads the 429 host cap from its message", () => {
    const e = new ApiError(429, "host `lab1` runs 4 targets, and its max_running_targets is 4; bring one down first", undefined, "rate_limited");
    const c = capError(e);
    expect(c).toMatchObject({ cap: "host", host: "lab1", running: 4, max: 4 });
    expect(capSentence(c!)).toBe("Host lab1 runs 4 of its 4 targets. Bring one down first.");
    expect(capSentence(capError(new ApiError(429, "slow down", undefined, "rate_limited"))!)).toBe("The host runs its max_running_targets. Bring one down first.");
  });

  it("is null for another 409 and for other errors", () => {
    expect(capError(new ApiError(409, "target `x` exists", undefined, "conflict"))).toBeNull();
    expect(capError(new ApiError(409, "held", undefined, "lease_held", { target: "x", holder: "a" }))).toBeNull();
    expect(capError(new ApiError(422, "bad", undefined, "invalid"))).toBeNull();
    expect(capError(new Error("x"))).toBeNull();
  });
});

describe("runningOnHost (docs/api.md §17.3)", () => {
  const t = (o: Partial<Target>): Target => ({ id: "x", name: "x", kind: "docker", state: "running", ephemeral: false, snapshots: [], host: "lab1", ...o });
  it("counts the running docker and vmlab targets and lab instances of the host", () => {
    const list = [
      t({ name: "a" }),
      t({ name: "b", state: "stopped" }),
      t({ name: "c", kind: "host" }),
      t({ name: "d", host: "lab2" }),
      t({ name: "e", kind: "vmlab", state: "unknown", lab_from_workspace: true, runtime: { instances: { A: { agent_name: "p/a", state: "running" }, B: { agent_name: "p/b", state: "stopped" }, C: { agent_name: "p/c", state: "running" } } } }),
    ];
    expect(runningOnHost(list, "lab1")).toBe(3);
    expect(runningOnHost(list, "lab2")).toBe(1);
    expect(runningLabel(3, { max_running_targets: 4 })).toBe("3 / 4");
    expect(runningLabel(3, {})).toBe("3 (no cap reported)");
  });
});

describe("holderRows", () => {
  it("lists each instance, then each lease holder with no instance", () => {
    const target = { runtime: { instances: { B: { agent_name: "p/bob", state: "running" as const, attached: ["p/bob"] }, A: { agent_name: "p/amy", state: "stopped" as const } } } };
    const rows = holderRows(target, ["B", "C"], (id) => (id === "C" ? "p/cat" : undefined));
    expect(rows.map((r) => [r.agent, r.agent_name, r.instance?.state ?? null])).toEqual([
      ["A", "p/amy", "stopped"],
      ["B", "p/bob", "running"],
      ["C", "p/cat", null],
    ]);
    expect(holderRows({}, ["Z"])).toEqual([{ agent: "Z", agent_name: null, instance: null }]);
  });

  it("offers up for an instance that does not run, and down for one that runs", () => {
    expect(instanceActions(undefined)).toEqual(["up", "status"]);
    expect(instanceActions("stopped")).toEqual(["up", "status"]);
    expect(instanceActions("running")).toEqual(["down", "status"]);
  });
});

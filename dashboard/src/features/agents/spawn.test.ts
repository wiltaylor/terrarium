import { describe, expect, it } from "vitest";
import type { Agent, AgentTemplate } from "../../api/types";
import { agentToForm, formToPatch } from "./form";
import { durationSecs, durationText, lifetimeLeft, orderSpawns, resultPreview, sameSpawn, spawnChip, spawnedTabLabel, spawnProblem, spawnRightsText, templateChoices } from "./spawn";

const agent = (extra: Partial<Agent> = {}): Agent => ({ id: "A1", name: "w", project: "p", harness: "claude", workspace: "/w", status: "idle", roles: [], policy: { targets: [], mail_to: ["*"] }, created_at: "2026-11-02T00:00:00Z", ...extra });

describe("spawn rights in the agent form", () => {
  it("keeps the spawn block in a policy patch and removes it only when turned off", () => {
    const a = agent({ policy: { targets: [], mail_to: ["*"], spawn: { templates: ["project:*"], max_concurrent: 2 } } });
    const v = agentToForm(a);
    expect(formToPatch(a, v)).toEqual({});
    // Another policy change keeps the block.
    expect(formToPatch(a, { ...v, mail_to: "lead" }).policy).toEqual({ targets: [], mail_to: ["lead"], cross_project: [], spawn: { templates: ["project:*"], max_concurrent: 2 } });
    expect(formToPatch(a, { ...v, spawn: null }).policy).toEqual({ targets: [], mail_to: ["*"], cross_project: [] });
    expect(formToPatch(a, { ...v, spawn: { templates: ["*"], max_concurrent: 3 } }).policy?.spawn).toEqual({ templates: ["*"], max_concurrent: 3 });
  });

  it("checks a block as the daemon does", () => {
    expect(spawnProblem({ templates: ["project:*", "global:worker", "w-1"] })).toBeNull();
    expect(spawnProblem({ templates: ["a b"] })).toContain("not a template name");
    expect(spawnProblem({ templates: [], max_concurrent: 0 })).toContain("max concurrent");
    expect(spawnProblem({ templates: [], max_lifetime: "25h" })).toContain("24h");
    expect(spawnProblem({ templates: [], max_lifetime: "soon" })).toContain("not a duration");
    expect(durationSecs("1h30m")).toBe(5400);
    expect(sameSpawn(undefined, null)).toBe(true);
    expect(sameSpawn({ templates: ["a", "b"] }, { templates: ["b", "a"], max_concurrent: 2 })).toBe(true);
    expect(sameSpawn({ templates: ["a"] }, { templates: ["a"], may_spawn_children: true })).toBe(false);
    expect(spawnRightsText(undefined)).toBe("none");
    expect(spawnRightsText({ templates: ["project:*"], max_per_day: 10 })).toBe("project:* · 2 at a time · 10 a day · lifetime 2h");
  });

  it("lists the patterns, then the project's templates, then the global ones", () => {
    const t = (name: string, project: string | null) => ({ id: name, name, project }) as AgentTemplate;
    const c = templateChoices("p", [t("worker", null), t("reviewer", "p"), t("other", "q")]);
    expect(c.map((x) => x.value)).toEqual(["project:*", "global:*", "reviewer", "global:worker"]);
  });

  it("validates bound caps and detects changes without dropping bound rights", () => {
    const spawn = { templates: ["project:*"], bound: true, max_bound_lifetime: "3d" };
    const a = agent({ policy: { spawn } });
    const v = agentToForm(a);
    expect(formToPatch(a, v)).toEqual({});
    expect(formToPatch(a, { ...v, spawn: { ...spawn, bound: false } }).policy?.spawn?.bound).toBe(false);
    expect(formToPatch(a, { ...v, spawn: { ...spawn, max_bound_lifetime: "4d" } }).policy?.spawn?.max_bound_lifetime).toBe("4d");
    expect(formToPatch(a, { ...v, mail_to: "lead" }).policy?.spawn).toEqual(spawn);
    expect(sameSpawn({ templates: [] }, { templates: [], bound: false })).toBe(true);
    expect(spawnProblem(spawn)).toBeNull();
    expect(spawnProblem({ ...spawn, max_bound_lifetime: "30d" })).toBeNull();
    for (const max_bound_lifetime of ["0s", "soon"]) {
      expect(spawnProblem({ ...spawn, max_bound_lifetime })).toContain("not a duration");
    }
    expect(spawnProblem({ ...spawn, max_bound_lifetime: "31d" })).toContain("30d");
    expect(spawnRightsText(spawn)).toContain("bound agents allowed (cap 3d)");
    expect(spawnRightsText({ templates: [], bound: true })).toContain("cap 30d");
  });
});

describe("the Spawned tab rows", () => {
  it("shows the status while it runs and the outcome after", () => {
    expect(spawnChip({ status: "working" })).toEqual({ label: "working", cls: "ok" });
    expect(spawnChip({ status: "finished", outcome: "done", finished_at: "x" })).toEqual({ label: "done", cls: "ok" });
    expect(spawnChip({ status: "finished", outcome: "timeout", finished_at: "x" }).cls).toBe("warn");
    expect(spawnChip({ status: "finished", outcome: "failed", finished_at: "x" }).cls).toBe("danger");
    expect(resultPreview("one\ntwo\nthree")).toBe("one · two");
    expect(resultPreview(undefined)).toBe("");
    expect(durationText(252)).toBe("4m 12s");
    expect(durationText(7500)).toBe("2h 5m");
    expect(spawnedTabLabel(0)).toBe("Spawned");
    expect(spawnedTabLabel(2)).toBe("Spawned (2)");
  });

  it("puts the running ones first and tells the lifetime left", () => {
    const row = (id: string, finished?: string) => ({ id, finished_at: finished }) as never;
    expect(orderSpawns([row("c", "x"), row("b"), row("a", "y")]).map((s: { id: string }) => s.id)).toEqual(["b", "c", "a"]);
    const now = Date.parse("2026-11-02T10:00:00Z");
    const spawn = { owner_name: "lead", template: "worker", task: "t", spawned_at: "2026-11-02T09:00:00Z", deadline_at: "2026-11-02T11:03:00Z" };
    expect(lifetimeLeft({ spawn }, now)).toBe("lifetime left 1h 3m");
    expect(lifetimeLeft({ spawn: { ...spawn, deadline_at: "2026-11-02T09:59:00Z" } }, now)).toBe("lifetime over");
  });
});

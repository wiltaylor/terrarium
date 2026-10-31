import { describe, expect, it } from "vitest";
import type { Agent } from "../api/types";
import { projectHealth, projectHealthLabel } from "./projectHealth";

type A = Pick<Agent, "status" | "status_reason" | "paused">;
const a = (status: Agent["status"], more: Partial<A> = {}): A => ({
  status,
  ...more,
});

describe("projectHealth", () => {
  it("is none, with no counts, for a project with no agents", () => {
    expect(projectHealth([])).toEqual({ level: "none", counts: {}, total: 0 });
    expect(projectHealthLabel("p", projectHealth([]))).toBe("p: no agents");
  });

  it("is red when an agent is in error or blocked, over all other states", () => {
    expect(projectHealth([a("idle"), a("working"), a("error")]).level).toBe("red");
    expect(projectHealth([a("blocked"), a("starting"), a("idle")]).level).toBe("red");
    expect(projectHealth([a("working"), a("blocked")]).level).toBe("red");
  });

  it("keeps an agent in error red when it is paused", () => {
    expect(projectHealth([a("error", { paused: true })]).level).toBe("red");
  });

  it("is yellow when an agent works, starts, restarts, or compacts", () => {
    for (const s of ["working", "starting", "restarting"] as const) expect(projectHealth([a("idle"), a(s)]).level).toBe("yellow");
    expect(projectHealth([a("idle"), a("working", { status_reason: "compacting" })]).level).toBe("yellow");
    expect(projectHealth([a("idle", { status_reason: "compacting" })]).counts).toEqual({ compacting: 1 });
  });

  it("is green when all agents rest: idle, waiting, stopped, idle stopped, paused", () => {
    const h = projectHealth([
      a("idle"),
      a("waiting"),
      a("stopped"),
      a("stopped", { status_reason: "idle" }),
      a("stopped", { paused: true }),
      a("idle", { paused: true }),
    ]);
    expect(h.level).toBe("green");
    expect(h.counts).toEqual({ idle: 1, waiting: 1, stopped: 2, paused: 2 });
    expect(h.total).toBe(6);
  });

  it("gives the counts in the label, the worst first", () => {
    const h = projectHealth([a("idle"), a("working"), a("idle"), a("blocked"), a("working"), a("idle")]);
    expect(projectHealthLabel("terrarium", h)).toBe("terrarium: 1 blocked, 2 working, 3 idle");
  });
});

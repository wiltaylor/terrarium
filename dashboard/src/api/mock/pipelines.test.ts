// The pipelines of the mock answer in the daemon's shapes (docs/api.md §31).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent } from "../types";

const mock = (opts = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...opts });

describe("mock pipelines", () => {
  it("seeds a GitHub pipeline, a script pipeline, and one that fails", async () => {
    const api = mock();
    const ps = await api.pipelines.list("terrarium");
    expect(new Set(ps.map((p) => p.source.kind))).toEqual(new Set(["github", "script"]));
    const broken = ps.find((p) => p.error)!;
    expect(broken.error).toContain("404");
    const ci = ps.find((p) => p.name === "ci")!;
    expect(ci.status).not.toBeNull();
    expect(ci.latest?.id).toBeTruthy();
    const runs = await api.pipelines.runs(ci.id);
    expect(runs.length).toBeGreaterThan(1);
    const d = await api.pipelines.run(ci.id, runs[0]!.id);
    expect(d.adapter).toBe("github");
    expect(d.steps?.length).toBeGreaterThan(0);
  });

  it("creates, refreshes, and emits events; checks the fields", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    api.events.stream({ since: api.world.seq }).subscribe((e) => frames.push(e));
    await new Promise((r) => setTimeout(r, 5));
    const p = await api.pipelines.create("terrarium", { name: "octo", source: { kind: "script", target: "pi-runner", script: "python3 x.py" } });
    expect(p.schedule).toEqual({ kind: "interval", every: "5m" });
    expect(p.max_runs).toBe(20);
    const r = await api.pipelines.refresh(p.id);
    expect(r.output?.exit_code).toBe(0);
    expect(r.pipeline.polled_at).toBeTruthy();
    await new Promise((res) => setTimeout(res, 5));
    expect(frames.some((e) => e.kind === "pipeline.status")).toBe(true);
    await expect(api.pipelines.create("terrarium", { name: "octo", source: { kind: "github", repo: "o/r" } })).rejects.toMatchObject({ status: 409 });
    await expect(api.pipelines.create("terrarium", { name: "bad", source: { kind: "github", repo: "nope" } })).rejects.toMatchObject({ status: 422 });
    const t = await api.pipelines.test("terrarium", { source: { kind: "github", repo: "o/r" } });
    expect(t.runs.length).toBeGreaterThan(0);
    expect((await api.pipelines.list("terrarium")).some((x) => x.name === "test-poll")).toBe(false);
    const u = await api.pipelines.update(p.id, { enabled: false });
    expect(u.next_run_at).toBeNull();
    await api.pipelines.remove(p.id);
    await expect(api.pipelines.get(p.id)).rejects.toMatchObject({ status: 404 });
  });
});

// The monitors of the mock answer in the daemon's shapes (docs/api.md §30).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent } from "../types";
import { firstNumber, parseOutput } from "./monitors";

const mock = (opts = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...opts });

describe("mock monitors", () => {
  it("seeds a monitor of each kind, a week of history, and a dashboard with every block kind", async () => {
    const api = mock();
    const ms = await api.monitors.list("terrarium");
    expect(new Set(ms.map((m) => m.kind))).toEqual(new Set(["number", "status", "text", "json"]));
    expect(ms.length).toBeGreaterThanOrEqual(8);
    const backup = ms.find((m) => m.name === "nightly-backup")!;
    expect(backup.last?.status).toBe("unknown");
    expect(backup.last?.error).toContain("target down");
    const d = await api.monitors.dashboard("terrarium");
    expect(new Set(d.blocks.map((b) => b.kind))).toEqual(new Set(["note", "stat", "status", "chart", "text", "table"]));
    const load = ms.find((m) => m.name === "load")!;
    const week = await api.monitors.series(load.id, { since: "7d", bucket: 3600 });
    expect(week.length).toBeGreaterThan(100);
    expect(week[0]).toHaveProperty("count");
  });

  it("creates, runs, and emits a status change", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    api.events.stream({ since: api.world.seq }).subscribe((e) => frames.push(e));
    await new Promise((r) => setTimeout(r, 5));
    const m = await api.monitors.create("terrarium", { name: "probe", target: "pi-runner", script: "echo 1", kind: "number", schedule: { kind: "interval", every: "30s" }, warn: 0, crit: 1000 });
    expect(m.timeout_secs).toBe(30);
    const r = await api.monitors.run(m.id);
    expect(r.result.status).toBe("warn");
    expect(r.output?.exit_code).toBe(0);
    await new Promise((res) => setTimeout(res, 5));
    const st = frames.filter((e) => e.kind === "monitor.status");
    expect(st).toHaveLength(1);
    expect(st[0]!.data).toMatchObject({ monitor: "probe", from: null, to: "warn", project: "terrarium" });
    expect(frames.some((e) => e.kind === "monitor.result")).toBe(true);
    // A test run keeps nothing.
    const t = await api.monitors.test("terrarium", { target: "pi-runner", script: "x", kind: "text" });
    expect(t.result.status).toBe("ok");
    expect((await api.monitors.list("terrarium")).some((x) => x.name === "test-run")).toBe(false);
  });

  it("checks the fields and the layout as the daemon does", async () => {
    const api = mock();
    await expect(api.monitors.create("terrarium", { name: "fast", target: "pi-runner", script: "x", kind: "number", schedule: { kind: "interval", every: "10s" } })).rejects.toMatchObject({ status: 422 });
    await expect(api.monitors.create("terrarium", { name: "other", target: "pg-test", script: "x", kind: "number", schedule: { kind: "interval", every: "1m" } })).rejects.toMatchObject({ status: 422 });
    const svc = (await api.monitors.list("terrarium")).find((m) => m.kind === "status")!;
    await expect(api.monitors.saveDashboard("terrarium", { blocks: [{ id: "a", kind: "stat", monitor: svc.id, size: "s", options: {} }] })).rejects.toMatchObject({ status: 422 });
    const v = await api.monitors.saveDashboard("terrarium", { blocks: [{ id: "a", kind: "status", monitor: svc.id, size: "s", options: {} }] });
    expect(v.blocks).toHaveLength(1);
    await api.monitors.remove(svc.id);
    expect((await api.monitors.dashboard("terrarium")).blocks).toHaveLength(0);
  });

  it("refuses writes to a member", async () => {
    const api = mock({ scheduleCaller: { role: "member", project: "terrarium" } });
    const m = (await api.monitors.list("terrarium"))[0]!;
    await expect(api.monitors.run(m.id)).rejects.toMatchObject({ status: 403 });
    await expect(api.monitors.saveDashboard("terrarium", { blocks: [] })).rejects.toMatchObject({ status: 403 });
  });

  it("reads output as the daemon does", () => {
    expect(firstNumber("used: 1,234.5 GB")).toBe(1234.5);
    expect(firstNumber("temp=-3")).toBe(-3);
    expect(firstNumber("a-5")).toBe(5);
    expect(parseOutput("status", {}, "down\n", 2)).toMatchObject({ status: "crit", text: "down" });
    expect(parseOutput("number", { warn: 80, crit: 90 }, "85%", 0)).toMatchObject({ status: "warn", value: 85 });
    expect(parseOutput("number", { warn: 20, crit: 5, direction: "below" }, "3", 0).status).toBe("crit");
    expect(parseOutput("json", {}, "{nope", 0).status).toBe("unknown");
  });
});

describe("mock monitor owners", () => {
  it("keeps the owner, moves it with one edit, and lists the target grants", async () => {
    const api = mock();
    const m = await api.monitors.create("terrarium", { name: "owned", target: "pi-runner", script: "echo 1", kind: "number", schedule: { kind: "interval", every: "30s" }, owner: "project" });
    expect(m.owner).toBe("project");
    const back = await api.monitors.update(m.id, { owner: "actor" });
    expect(back.owner).toBe("actor");
    expect(back.run_as).toBe("operator");
    const plain = await api.monitors.create("terrarium", { name: "plain", target: "pi-runner", script: "echo 1", kind: "number", schedule: { kind: "interval", every: "30s" } });
    expect(plain.owner).toBe("actor");
    const grants = await api.projects.targetGrants("terrarium");
    expect(grants[0]).toMatchObject({ target: "pi-runner", project: "terrarium", for: "monitors" });
    expect(await api.projects.targetGrants("webshop")).toEqual([]);
  });
});

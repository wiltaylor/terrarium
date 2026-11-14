import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import { callersText, claimText, filterServices, isOpen, outcomeText, runsText, scopeOptions, stateCounts, stateTone } from "./model";

describe("services model", () => {
  it("knows the open states and a tone for each state", () => {
    expect(["queued", "waiting", "running"].every((s) => isOpen(s as never))).toBe(true);
    expect(["done", "failed", "cancelled", "timed_out"].some((s) => isOpen(s as never))).toBe(false);
    expect(stateTone("done")).toBe("ok");
    expect(stateTone("timed_out")).toBe("bad");
    expect(stateTone("cancelled")).toBe("");
  });

  it("counts the states in flow order and drops the empty ones", () => {
    expect(stateCounts([{ state: "done" }, { state: "queued" }, { state: "done" }])).toEqual([
      { state: "queued", count: 1 },
      { state: "done", count: 2 },
    ]);
  });

  it("writes the claim, the runner, and the callers in words", () => {
    expect(claimText(undefined)).toBe("none");
    expect(claimText({ cpu_millis: 4000, memory: "16GiB", gpus: 1, gpu_exclusive: true })).toBe("4 CPU, 16GiB memory, 1 GPU, exclusive");
    expect(claimText({ cpu_millis: 1500, gpus: 2, gpu_vram: "8GiB" })).toBe("1.5 CPU, 2 GPUs, 8GiB VRAM each");
    expect(runsText({ kind: "script", command: ["/opt/run", "-q"] })).toBe("/opt/run -q");
    expect(runsText({ kind: "agent", template: "t1" })).toBe("template t1");
    expect(callersText({ callers: [], project: "p" })).toBe("the agents of project p");
    expect(callersText({ callers: [] })).toBe("its admins only");
    expect(callersText({ callers: ["project:a", "agent:b/c"] })).toBe("project:a, agent:b/c");
  });

  it("filters by project or global and lists global first", () => {
    const list = [{ name: "b", project: "z" }, { name: "a" }, { name: "c", project: "m" }];
    expect(scopeOptions(list)).toEqual(["global", "m", "z"]);
    expect(filterServices(list, "").map((s) => s.name)).toEqual(["a", "b", "c"]);
    expect(filterServices(list, "global").map((s) => s.name)).toEqual(["a"]);
    expect(filterServices(list, "m").map((s) => s.name)).toEqual(["c"]);
  });

  it("writes the outcome: the queue place, or the exit code and the error", () => {
    expect(outcomeText({ state: "queued", position: 2 })).toBe("#2 in the queue");
    expect(outcomeText({ state: "failed", exit_code: 2, error: "out of memory" })).toBe("exit 2: out of memory");
    expect(outcomeText({ state: "running" })).toBe("");
  });
});

describe("services mock", () => {
  it("lists the queue newest first with queue places, filters by state, and cancels only an open request", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    expect((await api.services.list()).map((s) => s.name)).toEqual(["doc-summary", "video-gen"]);
    const rows = await api.services.requests("video-gen");
    expect(rows.map((r) => r.id)).toEqual([...rows.map((r) => r.id)].sort().reverse());
    const queued = await api.services.requests("video-gen", { state: "queued" });
    expect(queued.map((r) => r.position)).toEqual([2, 1]);
    expect((await api.services.get("video-gen")).open_requests).toBe(3);
    const next = queued[1]!;
    expect((await api.services.cancel(next.id)).state).toBe("cancelled");
    expect((await api.services.requests("video-gen", { state: "queued" }))[0]!.position).toBe(1);
    await expect(api.services.cancel(next.id)).rejects.toMatchObject({ status: 409 });
    await expect(api.services.get("nope")).rejects.toMatchObject({ status: 404 });
  });
});

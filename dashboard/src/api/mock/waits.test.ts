// The wait and watch routes of the mock: the daemon's shapes and events.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent, ProcessWaitResult } from "../types";
import { untilMatches } from "./waits";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));
const listen = async (api: ReturnType<typeof mock>) => {
  const frames: AgentEvent[] = [];
  const s = api.events.stream({ since: api.world.seq });
  s.subscribe((e) => frames.push(e));
  await wait();
  return { frames, close: () => s.close() };
};

describe("waits in the mock", () => {
  it("seeds a waiting agent with waits, waiting_on, and open_waits on the detail only", async () => {
    const api = mock();
    const planner = (await api.agents.list()).find((a) => a.name === "planner")!;
    expect(planner.status).toBe("waiting");
    expect(planner.waits).toBe(3);
    expect(planner.waiting_on).toContain("agent terrarium/runner-dev (done)");
    expect(planner.open_waits).toBeUndefined();
    const detail = await api.agents.get("terrarium/planner");
    expect(detail.open_waits).toHaveLength(3);
    // An agent with no waits has neither field.
    const lead = await api.agents.get("terrarium/lead");
    expect(lead).not.toHaveProperty("waits");
    expect(lead).not.toHaveProperty("waiting_on");
    expect(lead).not.toHaveProperty("open_waits");
  });

  it("lists the open waits; all=true adds the closed ones, newest first; kind filters", async () => {
    const api = mock();
    const open = await api.agents.waits("terrarium/planner");
    expect(open.every((w) => w.state === "open")).toBe(true);
    expect(open.map((w) => w.created_at)).toEqual([...open.map((w) => w.created_at)].sort().reverse());
    const all = await api.agents.waits("terrarium/planner", { all: true });
    expect(all.length).toBeGreaterThan(open.length);
    const closed = all.filter((w) => w.state !== "open");
    expect(closed.map((w) => w.resolved_at)).toEqual([...closed.map((w) => w.resolved_at)].sort().reverse());
    const proc = await api.agents.waits("terrarium/planner", { all: true, kind: "process" });
    expect(proc.length).toBe(2);
    expect(proc.every((w) => w.kind === "process")).toBe(true);
    const w = open[0]!;
    expect(Object.keys(w)).toEqual(expect.arrayContaining(["id", "agent", "agent_name", "project", "kind", "spec", "summary", "state", "created_by", "created_at"]));
    expect(await api.waits.get(w.id)).toEqual(w);
    await expect(api.waits.get("nope")).rejects.toMatchObject({ status: 404, code: "not_found" });
  });

  it("cancels a wait with a wait event, refuses a second cancel, and kill=true ends the process run", async () => {
    const api = mock();
    const l = await listen(api);
    const [proc] = await api.agents.waits("terrarium/planner", { kind: "process" });
    const killed = await api.waits.cancel(proc!.id, { kill: true });
    expect(killed.state).toBe("cancelled");
    expect(killed.result).toMatchObject({ exit_code: null, signal: "SIGKILL", reason: "killed" });
    const e = l.frames.find((x) => x.kind === "wait")!;
    expect(e.agent_id).toBe(proc!.agent);
    expect(e.data).toMatchObject({ wait_id: proc!.id, change: "cancelled", kind: "process", summary: proc!.summary, state: "cancelled" });
    await expect(api.waits.cancel(proc!.id)).rejects.toMatchObject({ status: 409 });
    expect((await api.agents.get("terrarium/planner")).waits).toBe(2);
    l.close();
  });

  it("moves the waiter from waiting to idle when its last wait closes, with waits in the status event", async () => {
    const api = mock();
    const l = await listen(api);
    for (const w of await api.agents.waits("terrarium/planner")) await api.waits.cancel(w.id);
    const planner = await api.agents.get("terrarium/planner");
    expect(planner.status).toBe("idle");
    expect(planner).not.toHaveProperty("waits");
    const st = l.frames.filter((x) => x.kind === "status" && x.agent === "planner");
    expect(st.at(-1)!.data).toMatchObject({ status: "idle", from: "waiting" });
    expect(st.at(-1)!.data).not.toHaveProperty("waits");
    l.close();
  });

  it("closes an agent wait when the watched agent is done", async () => {
    const api = mock();
    const l = await listen(api);
    // runner-dev is working; a restart brings it to idle with no waits: done.
    await api.agents.restart("terrarium/runner-dev");
    await wait(20);
    const resolved = l.frames.find((x) => x.kind === "wait" && x.data.change === "resolved")!;
    expect(resolved.data).toMatchObject({ kind: "agent", state: "resolved", summary: "agent terrarium/runner-dev (done)" });
    expect((await api.waits.get(String(resolved.data.wait_id))).result).toEqual({ outcome: "idle" });
    l.close();
  });

  it("answers the log of a process wait as text, with end when closed; another kind is 422", async () => {
    const api = mock();
    const all = await api.agents.waits("terrarium/planner", { all: true, kind: "process" });
    const running = all.find((w) => w.state === "open")!;
    const ended = all.find((w) => w.state !== "open")!;
    const log = await api.waits.log(running.id);
    expect(log).toMatchObject({ wait_id: running.id, target: "build-box", running: true });
    expect(typeof log.output).toBe("string");
    expect(log.end).toBeUndefined();
    const done = await api.waits.log(ended.id, { max: 16 });
    expect(done.running).toBe(false);
    expect(done.output.length).toBeLessThanOrEqual(16);
    expect(done.end).toMatchObject({ code: (ended.result as ProcessWaitResult).exit_code });
    const [agentWait] = await api.agents.waits("terrarium/planner", { kind: "agent" });
    await expect(api.waits.log(agentWait!.id)).rejects.toMatchObject({ status: 422 });
  });

  it("matches until values as the daemon does", () => {
    expect(untilMatches("done", "idle")).toBe(true);
    expect(untilMatches("done", "waiting")).toBe(false);
    expect(untilMatches("idle", "waiting")).toBe(true);
    expect(untilMatches("blocked", "error")).toBe(false);
    expect(untilMatches("any_change", "stopped")).toBe(true);
  });
});

describe("watches in the mock", () => {
  it("lists the watches on an agent and by it", async () => {
    const api = mock();
    const onRunner = await api.agents.watches("terrarium/runner-dev");
    expect(onRunner.map((w) => w.watcher_name)).toEqual(["terrarium/planner"]);
    const byPlanner = await api.agents.watches("terrarium/planner");
    expect(byPlanner.map((w) => w.agent_name)).toEqual(["terrarium/runner-dev"]);
    expect(await api.watches.list({ watcher: "operator" })).toHaveLength(1);
  });

  it("creates an operator watch with 201 shape and a watch event; busy is working; bad values are 422", async () => {
    const api = mock();
    const l = await listen(api);
    const w = await api.agents.watch("terrarium/lead", { on: ["idle", "busy", "idle"], once: true });
    expect(w).toMatchObject({ watcher: "operator", watcher_name: "operator", agent_name: "terrarium/lead", on: ["idle", "working"], once: true, debounce: true, fired: 0 });
    expect(l.frames.find((x) => x.kind === "watch")!.data).toMatchObject({ watch_id: w.id, change: "created", on: ["idle", "working"] });
    await expect(api.agents.watch("terrarium/lead", { on: [] })).rejects.toMatchObject({ status: 422 });
    await expect(api.agents.watch("terrarium/lead", { on: ["dancing" as "idle"] })).rejects.toMatchObject({ status: 422 });
    expect((await api.watches.list({ watcher: "operator" })).map((x) => x.id)).toContain(w.id);
    l.close();
  });

  it("fires a watch on a status change, mails the watcher, and drops a once watch", async () => {
    const api = mock();
    const w = await api.agents.watch("terrarium/sysadmin", { on: ["idle"], once: true });
    const l = await listen(api);
    await api.agents.start("terrarium/sysadmin");
    await wait(20);
    const fired = l.frames.find((x) => x.kind === "watch" && x.data.change === "fired")!;
    expect(fired.data).toMatchObject({ watch_id: w.id, status: "idle" });
    expect(l.frames.some((x) => x.kind === "watch" && x.data.change === "removed")).toBe(true);
    expect(api.world.mail.some((m) => m.to === "operator" && m.subject === "terrarium/sysadmin is idle")).toBe(true);
    expect((await api.watches.list()).map((x) => x.id)).not.toContain(w.id);
    l.close();
  });

  it("removes a watch; a second remove is 404", async () => {
    const api = mock();
    const [w] = await api.watches.list({ watcher: "operator" });
    await api.watches.remove(w!.id);
    await expect(api.watches.remove(w!.id)).rejects.toMatchObject({ status: 404 });
  });
});

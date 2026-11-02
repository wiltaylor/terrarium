// Ephemeral agents in the mock (docs/api.md §6.10): the seed, the lists that
// leave them out, a spawn, a cancel, and the removal of the owner.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import { lifetimeMinutes, spawnAllows } from "./spawns";
import { waitFor } from "../../test/wait";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });

describe("ephemeral agents in the mock", () => {
  it("seeds lead with spawn rights, two running and three finished", async () => {
    const api = mock();
    const lead = await api.agents.get("terrarium/lead");
    expect(lead.policy?.spawn?.templates).toEqual(["global:worker", "project:*"]);
    expect(lead.spawned_running).toBe(2);
    const page = await api.agents.spawned(lead.id);
    expect(page.items.map((s) => s.name)).toEqual(["lead-reviewer-2", "lead-worker-3", "lead-worker-2", "lead-worker-1", "lead-reviewer-1"]);
    expect(page.items.filter((s) => !s.finished_at)).toHaveLength(2);
    const done = page.items.find((s) => s.outcome === "done")!;
    expect(done.result).toContain("2 of 20 runs failed");
    // The session of a finished one still opens.
    const ses = await api.sessions.get(done.session_id!);
    expect(ses.removed).toBe(true);
    expect((await api.sessions.transcript(done.session_id!)).blocks.length).toBeGreaterThan(1);
  });

  it("leaves them out of the agent list unless asked", async () => {
    const api = mock();
    const names = (await api.agents.list("terrarium")).map((a) => a.name);
    expect(names).not.toContain("lead-worker-3");
    const all = (await api.agents.list("terrarium", { ephemeral: true })).map((a) => a.name);
    expect(all).toContain("lead-worker-3");
    const eph = await api.agents.get("terrarium/lead-worker-3");
    expect(eph).toMatchObject({ ephemeral: true, owner: "01JB0LEAD00000000000000001", idle_stop: "off" });
    expect(eph.spawn?.owner_name).toBe("lead");
  });

  it("spawns with the task as the first turn, and cancels with a mail to the owner", async () => {
    const api = mock();
    const s = await api.agents.spawn("terrarium/lead", { template: "worker", task: "Count the files\nin src" });
    expect(s.name).toBe("lead-worker-4");
    expect(s.template).toBe("global:worker");
    expect(s.summary).toBe("Count the files");
    await waitFor(() => expect(api.world.agents.find((a) => a.id === s.id)?.status).toBe("idle"));
    const ev = api.world.events.filter((e) => e.kind === "agent.spawned" && e.data?.spawn_id === s.id);
    expect(ev).toHaveLength(1);
    expect(ev[0]!.agent).toBe("lead");
    const c = await api.agents.cancel(s.id);
    expect(c).toMatchObject({ outcome: "cancelled", status: "finished" });
    expect(api.world.agents.some((a) => a.id === s.id)).toBe(false);
    expect(api.world.mail.some((m) => m.subject === "Spawned lead-worker-4 finished: cancelled")).toBe(true);
    expect(api.world.events.some((e) => e.kind === "agent.finished" && e.data?.spawn_id === s.id)).toBe(true);
    // A pause of an ephemeral agent is refused; the operator's own needs a project.
    const other = await api.agents.spawn("operator", { template: "worker", task: "x", project: "terrarium" });
    expect(other.owner).toBe("operator");
    await expect(api.agents.pause(other.id)).rejects.toMatchObject({ status: 409 });
    await expect(api.agents.spawn("operator", { template: "worker", task: "x" })).rejects.toMatchObject({ status: 422 });
  });

  it("removes the ephemeral agents and their rows with the owner", async () => {
    const api = mock();
    const lead = api.world.agents.find((a) => a.name === "lead")!;
    // The seeded lead comes from a file: an API agent here.
    lead.source = "api";
    await api.agents.remove(lead.id, true);
    expect(api.world.agents.some((a) => a.ephemeral)).toBe(false);
    expect((api.world.spawns ?? []).some((s) => s.owner === lead.id)).toBe(false);
  });

  it("reads lifetimes and template rules as the daemon does", () => {
    expect(lifetimeMinutes(undefined)).toBe(120);
    expect(lifetimeMinutes("1h30m")).toBe(90);
    expect(lifetimeMinutes("soon")).toBeNull();
    const sp = { templates: ["worker", "global:helper", "project:*"] };
    expect(spawnAllows(sp, "worker", true)).toBe(true);
    expect(spawnAllows(sp, "helper", true)).toBe(true);
    expect(spawnAllows(sp, "other", false)).toBe(true);
    expect(spawnAllows(sp, "other", true)).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import type { Agent, Target } from "../../api/types";
import { createGrantDialog, eligibleAgents, eligibleTargets, emptyGrantForm, grantBody, grantTitle } from "./grant";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const NOW = Date.parse("2026-10-25T09:00:00.000Z");

describe("grantBody", () => {
  it("sends the agent, the duration, exclusive, and a trimmed reason", () => {
    expect(grantBody(emptyGrantForm({ target: "vm", agent: "01A", exclusive: true, reason: "  TER-3 " }), NOW)).toEqual({ body: { agent: "01A", until: "30m", exclusive: true, reason: "TER-3" } });
  });

  it("leaves until out for 'until revoked', and the reason out when empty", () => {
    expect(grantBody(emptyGrantForm({ target: "vm", agent: "01A", untilMode: "none" }), NOW)).toEqual({ body: { agent: "01A", exclusive: false } });
  });

  it("says what is missing", () => {
    expect(grantBody(emptyGrantForm({ agent: "01A" }), NOW)).toEqual({ error: "Choose a target." });
    expect(grantBody(emptyGrantForm({ target: "vm" }), NOW)).toEqual({ error: "Choose an agent." });
    expect(grantBody(emptyGrantForm({ target: "vm", agent: "01A", duration: "soon" }), NOW)).toHaveProperty("error");
  });
});

describe("the agent and target pickers", () => {
  const a = (name: string, project: string) => ({ id: name, name, project }) as Agent;
  const t = (name: string, project?: string) => ({ name, project }) as Target;
  it("lists the agents of a project target, and all agents for a global target", () => {
    const list = [a("w2", "terra"), a("shop", "web"), a("w1", "terra")];
    expect(eligibleAgents(list, t("vm", "terra")).map((x) => x.name)).toEqual(["w1", "w2"]);
    expect(eligibleAgents(list, t("box")).map((x) => x.name)).toEqual(["w1", "w2", "shop"]);
  });
  it("lists the global targets and the agent's project targets", () => {
    expect(eligibleTargets([t("vm", "terra"), t("pg", "web"), t("box")], a("w1", "terra")).map((x) => x.name)).toEqual(["box", "vm"]);
  });
});

describe("createGrantDialog", () => {
  it("shows the lease_held refusal with the holder and its expiry, then a field change clears it", async () => {
    const api = mock();
    const codex = api.world.agents.find((x) => x.name === "adapter-codex")!;
    const runner = api.world.agents.find((x) => x.name === "runner-dev")!;
    const held = api.world.leases.find((l) => l.target === "lab-win11" && l.state === "active")!;
    const dlg = createGrantDialog(api, { target: "lab-win11", agent: codex.id, reason: "TER-15" });
    expect(await dlg.submit()).toBeNull();
    const o = dlg.outcome();
    expect(o.kind).toBe("held");
    if (o.kind !== "held") throw new Error("not held");
    expect(o.held).toMatchObject({ target: "lab-win11", holder: `terrarium/${runner.name}`, holder_id: runner.id, lease_id: held.id, exclusive: true, expires_at: held.expires_at });
    expect(o.message).toContain("held exclusively by terrarium/runner-dev");
    dlg.set("exclusive", true);
    expect(dlg.outcome().kind).toBe("idle");
  });

  it("keeps a form error apart from an API error, and does not send it", async () => {
    const api = mock();
    const before = api.world.leases.length;
    const dlg = createGrantDialog(api, { target: "lab-win11", agent: "", duration: "30m" });
    expect(await dlg.submit()).toBeNull();
    expect(dlg.outcome()).toEqual({ kind: "invalid", message: "Choose an agent." });
    expect(api.world.leases.length).toBe(before);
    // A webshop agent on a terrarium target: 422 invalid, a plain error.
    dlg.set("agent", api.world.agents.find((x) => x.name === "shop-lead")!.id);
    expect(await dlg.submit()).toBeNull();
    const o = dlg.outcome();
    expect(o.kind).toBe("error");
    expect(o.kind === "error" && (o.error as { code: string }).code).toBe("invalid");
  });

  it("grants, and answers the lease", async () => {
    const api = mock();
    const lead = api.world.agents.find((x) => x.name === "lead")!;
    const dlg = createGrantDialog(api, { target: "build-box", agent: lead.id, untilMode: "duration", duration: "2h" });
    const l = await dlg.submit();
    expect(l).toMatchObject({ target: "build-box", agent: lead.id, agent_name: "terrarium/lead", exclusive: false, state: "active", granted_by: "operator" });
    expect(dlg.outcome().kind).toBe("done");
    expect(Date.parse(l!.expires_at!) - Date.now()).toBeGreaterThan(119 * 60_000);
  });
});

describe("the grant dialog and the target scope", () => {
  const a = (name: string, project: string) => ({ id: name, name, project }) as Agent;
  const list = [a("w1", "lab"), a("shop", "web")];
  it("lists only the project's agents for a project target, and all agents for a global target, with or without scope", () => {
    expect(eligibleAgents(list, { project: "lab", scope: "project" }).map((x) => x.name)).toEqual(["w1"]);
    expect(eligibleAgents(list, { project: undefined, scope: "global" }).map((x) => x.name)).toEqual(["w1", "shop"]);
    expect(eligibleAgents(list, { project: undefined }).map((x) => x.name)).toEqual(["w1", "shop"]);
  });

  it("shows the scope in the title", () => {
    expect(grantTitle({ name: "vm", project: "lab", scope: "project" })).toBe("Grant vm · project lab");
    expect(grantTitle({ name: "dbox", scope: "global" })).toBe("Grant dbox · global target");
    expect(grantTitle({ name: "vlab" })).toBe("Grant vlab · global target");
  });
});

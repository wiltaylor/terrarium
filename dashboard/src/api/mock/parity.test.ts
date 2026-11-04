// The mock must answer in the daemon's shapes (docs/api.md), so mock mode and
// real mode never diverge.
import { describe, expect, it } from "vitest";
import { ApiError, AuthError, createHttpTransport, isMutating } from "../http";
import { createMockApi, type MockOptions } from ".";
import type { AgentEvent } from "../types";
import { bearerProtocol, type AuthSocket, type MakeRawSocket, type PtySocket } from "../sockets";
import { FakePty } from "./pty";
import { acceptHandshake, MOCK_ORIGIN, MOCK_PASSWORD, MOCK_RULES, MOCK_TOKEN, type HandshakeRules } from "./ws";
import { eachMethod, httpVerbs, NOT_TRANSPORT } from "./verbs";
import { fakeClock, waitFor } from "../../test/wait";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));
/** A mock whose fake turns take some time, so that a test sees the agent busy. */
const slowMock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 30 });

describe("mock parity with docs/api.md", () => {
  it("answers a reply with an array, one message per participant", async () => {
    const api = mock();
    const first = (await api.mail.list({ state: "unread" })).items.find((m) => m.to === "operator")!;
    const sent = await api.mail.reply(first.id, "On it.");
    expect(Array.isArray(sent)).toBe(true);
    expect(sent.length).toBeGreaterThan(0);
    for (const m of sent) {
      expect(m.thread_id).toBe(first.thread_id);
      expect(m.from).toBe("operator");
      expect(m.in_reply_to).toBe(first.id);
      expect(m).toHaveProperty("to_name");
      expect(m).toHaveProperty("from_kind");
    }
    expect((await api.mail.get(first.id)).state).toBe("replied");
  });

  it("replies from the project inbox as project:<name>", async () => {
    const api = mock();
    const m = (await api.projectInbox.list("terrarium")).items[0]!;
    const sent = await api.projectInbox.reply("terrarium", m.id, "Accepted.");
    expect(sent.every((x) => x.from === "project:terrarium" && x.from_kind === "project")).toBe(true);
  });

  it("answers start with the old status, then emits status events", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const a = await api.agents.start("terrarium/sysadmin");
    expect(a.status).toBe("stopped");
    const statuses = await waitFor(() => {
      const st = frames.filter((e) => e.kind === "status");
      expect(st.map((e) => e.data.status)).toEqual(["starting", "idle"]);
      return st;
    });
    const e = statuses[0]!;
    expect(e).toMatchObject({ kind: "status", actor: "system", agent: "sysadmin", agent_id: a.id, project: "terrarium" });
    expect(e.data).toMatchObject({ agent_id: a.id, status: "starting", from: "stopped", reason: null });
    expect(typeof e.seq).toBe("number");
    expect(statuses[1]!.seq).toBeGreaterThan(e.seq);
    await expect(api.agents.start(a.id)).rejects.toMatchObject({ code: "already_running", status: 409 });
    s.close();
  });

  it("queues a turn to a stopped agent, wakes it, and then runs the turn", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    expect(await api.agents.send("terrarium/sysadmin", "hello")).toEqual({ accepted: true, queued: true });
    await waitFor(() => expect(frames.some((e) => e.kind === "turn_start" && e.agent === "sysadmin")).toBe(true));
    expect(frames.find((e) => e.kind === "agent.woken")).toMatchObject({ agent: "sysadmin", data: { cause: "turn" } });
    expect((await api.agents.get("terrarium/sysadmin")).queued).toBeUndefined();
    s.close();
  });

  it("pauses a live agent, keeps turns queued, refuses a start, and wakes on the unpause when work waits", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const a = await api.agents.pause("terrarium/lead");
    expect(a.paused).toBe(true);
    await waitFor(() => expect(api.world.agents.find((x) => x.name === "lead")).toMatchObject({ status: "stopped", status_reason: "paused" }));
    // Idempotent.
    expect((await api.agents.pause("terrarium/lead")).paused).toBe(true);
    expect(frames.filter((e) => e.kind === "agent.paused")).toHaveLength(1);
    await expect(api.agents.start("terrarium/lead")).rejects.toMatchObject({ status: 409, code: "conflict", message: expect.stringContaining("paused") });
    expect(await api.agents.send("terrarium/lead", "later")).toEqual({ accepted: true, queued: true });
    await wait();
    expect(api.world.agents.find((x) => x.name === "lead")).toMatchObject({ status: "stopped", queued: 1 });
    expect((await api.agents.unpause("terrarium/lead")).paused).toBe(false);
    await waitFor(() => expect(frames.some((e) => e.kind === "turn_start" && e.agent === "lead")).toBe(true));
    expect(frames.find((e) => e.kind === "agent.woken")).toMatchObject({ data: { cause: "unpause" } });
    s.close();
  });

  it("checks idle_stop on the agent create and patch, and a null patch goes back to the default", async () => {
    const api = mock();
    expect((await api.health()).idle_stop).toBe("10m");
    const a = await api.agents.create({ name: "idler", project: "terrarium", harness: "claude", idle_stop: "1h30m" });
    expect(a).toMatchObject({ idle_stop: "1h30m", paused: false });
    await expect(api.agents.update(a.id, { idle_stop: "soon" })).rejects.toMatchObject({ status: 422 });
    expect((await api.agents.update(a.id, { idle_stop: "off" })).idle_stop).toBe("off");
    expect((await api.agents.update(a.id, { idle_stop: null })).idle_stop).toBeUndefined();
  });

  it("checks the auto compact settings, and compacts an idle agent by hand", async () => {
    const api = mock();
    const h = await api.health();
    expect(h).toMatchObject({ compact_on_idle: false, compact_max_age: "off", compact_harnesses: ["claude", "codex", "copilot", "pi"] });
    const a = await api.agents.create({ name: "packer", project: "terrarium", harness: "claude", compact_on_idle: true, compact_max_age: "24h" });
    expect(a).toMatchObject({ compact_on_idle: true, compact_max_age: "24h", compact_supported: true });
    await expect(api.agents.update(a.id, { compact_max_age: "soon" })).rejects.toMatchObject({ status: 422 });
    const b = await api.agents.update(a.id, { compact_on_idle: null, compact_max_age: null });
    expect(b.compact_on_idle).toBeUndefined();
    expect(b.compact_max_age).toBeUndefined();
    // Not running: 409 not_running.
    await expect(api.agents.compact(a.id)).rejects.toMatchObject({ status: 409 });
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    // The lead of the seed is idle.
    const first = await api.agents.compact("terrarium/lead");
    expect(first.reset).toEqual({ kind: "compact", state: "started", replaced: false, deferred: false, then_set: false });
    expect(api.world.agents.find((x) => x.name === "lead")).toMatchObject({ status: "working", status_reason: "compacting" });
    // A second request while the agent compacts waits for the end (202, `queued`).
    const second = await api.agents.compact("terrarium/lead");
    expect(second.reset).toMatchObject({ kind: "compact", state: "queued", replaced: false });
    expect(second.reset_pending).toMatchObject({ kind: "compact", by: "operator", then_set: false, recap: false, deferred: false });
    expect(frames.filter((e) => e.kind === "agent.reset").map((e) => e.data?.state)).toEqual(["started", "queued"]);
    await waitFor(() => expect(frames.filter((e) => e.kind === "agent.compacted")).toHaveLength(2), 6000);
    await waitFor(() => expect(api.world.agents.find((x) => x.name === "lead")).toMatchObject({ status: "idle", last_compact_ok: true }));
    expect(api.world.agents.find((x) => x.name === "lead")!.reset_pending).toBeUndefined();
    s.close();
  }, 10000);

  it("refuses a compaction that the daemon refuses, and checks the then prompt", async () => {
    const api = mock();
    await expect(api.agents.compact("terrarium/lead", { then: "   " })).rejects.toMatchObject({ status: 400 });
    await expect(api.agents.compact("terrarium/lead", { then: "x".repeat(32 * 1024 + 1) })).rejects.toMatchObject({ status: 400 });
    await expect(api.agents.newSession("terrarium/lead", { then: "" })).rejects.toMatchObject({ status: 400 });
    await api.agents.pause("terrarium/lead");
    await expect(api.agents.compact("terrarium/lead")).rejects.toMatchObject({ status: 409, message: expect.stringContaining("paused") });
    const audit = (await api.audit({ limit: 500 })).items.map((x) => x.action);
    expect(audit).not.toContain("agent.compact");
  });

  it("queues a compaction of a busy agent, replaces it, and sends the then prompt after the compaction", async () => {
    const api = slowMock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    const lead = () => api.world.agents.find((x) => x.name === "lead")!;
    await api.agents.send("terrarium/lead", "work on TER-12");
    await waitFor(() => expect(lead().status).toBe("working"));
    const a = await api.agents.compact("terrarium/lead", { then: "first" });
    expect(a.reset).toEqual({ kind: "compact", state: "queued", replaced: false, deferred: false, then_set: true });
    const b = await api.agents.compact("terrarium/lead", { then: "Continue with TER-12: write the tests.", defer: true });
    expect(b.reset).toEqual({ kind: "compact", state: "queued", replaced: true, deferred: true, then_set: true });
    expect(b.reset_pending).toMatchObject({ kind: "compact", by: "operator", then_set: true, deferred: true });
    expect(frames.find((e) => e.kind === "agent.reset" && e.data?.replaced === true)?.data).toMatchObject({ kind: "compact", by: "operator", state: "queued" });
    await waitFor(() => expect(frames.some((e) => e.kind === "agent.compacted")).toBe(true), 6000);
    await waitFor(() => expect(frames.some((e) => e.kind === "turn_start" && e.data?.text === "Continue with TER-12: write the tests.")).toBe(true), 6000);
    expect(frames.some((e) => e.kind === "turn_start" && e.data?.text === "first")).toBe(false);
    expect(frames.filter((e) => e.kind === "agent.compacted")).toHaveLength(1);
    const session = (await api.agents.sessions("terrarium/lead")).items.find((x) => x.current)!;
    const blocks = (await api.agents.transcript("terrarium/lead", session.id, { limit: 500 })).blocks;
    expect(blocks.some((x) => x.text === "[System:reset requested by operator] Continue with TER-12: write the tests.")).toBe(true);
    s.close();
  }, 15000);

  it("interrupts a running turn: the turn stops, a system block shows it, and the agent is idle (issue #12)", async () => {
    const api = slowMock();
    const lead = () => api.world.agents.find((x) => x.name === "lead")!;
    await expect(api.agents.interrupt("terrarium/lead")).rejects.toMatchObject({ status: 409, code: "conflict" });
    await api.agents.send("terrarium/lead", "work on TER-12");
    await waitFor(() => expect(lead().status).toBe("working"));
    expect(await api.agents.interrupt("terrarium/lead")).toEqual({ accepted: true, interrupted: true });
    expect(lead()).toMatchObject({ status: "idle", status_reason: "interrupted" });
    const session = (await api.agents.sessions("terrarium/lead")).items.find((x) => x.current)!;
    const blocks = async () => (await api.agents.transcript("terrarium/lead", session.id, { limit: 500 })).blocks;
    const after = (await blocks()).length;
    expect((await blocks()).at(-1)).toMatchObject({ role: "system", kind: "system", text: "[Request interrupted by user]" });
    // The steps of the stopped turn do not come.
    await wait(300);
    expect((await blocks()).length).toBe(after);
    expect(lead().status).toBe("idle");
    const audit = (await api.audit({ limit: 500 })).items.find((x) => x.action === "agent.interrupt");
    expect(audit).toMatchObject({ target: lead().id, project: "terrarium" });
  });

  it("starts a new session for an agent at rest, with a recap and the then prompt", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    const before = (await api.agents.sessions("terrarium/lead")).items.find((x) => x.current)!;
    const r = await api.agents.newSession("terrarium/lead", { then: "Go on with TER-12." });
    expect(r.reset).toEqual({ kind: "new_session", state: "started", replaced: false, deferred: false, then_set: true });
    expect(r.reset_pending).toBeUndefined();
    expect(api.world.agents.find((x) => x.name === "lead")).toMatchObject({ status: "starting", status_reason: "new session: requested by operator" });
    const list = (await api.agents.sessions("terrarium/lead")).items;
    const now = list.find((x) => x.current)!;
    expect(now.id).not.toBe(before.id);
    expect(list.find((x) => x.id === before.id)).toMatchObject({ current: false, outcome: "new_session" });
    await waitFor(() => expect(frames.some((e) => e.kind === "turn_start" && e.data?.text === "Go on with TER-12.")).toBe(true), 6000);
    const blocks = (await api.agents.transcript("terrarium/lead", now.id, { limit: 500 })).blocks.map((x) => x.text ?? "");
    const recap = blocks.findIndex((t) => t.startsWith("[System:recap] "));
    const then = blocks.indexOf("[System:reset requested by operator] Go on with TER-12.");
    expect(recap).toBeGreaterThan(-1);
    expect(then).toBeGreaterThan(recap);
    expect((await api.audit({ limit: 500 })).items.some((x) => x.action === "agent.new_session")).toBe(true);
    expect(frames.find((e) => e.kind === "agent.reset")?.data).toMatchObject({ kind: "new_session", by: "operator", then_set: true, deferred: false, replaced: false, state: "started" });
    s.close();
  }, 15000);

  it("starts a stopped agent in a new session with a then prompt, and waits for the next wake without one", async () => {
    const api = mock();
    const stopped = api.world.agents.find((x) => x.status === "stopped" && !x.paused && x.project === "terrarium")!;
    const id = stopped.id;
    const r = await api.agents.newSession(id, { recap: false });
    expect(r.reset).toMatchObject({ kind: "new_session", state: "next_wake", replaced: false });
    expect(r.reset_pending).toMatchObject({ kind: "new_session", by: "operator", then_set: false, recap: false });
    expect(api.world.agents.find((x) => x.id === id)!.status).toBe("stopped");
    // A turn wakes the agent: the wake starts the new session.
    await api.agents.send(id, "hello");
    await waitFor(() => expect(api.world.agents.find((x) => x.id === id)!.status_reason).toBe("new session: requested by operator"));
    expect(api.world.agents.find((x) => x.id === id)!.reset_pending).toBeUndefined();
    await waitFor(() => expect(["idle", "working", "waiting"]).toContain(api.world.agents.find((x) => x.id === id)!.status));

    const api2 = mock();
    const b = api2.world.agents.find((x) => x.id === id)!;
    const s2 = await api2.agents.newSession(id, { then: "Start TER-12." });
    expect(s2.reset).toMatchObject({ kind: "new_session", state: "started" });
    expect(b).toMatchObject({ status: "starting", status_reason: "new session: requested by operator" });
    expect((await api2.agents.sessions(id)).items.filter((x) => x.current)).toHaveLength(1);
  }, 15000);

  it("queues a new session of a busy or paused agent, and runs it when the turn ends", async () => {
    const api = slowMock();
    const lead = () => api.world.agents.find((x) => x.name === "lead")!;
    await api.agents.send("terrarium/lead", "work on TER-12");
    await waitFor(() => expect(lead().status).toBe("working"));
    const q = await api.agents.newSession("terrarium/lead");
    expect(q.reset).toEqual({ kind: "new_session", state: "queued", replaced: false, deferred: false, then_set: false });
    expect(q.reset_pending).toMatchObject({ kind: "new_session", recap: true });
    // A compaction request replaces the new session that waits.
    const c = await api.agents.compact("terrarium/lead");
    expect(c.reset).toMatchObject({ kind: "compact", state: "queued", replaced: true });
    const n = await api.agents.newSession("terrarium/lead", { recap: false });
    expect(n.reset).toMatchObject({ kind: "new_session", state: "queued", replaced: true });
    await waitFor(() => expect(lead().status_reason).toBe("new session: requested by operator"), 6000);
    expect(lead().reset_pending).toBeUndefined();

    await api.agents.pause("terrarium/lead");
    const p = await api.agents.newSession("terrarium/lead");
    expect(p.reset).toMatchObject({ state: "queued" });
    expect(p.reset_pending).toMatchObject({ kind: "new_session" });
  }, 15000);

  it("emits agent_removed when an agent is deleted, after the stop", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const a = api.world.agents.find((x) => x.source !== "file")!;
    await api.agents.remove(a.id, true);
    const rm = await waitFor(() => {
      const r = frames.filter((e) => e.kind === "agent_removed");
      expect(r).toHaveLength(1);
      return r;
    });
    expect(rm[0]).toMatchObject({ agent: a.name, agent_id: a.id, project: a.project, data: { agent_id: a.id, project: a.project, name: a.name } });
    s.close();
  });

  it("links agent comments to sessions that outlive agent rm (docs/api.md §6.6)", async () => {
    const api = mock();
    const cards = (await api.cards.list("terrarium")).items;
    const card = cards.find((c) => c.comments.some((cm) => cm.session))!;
    const cm = card.comments.find((x) => x.session)!;
    const ref = cm.session!;
    expect(ref).toMatchObject({ agent_id: cm.author, project: "terrarium", removed: false });
    expect(card.comments.filter((x) => x.author === "operator").every((x) => !x.session)).toBe(true);
    expect((await api.cards.sessions(card.id)).map((s) => s.id)).toContain(ref.id);
    expect(await api.sessions.get(ref.id)).toMatchObject({ id: ref.id, agent: ref.agent, removed: false });

    await api.agents.remove(ref.agent_id, true);
    const after = await api.cards.get(card.id);
    expect(after.comments.find((x) => x.id === cm.id)!.session).toMatchObject({ id: ref.id, removed: true, agent: ref.agent });
    const listed = (await api.cards.sessions(card.id)).find((s) => s.id === ref.id)!;
    expect(listed).toMatchObject({ removed: true });
    expect(listed.ended_at).toBeTruthy();
    const page = await api.sessions.transcript(ref.id);
    expect(page).toMatchObject({ session: ref.id, harness: ref.harness, active: false });
    await expect(api.sessions.get("NOPE")).rejects.toMatchObject({ status: 404 });
  });

  it("emits card events with data.card_id, the column, and the assignee as agent", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    await api.cards.update("TER-16", { column: "done" });
    const e = frames.find((x) => x.kind === "card")!;
    expect(e).toMatchObject({ card: "TER-16", agent: "lead", project: "terrarium" });
    expect(e.data).toMatchObject({ card_id: "TER-16", change: "moved", from: "review", to: "done", column: "done" });
    s.close();
  });

  it("replays the backlog after ?since= on the socket, like the daemon", async () => {
    const api = mock();
    const seen: number[] = [];
    const s = api.events.stream({ since: api.world.seq - 3 });
    s.subscribe((e) => seen.push(e.seq));
    await wait();
    expect(seen).toEqual([api.world.seq - 2, api.world.seq - 1, api.world.seq]);
    s.close();
  });

  it("pages lists with a cursor, oldest first", async () => {
    const api = mock();
    const p1 = await api.events.history({ since: 0, limit: 5 });
    expect(p1.items).toHaveLength(5);
    expect(p1.next).toBe(String(p1.items.at(-1)!.seq));
    const p2 = await api.events.history({ since: 0, limit: 5, cursor: p1.next! });
    expect(p2.items[0]!.seq).toBe(p1.items.at(-1)!.seq + 1);
    const audit = await api.audit({ limit: 2 });
    expect(audit.items).toHaveLength(2);
    expect(audit.next).not.toBeNull();
    expect(typeof audit.items[0]!.detail).toBe("object");
  });

  it("pages events newest first with order=desc, down to `since`", async () => {
    const api = mock();
    const all = (await api.events.history({ since: 0, limit: 1000 })).items.map((e) => e.seq);
    const p1 = await api.events.history({ order: "desc", limit: 3 });
    expect(p1.items.map((e) => e.seq)).toEqual(all.slice(-3).reverse());
    expect(p1.next).toBe(String(p1.items.at(-1)!.seq));
    const p2 = await api.events.history({ order: "desc", limit: 3, cursor: p1.next! });
    expect(p2.items.map((e) => e.seq)).toEqual(all.slice(-6, -3).reverse());
    // `since` is the lower bound: the last page stops at it.
    const since = all.at(-5)!;
    const q = { order: "desc" as const, since, limit: 3 };
    const a = await api.events.history(q);
    const b = await api.events.history({ ...q, cursor: a.next! });
    expect([...a.items, ...b.items].map((e) => e.seq)).toEqual(all.slice(-4).reverse());
    expect(b.next).toBeNull();
    // The agent events take the same order.
    const agent = (await api.events.history({ since: 0, limit: 1000 })).items.find((e) => e.agent_id)!.agent_id!;
    const mine = await api.agents.events(agent, { order: "desc", limit: 2 });
    expect(mine.items.length).toBeGreaterThan(0);
    expect(mine.items.map((e) => e.seq)).toEqual(mine.items.map((e) => e.seq).sort((x, y) => y - x));
  });

  it("names the tool call ID data.tool_id, not data.id (the event's own id)", async () => {
    const api = mock();
    const page = await api.events.history({ since: 0, limit: 1000 });
    const tools = page.items.filter((e) => e.kind === "tool_use" || e.kind === "tool_finished");
    expect(tools.length).toBeGreaterThan(0);
    for (const e of tools) {
      expect(typeof e.data.tool_id).toBe("string");
      expect(e.data).not.toHaveProperty("id");
    }
  });

  it("takes an agent key as a ULID, project/name, or a unique name", async () => {
    const api = mock();
    const byKey = await api.agents.get("terrarium/lead");
    expect((await api.agents.get(byKey.id)).name).toBe("lead");
    expect((await api.agents.get("lead")).id).toBe(byKey.id);
    const err = await api.agents.get("terrarium/nobody").catch((e: unknown) => e);
    expect((err as ApiError).code).toBe("not_found");
  });

  it("makes card IDs from the project's card_prefix", async () => {
    const api = mock();
    const p = await api.projects.create({ name: "alpine" });
    expect(p.card_prefix).toBe("ALP");
    const q = await api.projects.create({ name: "alpha" });
    expect(q.card_prefix).toBe("ALPH");
    const c = await api.cards.create("alpine", { title: "First" });
    expect(c.id).toBe("ALP-1");
    expect((await api.board.get("alpine")).card_prefix).toBe("ALP");
    await expect(api.projects.update("alpine", { card_prefix: "XYZ" })).rejects.toMatchObject({ code: "conflict" });
  });

  it("refuses a project delete with agents, and an agent delete with a live session without confirm", async () => {
    const api = mock();
    await expect(api.projects.remove("webshop")).rejects.toMatchObject({ code: "conflict" });
    await expect(api.agents.remove("terrarium/lead")).rejects.toMatchObject({ code: "file_managed" });
    await expect(api.agents.remove("terrarium/reviewer")).rejects.toMatchObject({ code: "confirm_required" });
    await api.agents.remove("terrarium/runner-dev", true);
    await expect(api.agents.get("terrarium/runner-dev")).rejects.toMatchObject({ status: 404 });
  });

  it("answers 404 'no such API route' for endpoints the daemon does not have", async () => {
    const api = mock();
    const repo = (await api.projects.repos("terrarium"))[0]!;
    await expect(api.repos.checkout(repo.id, {})).rejects.toMatchObject({ status: 404, message: "no such API route" });
  });

  it("creates an agent from a full definition, stopped", async () => {
    const api = mock();
    const a = await api.agents.create({
      name: "triage",
      project: "webshop",
      harness: "pi",
      subscription: "openrouter-key",
      model: "openai/gpt-5",
      prompt: "worker",
      roles: ["project_admin"],
      policy: { targets: ["pg-test"], mail_to: ["*"], cross_project: ["terrarium/lead"] },
      restart: "never",
    });
    expect(a).toMatchObject({ status: "stopped", roles: ["project_admin"], restart: "never", source: "api" });
    const patched = await api.agents.update(a.id, { model: "openai/gpt-5-mini" });
    expect(patched.model).toBe("openai/gpt-5-mini");
    expect(await api.agents.setRoles(a.id, ["member", "sysadmin"])).toEqual(["sysadmin"]);
  });

  it("refuses a stale wiki base with 409 stale_base and the diff in details", async () => {
    const api = mock();
    const pg = await api.wiki.get("terrarium/designs/runner-v2");
    const saved = await api.wiki.save(pg.id, { body: `${pg.body}\nNew line.\n`, base: pg.rev, summary: "add a line" });
    expect(saved.rev).toBe(pg.rev + 1);
    const err = (await api.wiki.save(pg.id, { body: "x", base: pg.rev, summary: "stale" }).catch((e: unknown) => e)) as ApiError;
    expect(err.status).toBe(409);
    expect(err.code).toBe("stale_base");
    expect(err.details).toMatchObject({ base: pg.rev, current: pg.rev + 1 });
    expect((err.details as { diff: string }).diff).toContain("+ New line.");
  });

  it("makes the index page on the first list, and reads status from front matter", async () => {
    const api = mock();
    await api.projects.create({ name: "fresh" });
    const tree = await api.wiki.tree("fresh");
    expect(tree.map((p) => [p.path, p.author])).toEqual([["index", "system"]]);
    const pg = await api.wiki.create("fresh", { path: "notes", body: "---\nstatus: ready_for_review\n---\n# Notes\n", summary: "start" });
    expect(pg).toMatchObject({ title: "Notes", status: "ready_for_review", rev: 1 });
    expect((await api.wiki.tree("fresh", { status: "ready_for_review" })).map((p) => p.path)).toEqual(["notes"]);
  });

  it("serves artifacts through a minted URL, not a url field", async () => {
    const api = mock();
    const a = (await api.artifacts.list("terrarium")).find((x) => x.id === "ART-2")!;
    expect(a).not.toHaveProperty("url");
    expect(a.key).toBe("terrarium/ART-2");
    expect(a.embedded_in).toEqual(["terrarium/reports/phase-1"]);
    const signed = await api.artifacts.url(a.uid);
    expect(signed.exp).toBeGreaterThan(0);
    expect(await api.artifacts.text(signed.url)).toContain("running 14 tests");
    const up = await api.artifacts.uploadJson("terrarium", { name: "n.txt", content_type: "text/plain", data_base64: "aGk=", attach: { kind: "card", ref: "TER-12" } });
    expect(up.id).toBe("ART-6");
    expect(up.links).toMatchObject([{ kind: "card", ref: "TER-12" }]);
  });

  it("answers the memory graph as {root, depth, nodes, edges} with links in both directions", async () => {
    const api = mock();
    const g = await api.memories.graph("terrarium/MEM-4", 1);
    expect(g.root).toBe("MEM-4");
    expect(g.edges.every((e) => g.nodes.some((n) => n.id === e.from) && g.nodes.some((n) => n.id === e.to))).toBe(true);
    const m1 = await api.memories.get("terrarium/MEM-1");
    expect(m1.links.some((l) => l.direction === "in" && l.other === "MEM-4")).toBe(true);
    const created = await api.memories.create("terrarium", { title: "Stall timeout notes", tags: ["runner"] });
    expect(created.similar.map((x) => x.id)).toContain("MEM-6");
  });

  it("follows the version flow and answers compare in the documented shape", async () => {
    const api = mock();
    const v = await api.versions.create("terrarium", { name: "v0.2.0", notes: "next", cards: ["TER-13"] });
    expect(v.cards).toEqual([{ id: "TER-13", title: "Stall timeout marks agents blocked", column: "ready" }]);
    const submitted = await api.versions.transition("terrarium/v0.2.0", "submit");
    expect(submitted.status).toBe("in_review");
    expect(submitted.reviews.at(-1)!.decision).toBe("submitted");
    await expect(api.versions.transition(v.id, "reject")).rejects.toMatchObject({ code: "comment_required" });
    await expect(api.versions.update(v.id, { notes: "x" })).rejects.toMatchObject({ code: "conflict" });
    const c = await api.versions.compare(v.id, "terrarium/v0.1.0");
    expect(c.cards.added).toEqual(["TER-13"]);
    expect(c.cards.removed.sort()).toEqual(["TER-11", "TER-12", "TER-16"]);
  });

  // ----- docs/api.md §16: subscriptions, quota, usage, login sessions -----

  const SUB_KEYS = ["name", "provider", "harnesses", "alert_at", "source", "login_status", "probe_status", "created_at", "updated_at", "windows", "local_estimate_only", "has_api_key", "held_agents"];

  it("answers subscriptions in the §16.1 shape", async () => {
    const api = mock();
    const list = await api.subscriptions.list();
    expect(list.length).toBeGreaterThan(0);
    for (const s of list) {
      for (const k of SUB_KEYS) expect(s).toHaveProperty(k);
      expect(s).not.toHaveProperty("status");
      expect(s).not.toHaveProperty("history");
      expect(["logged_in", "expiring", "expired", "missing", "unknown", "api_key"]).toContain(s.login_status);
      expect(["ok", "failed", "none"]).toContain(s.probe_status);
      expect(["file", "api"]).toContain(s.source);
      expect(s.local_estimate_only).toBe(!s.probe);
      for (const w of s.windows) {
        expect(Object.keys(w).sort()).toEqual(["at", "resets_at", "used_pct", "window"]);
      }
    }
    const failed = list.find((s) => s.probe_status === "failed")!;
    expect(failed.probe_error).toBeTruthy();
    expect(failed.last_success_at).toBeTruthy();
    const samples = await api.subscriptions.quota("anthropic-max", { window: "5h" });
    expect(samples.length).toBeGreaterThan(1);
    expect(samples[0]).toMatchObject({ subscription: "anthropic-max", window: "5h" });
    expect(samples[0]!.at <= samples.at(-1)!.at).toBe(true);
    expect(samples[0]).toHaveProperty("raw");
  });

  it("creates and deletes subscriptions with the §16.3 errors", async () => {
    const api = mock();
    const s = await api.subscriptions.create({ name: "claude-team", provider: "anthropic", harnesses: ["claude"], probe: "anthropic-oauth-usage" });
    expect(s).toMatchObject({ name: "claude-team", source: "api", login_status: "unknown", probe_status: "none", alert_at: 80, windows: [], local_estimate_only: false, has_api_key: false, held_agents: 0 });
    await expect(api.subscriptions.create({ name: "claude-team", provider: "anthropic" })).rejects.toMatchObject({ status: 409, code: "conflict" });
    await expect(api.subscriptions.create({ name: "x1", provider: "anthropic", probe: "nope" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.create({ name: "login", provider: "anthropic" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.remove("anthropic-max")).rejects.toMatchObject({ status: 409, code: "file_managed" });
    await api.subscriptions.remove("claude-team");
    await expect(api.subscriptions.get("claude-team")).rejects.toMatchObject({ status: 404 });
  });

  it("answers a failed probe with 200 and probe_status failed, and a good probe with a quota.sample event", async () => {
    const api = mock();
    const fresh = await api.subscriptions.create({ name: "codex-new", provider: "openai", harnesses: ["codex"], probe: "openai-codex-usage" });
    const bad = await api.subscriptions.probe(fresh.name);
    expect(bad).toMatchObject({ subscription: "codex-new", probe_status: "failed", windows: [] });
    expect(typeof bad.error).toBe("string");
    expect((await api.subscriptions.get("codex-new")).probe_status).toBe("failed");
    const none = await api.subscriptions.probe("openrouter-key");
    expect(none).toEqual({ subscription: "openrouter-key", probe_status: "none", windows: [] });

    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    const ok = await api.subscriptions.probe("anthropic-max");
    expect(ok.probe_status).toBe("ok");
    expect(ok.windows[0]).toHaveProperty("raw");
    const e = frames.find((x) => x.kind === "quota.sample")!;
    expect(e.data).toMatchObject({ subscription: "anthropic-max" });
    expect((e.data.windows as unknown[]).length).toBe(2);
    st.close();
  });

  it("answers verify as {subscription, status} and logout as the subscription", async () => {
    const api = mock();
    const v = await api.subscriptions.verify("anthropic-max");
    expect(v.subscription.name).toBe("anthropic-max");
    expect(v.status).toMatchObject({ logged_in: true, harness: "claude", provider: "anthropic" });
    const key = await api.subscriptions.verify("openrouter-key");
    expect(key.status).toEqual({ logged_in: true, detail: "a stored API key" });
    // PLAN §16.3: no logout while agents run on the account.
    await expect(api.subscriptions.logout("anthropic-max")).rejects.toMatchObject({ status: 409 });
    const out = await api.subscriptions.logout("anthropic-max-2");
    expect(out.login_status).toBe("missing");
    expect(out.account).toMatchObject({ harness: "claude", has_login: false });
    await expect(api.subscriptions.logout("openrouter-key")).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("shows each plan account with the agents on it (PLAN §16.3)", async () => {
    const api = mock();
    const list = await api.subscriptions.list();
    for (const s of list) expect(Array.isArray(s.agents)).toBe(true);
    const max = list.find((s) => s.name === "anthropic-max")!;
    expect(max.account).toMatchObject({ harness: "claude", has_login: true, source: "migration" });
    expect(max.agents.map((a) => a.name)).toContain("runner-dev");
    expect(list.find((s) => s.name === "anthropic-max-2")!.account?.source).toBe("login");
    expect(list.find((s) => s.name === "openrouter-key")).not.toHaveProperty("account");
  });

  it("marks the default account of each harness and moves it with makeDefault (PLAN §16.3)", async () => {
    const api = mock();
    const list = await api.subscriptions.list();
    for (const s of list) expect(typeof s.default).toBe("boolean");
    const defaults = list.filter((s) => s.default).map((s) => `${s.account?.harness}:${s.name}`).sort();
    expect(defaults).toEqual(["claude:anthropic-max", "codex:chatgpt-pro", "copilot:copilot-pro"]);

    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    const made = await api.subscriptions.makeDefault("anthropic-max-2");
    expect(made.default).toBe(true);
    expect((await api.subscriptions.get("anthropic-max")).default).toBe(false);
    await wait();
    const e = frames.find((x) => x.kind === "subscription")!;
    expect(e.data).toMatchObject({ change: "default", harness: "claude", from: "anthropic-max", to: "anthropic-max-2" });
    const audit = (await api.audit({ action: "subscription.default" })).items.find((r) => r.target === "anthropic-max-2")!;
    expect(audit.detail).toMatchObject({ harness: "claude", from: "anthropic-max", to: "anthropic-max-2" });

    // Not a plan account of Claude Code, Codex, or Copilot.
    await expect(api.subscriptions.makeDefault("openrouter-key")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.makeDefault("zai-key")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.makeDefault("nope")).rejects.toMatchObject({ status: 404 });

    // The logout of the default picks no new default.
    await api.subscriptions.logout("anthropic-max-2");
    const after = await api.subscriptions.list();
    expect(after.filter((s) => s.account?.harness === "claude" && s.default)).toEqual([]);
    expect(frames.some((x) => x.kind === "subscription" && x.data.change === "default_cleared")).toBe(true);
    st.close();
  });

  it("edits the pool with PATCH and checks each member", async () => {
    const api = mock();
    const a = await api.agents.update("01JB0SYS000000000000000007", { subscription_pool: ["anthropic-max-2"] });
    expect(a.subscription_pool).toEqual(["anthropic-max", "anthropic-max-2"]);
    await expect(api.agents.update(a.id, { subscription_pool: ["chatgpt-pro"] })).rejects.toMatchObject({ status: 422 });
    await expect(api.agents.update(a.id, { subscription_pool: ["nope"] })).rejects.toMatchObject({ status: 422 });
    expect((await api.agents.update(a.id, { subscription_pool: [] })).subscription_pool).toBeUndefined();
  });

  it("swaps an agent to another account of its pool and says how it applied", async () => {
    const api = mock();
    const r = await api.agents.swapSubscription("01JB0WRK100000000000000002", "anthropic-max-2");
    expect(r).toEqual({ agent: "01JB0WRK100000000000000002", from: "anthropic-max", to: "anthropic-max-2", applied: "live" });
    expect((await api.agents.get(r.agent)).subscription).toBe("anthropic-max-2");
    expect((await api.subscriptions.get("anthropic-max-2")).agents.map((a) => a.name)).toEqual(["runner-dev"]);
    await expect(api.agents.swapSubscription(r.agent, "anthropic-max-2")).rejects.toMatchObject({ status: 409 });
    await expect(api.agents.swapSubscription("01JB0SYS000000000000000007", "anthropic-max-2")).rejects.toMatchObject({ status: 422 });
    // A file agent swaps inside its pool.
    expect((await api.agents.swapSubscription("01JB0LEAD00000000000000001", "anthropic-max-2")).applied).toBe("live");
    // A stopped agent: the next start uses the new account.
    await api.agents.update("01JB0SYS000000000000000007", { subscription_pool: ["anthropic-max-2"] });
    expect((await api.agents.swapSubscription("01JB0SYS000000000000000007", "anthropic-max-2")).applied).toBe("next_start");
  });

  it("stores a Moonshot key as a pi-only subscription; an Open Platform plan gets the balance probe", async () => {
    const api = mock();
    const s = await api.subscriptions.setApiKey("kimi", { provider: "moonshot", key: "sk-never-shown" });
    expect(s).toMatchObject({
      name: "kimi", provider: "moonshot", plan: "coding", harnesses: ["pi"], env_var: "MOONSHOT_API_KEY", source: "api",
      login_status: "api_key", probe_status: "none", local_estimate_only: true, has_api_key: true, default_models: { pi: "kimi-for-coding" },
    });
    expect(s).not.toHaveProperty("probe");
    expect(JSON.stringify(s)).not.toContain("sk-never-shown");
    const cn = await api.subscriptions.setApiKey("kimi-cn", { provider: "moonshot", key: "k", plan: "api-cn" });
    expect(cn).toMatchObject({ plan: "api-cn", probe: "moonshot-cn-balance", local_estimate_only: false, default_models: { pi: "kimi-k3" } });
    await expect(api.subscriptions.setApiKey("kimi3", { provider: "moonshot", key: "k", plan: "Allegretto" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    // pi only: a definition for another harness, or a key on a subscription of another harness.
    for (const h of ["claude", "codex", "copilot"] as const) {
      await expect(api.subscriptions.create({ name: "kimi4", provider: "moonshot", plan: "api", harnesses: [h] })).rejects.toMatchObject({ status: 422, code: "invalid" });
      await expect(api.subscriptions.create({ name: "kimi4", provider: "moonshot", harnesses: ["pi", h] })).rejects.toMatchObject({ status: 422, code: "invalid" });
    }
    await expect(api.subscriptions.setApiKey("anthropic-max", { provider: "moonshot", key: "k" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    expect((await api.subscriptions.verify("kimi")).subscription.login_status).toBe("api_key");
    await expect(api.subscriptions.logout("kimi")).rejects.toMatchObject({ status: 422, code: "invalid" });
    const models = await api.subscriptions.models("kimi");
    expect(models.source).toBe("fallback");
    expect(models.models[0]?.id).toBe("kimi-for-coding");
  });

  it("stores a Z.ai key as a claude+pi subscription with plan, api_key status, and no probe", async () => {
    const api = mock();
    const s = await api.subscriptions.setApiKey("glm", { provider: "zai", key: "zk-never-shown", plan: "api" });
    expect(s).toMatchObject({
      name: "glm", provider: "zai", plan: "api", harnesses: ["claude", "pi"], env_var: "ZAI_API_KEY", source: "api",
      login_status: "api_key", probe_status: "none", windows: [], local_estimate_only: true, has_api_key: true, held_agents: 0,
    });
    expect(s).not.toHaveProperty("probe");
    expect(JSON.stringify(s)).not.toContain("zk-never-shown");
    expect((await api.subscriptions.setApiKey("glm2", { provider: "zai", key: "k" })).plan).toBe("coding");
    await expect(api.subscriptions.setApiKey("glm3", { provider: "zai", key: "k", plan: "Max 20x" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.setApiKey("or2", { provider: "openrouter", key: "k", plan: "api" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.create({ name: "glm4", provider: "zai", plan: "pro" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    expect((await api.subscriptions.verify("glm")).subscription.login_status).toBe("api_key");
    await expect(api.subscriptions.logout("glm")).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("stores an API key write-only: no answer has the key", async () => {
    const api = mock();
    const secret = "sk-test-0123456789-secret";
    const s = await api.subscriptions.setApiKey("groq-key", { provider: "groq", key: secret });
    expect(s).toMatchObject({ name: "groq-key", source: "api", env_var: "GROQ_API_KEY", has_api_key: true, harnesses: ["pi"], local_estimate_only: true });
    expect(JSON.stringify(s)).not.toContain(secret);
    expect(JSON.stringify(await api.subscriptions.list())).not.toContain(secret);
    expect(JSON.stringify(await api.audit({ action: "subscription.api_key" }))).not.toContain(secret);
    expect((await api.subscriptions.setApiKey("gh-pat", { provider: "github", key: "github_pat_x" })).harnesses).toEqual(["copilot"]);
    await expect(api.subscriptions.setApiKey("compat", { provider: "openai-compatible", key: "k" })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.setApiKey("nope", { provider: "acme", key: "k" })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.setApiKey("groq-key", { provider: "groq", key: " " })).rejects.toMatchObject({ code: "invalid" });
  });

  it("detects logins with no subscription, with a suggested definition", async () => {
    const api = mock();
    // Every login of the mock has a subscription.
    expect(await api.subscriptions.detect()).toEqual([]);
    // pi uses API keys only, so a login is Claude Code, Codex, or Copilot: here the Codex login loses its subscription.
    api.world.subscriptions = api.world.subscriptions.filter((s) => s.name !== "chatgpt-pro");
    const found = await api.subscriptions.detect();
    expect(found.length).toBe(1);
    const d = found[0]!;
    expect(d).toMatchObject({ kind: "codex", harness: "codex", provider: "openai", logged_in: true });
    expect(d).not.toHaveProperty("pi_provider");
    expect(d.suggested).toMatchObject({ provider: "openai", harnesses: ["codex"] });
    await api.subscriptions.create({ ...d.suggested, name: d.suggested.name! });
    expect(await api.subscriptions.detect()).toEqual([]);
  });

  it("emits subscription.offer as {login, suggested}", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    api.world.subscriptions = api.world.subscriptions.filter((s) => s.name !== "chatgpt-pro");
    api.sim.offerLogins();
    const e = frames.find((x) => x.kind === "subscription.offer")!;
    expect(Object.keys(e.data).sort()).toEqual(["login", "suggested"]);
    expect(e.data.login).toMatchObject({ harness: "codex", provider: "openai", logged_in: true });
    st.close();
  });

  it("runs a device-code login session: starting, waiting with url and device_code, a PTY that prints them, cancel", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0 });
    const s = await api.subscriptions.startLogin({ kind: "codex" });
    expect(s).toMatchObject({ kind: "codex", state: "starting", done: false });
    expect(Object.keys(s).sort()).toEqual(["done", "id", "instructions", "kind", "started_at", "state"]);
    await wait();
    const w = await api.subscriptions.loginState(s.id);
    expect(w).toMatchObject({ state: "waiting", url: "https://auth.openai.com/codex/device", done: false });
    expect(w.device_code).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4,5}$/);

    const pty = api.subscriptions.loginPty(s.id);
    const out: string[] = [];
    pty.addEventListener("message", (ev) => out.push(new TextDecoder().decode(ev.data as ArrayBuffer)));
    await wait();
    const screen = out.join("");
    expect(screen).toContain(w.url!);
    expect(screen).toContain(w.device_code!);
    pty.close();

    await api.subscriptions.cancelLogin(s.id);
    const c = await api.subscriptions.loginState(s.id);
    expect(c).toMatchObject({ state: "cancelled", done: true });
    expect(c.finished_at).toBeTruthy();
    await expect(api.subscriptions.cancelLogin(s.id)).rejects.toMatchObject({ status: 409, code: "not_running" });
    await expect(api.subscriptions.loginInput(s.id, "x")).rejects.toMatchObject({ status: 409, code: "not_running" });
    expect(api.world.audit.map((a) => a.action)).toEqual(expect.arrayContaining(["login.start", "login.cancel", "login.end"]));
  });

  it("finishes a paste-code login: verifying, done, the subscription made and probed, the code never stored", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0 });
    const s = await api.subscriptions.startLogin({ kind: "claude", name: "claude-home" });
    await wait();
    expect((await api.subscriptions.loginState(s.id)).url).toMatch(/^https:\/\/claude\.ai\/oauth/);
    await expect(api.subscriptions.loginInput(s.id, "a\nb")).rejects.toMatchObject({ code: "invalid" });
    await api.subscriptions.loginInput(s.id, "secret-code-123");
    expect((await api.subscriptions.loginState(s.id)).state).toBe("verifying");
    await wait();
    const done = await api.subscriptions.loginState(s.id);
    expect(done).toMatchObject({ state: "done", done: true, subscription: "claude-home" });
    const sub = await api.subscriptions.get("claude-home");
    expect(sub).toMatchObject({ login_status: "logged_in", probe_status: "ok", source: "api" });
    expect(JSON.stringify(api.world.audit)).not.toContain("secret-code-123");
    expect(api.world.audit.find((a) => a.action === "login.input")!.detail).toEqual({ length: 15 });
  });

  it("expires a login after its time, refuses a fifth live session, and has no pi login", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0, loginTtlMs: 5 });
    const s = await api.subscriptions.startLogin({ kind: "copilot" });
    await wait(15);
    expect(await api.subscriptions.loginState(s.id)).toMatchObject({ state: "expired", done: true });
    // pi uses API keys only (PLAN §16.3).
    for (const req of [{ kind: "pi" as const }, { kind: "pi" as const, provider: "anthropic" }]) {
      await expect(api.subscriptions.startLogin(req)).rejects.toMatchObject({ status: 422, message: expect.stringContaining("pi uses API keys only") });
    }
    const api2 = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0 });
    for (let i = 0; i < 4; i++) await api2.subscriptions.startLogin({ kind: "codex" });
    await expect(api2.subscriptions.startLogin({ kind: "codex" })).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("refuses a plan login on a pi agent, like the daemon", async () => {
    const api = mock();
    const project = api.world.projects[0]!.name;
    const refused = { status: 422, message: expect.stringContaining("pi uses API keys only") };
    await expect(api.agents.create({ name: "p-plan", project, harness: "pi", subscription: "anthropic-max" })).rejects.toMatchObject(refused);
    const a = await api.agents.create({ name: "p-key", project, harness: "pi", subscription: "openrouter-key" });
    await expect(api.agents.update(a.id, { subscription: "chatgpt-pro" })).rejects.toMatchObject(refused);
    const c = await api.agents.create({ name: "c-plan", project, harness: "claude", subscription: "anthropic-max" });
    await expect(api.agents.update(c.id, { harness: "pi" })).rejects.toMatchObject(refused);
  });

  it("answers GET /api/usage as points oldest first; group keys agents by ULID", async () => {
    const api = mock();
    const flat = await api.usage.series({ bucket: "hour" });
    expect(flat.length).toBeGreaterThan(0);
    expect(Object.keys(flat[0]!).sort()).toEqual(["bucket", "cache_read", "cache_write", "input_tokens", "messages", "output_tokens", "turns"]);
    expect(flat.every((p, i) => i === 0 || flat[i - 1]!.bucket <= p.bucket)).toBe(true);
    expect(flat[0]!.bucket).toMatch(/T\d\d:00:00Z$/);
    const byAgent = await api.usage.series({ project: "terrarium", group: "agent", bucket: "day", since: new Date(Date.now() - 3 * 86_400_000).toISOString() });
    expect(byAgent.every((p) => p.bucket.endsWith("T00:00:00Z"))).toBe(true);
    const lead = await api.agents.get("terrarium/lead");
    expect(byAgent.some((p) => p.key === lead.id)).toBe(true);
    expect(byAgent.every((p) => /^01/.test(p.key!))).toBe(true);
    const one = await api.usage.series({ agent: "terrarium/lead", group: "model" });
    expect(new Set(one.map((p) => p.key))).toEqual(new Set(["claude-opus-5", "claude-haiku-5-5"]));
    await expect(api.usage.series({ since: "yesterday" })).rejects.toMatchObject({ status: 400 });
  });

  it("answers group=card with the title, a card filter, and GET /api/cards/{id}/usage", async () => {
    const api = mock();
    const since = new Date(Date.now() - 7 * 86_400_000).toISOString();
    const byCard = await api.usage.series({ group: "card", bucket: "day", since });
    const withCard = byCard.filter((p) => p.key);
    expect(withCard.length).toBeGreaterThan(0);
    // Samples with no card have no key and no title, like the daemon's null.
    expect(byCard.some((p) => !("key" in p) && !("title" in p))).toBe(true);
    for (const p of withCard) {
      expect(p.key).toMatch(/^[A-Z]+-\d+$/);
      expect(typeof p.title).toBe("string");
    }
    const flat = await api.usage.series({ bucket: "hour" });
    expect(flat.every((p) => !("title" in p))).toBe(true);

    const id = withCard[0]!.key!;
    const filtered = await api.usage.series({ card: id.toLowerCase(), bucket: "day", since });
    const sum = (ps: { input_tokens: number; output_tokens: number }[]) => ps.reduce((a, p) => a + p.input_tokens + p.output_tokens, 0);
    expect(sum(filtered)).toBe(sum(byCard.filter((p) => p.key === id)));

    const u = await api.usage.card(id.toLowerCase());
    expect(Object.keys(u).sort()).toEqual(["agents", "card", "models", "project", "title", "total"]);
    expect(u.card).toBe(id);
    expect(Object.keys(u.total).sort()).toEqual(["cache_read", "cache_write", "input_tokens", "messages", "output_tokens", "turns"]);
    expect(Object.keys(u.models[0]!).sort()).toEqual(["cache_read", "cache_write", "input_tokens", "messages", "model", "output_tokens", "turns"]);
    expect(Object.keys(u.agents[0]!).sort()).toEqual(["cache_read", "cache_write", "id", "input_tokens", "messages", "name", "output_tokens", "turns"]);
    expect(u.agents[0]!.name).toMatch(/^[a-z-]+\/[a-z-]+$/);
    const spend = (t: { input_tokens: number; output_tokens: number }) => t.input_tokens + t.output_tokens;
    expect(u.models.every((m, i) => i === 0 || spend(u.models[i - 1]!) >= spend(m))).toBe(true);
    expect(spend(u.total)).toBe(u.models.reduce((a, m) => a + spend(m), 0));
    expect(spend(u.total)).toBe(u.agents.reduce((a, m) => a + spend(m), 0));
    const later = await api.usage.card(id, { since: "2999-01-01T00:00:00Z" });
    expect(later).toMatchObject({ since: "2999-01-01T00:00:00.000Z", models: [], agents: [], total: { messages: 0 } });
    await expect(api.usage.card("NOPE-1")).rejects.toMatchObject({ status: 404 });
    await expect(api.usage.card(id, { since: "yesterday" })).rejects.toMatchObject({ status: 400 });
    await expect(api.usage.series({ group: "board" as never })).rejects.toMatchObject({ status: 400 });
  });

  it("answers GET /api/usage/me with the subscription, today, and holds", async () => {
    const api = mock();
    const me = await api.usage.me("terrarium/lead");
    expect(me.agent).toMatchObject({ name: "lead", project: "terrarium" });
    expect(me.subscription).toMatchObject({ name: "anthropic-max", probe_status: "ok", local_estimate_only: false });
    expect(Object.keys(me.today.mine).sort()).toEqual(["cache_read", "cache_write", "input_tokens", "messages", "output_tokens", "turns"]);
    expect(me.holds).toEqual([]);
  });

  it("runs the quota policy: quota.alert mails at each line, and a probe window holds no turn", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    const quotaMail = () => api.world.mail.filter((m) => m.to === "operator" && m.from === "system" && m.subject.startsWith("Quota: anthropic-max 5h window at"));
    api.sim.setWindow("anthropic-max", "5h", 79);
    expect(frames.filter((x) => x.kind === "quota.alert")).toHaveLength(0);
    api.sim.setWindow("anthropic-max", "5h", 85);
    const alert = frames.find((x) => x.kind === "quota.alert")!;
    expect(Object.keys(alert.data).sort()).toEqual(["alert_at", "level", "message", "resets_at", "subscription", "threshold", "used_pct", "window"]);
    expect(alert.data).toMatchObject({ subscription: "anthropic-max", window: "5h", used_pct: 85, level: "alert", alert_at: 80, threshold: 80 });
    expect(quotaMail()).toHaveLength(1);
    expect(quotaMail()[0]!.body).toContain("Open: /subscriptions");
    api.sim.setWindow("anthropic-max", "5h", 85);
    api.sim.setWindow("anthropic-max", "5h", 88);
    expect(frames.filter((x) => x.kind === "quota.alert")).toHaveLength(1); // once per line and window period
    // The second line: one more mail at 90, then nothing.
    api.sim.setWindow("anthropic-max", "5h", 91);
    expect(frames.filter((x) => x.kind === "quota.alert").at(-1)!.data).toMatchObject({ level: "alert", threshold: 90 });
    api.sim.setWindow("anthropic-max", "5h", 95);
    expect(quotaMail()).toHaveLength(2);
    expect(frames.filter((x) => x.kind === "quota.alert")).toHaveLength(2);

    api.sim.setWindow("anthropic-max", "5h", 100);
    const exhausted = frames.filter((x) => x.kind === "quota.alert").at(-1)!;
    expect(exhausted.data.level).toBe("exhausted");
    expect(Object.keys(exhausted.data).sort()).toEqual(["alert_at", "level", "message", "resets_at", "subscription", "threshold", "used_pct", "window"]);
    expect(exhausted.data).toMatchObject({ threshold: 100 });
    // One used-up mail; a repeated sample sends none. A probe window holds no turn.
    const usedUp = () => api.world.mail.filter((m) => m.to === "operator" && m.subject === "Quota: anthropic-max 5h window is used up");
    expect(usedUp()).toHaveLength(1);
    expect(usedUp()[0]!.body).toContain("The agents keep their turns.");
    expect(usedUp()[0]!.body).toContain("terrarium/lead");
    api.sim.setWindow("anthropic-max", "5h", 100);
    expect(usedUp()).toHaveLength(1);
    expect(frames.filter((x) => x.kind === "quota.alert")).toHaveLength(3);
    expect(frames.filter((x) => x.kind === "quota.blocked")).toHaveLength(0);
    expect((await api.subscriptions.get("anthropic-max")).held_agents).toBe(0);
    expect((await api.usage.me("terrarium/lead")).holds).toEqual([]);

    api.sim.setWindow("anthropic-max", "5h", 20);
    expect(frames.filter((x) => x.kind === "quota.resumed")).toHaveLength(0);
    // The window dropped: the next crossing notifies again.
    api.sim.setWindow("anthropic-max", "5h", 82);
    expect(quotaMail()).toHaveLength(3);
    st.close();
  });

  it("stores self_report events as {status, note}, apart from the runner status", async () => {
    const api = mock();
    const page = await api.events.history({ kind: "self_report" });
    const e = page.items[0]!;
    expect(e).toMatchObject({ kind: "self_report", agent: "adapter-codex", project: "terrarium" });
    expect(e.data).toMatchObject({ status: "blocked", note: expect.any(String) });
    expect(e.actor).toMatch(/^agent:/);
  });

  it("answers GET /api/events/latest with the newest seq", async () => {
    const api = mock();
    expect(await api.events.latest()).toBe(api.world.seq);
  });

  it("gives host metrics as the daemon does (docs/api.md §17.1)", async () => {
    const api = mock();
    const hosts = await api.hosts.list();
    const lab1 = hosts.find((h) => h.name === "lab1")!;
    expect(lab1.capabilities?.metrics).toBe(true);
    expect(lab1.metrics).toMatchObject({ stale: false, interval_secs: 10 });
    expect(lab1.metrics!.cpu!.busy_pct).toBeGreaterThanOrEqual(0);
    // An older host agent has no samples.
    expect(hosts.find((h) => h.name === "nas")!.metrics).toBeNull();
    const hist = await api.hosts.metrics("lab1", { minutes: 15 });
    expect(hist).toMatchObject({ host: "lab1", interval_secs: 10, minutes: 15, stale: false });
    expect(hist.samples).toHaveLength(90);
    expect(Date.parse(hist.samples[0]!.at)).toBeLessThan(Date.parse(hist.samples.at(-1)!.at));
    expect((await api.hosts.metrics("nas")).samples).toEqual([]);
    await expect(api.hosts.metrics("lab1", { minutes: 61 })).rejects.toMatchObject({ status: 400, code: "bad_request" });
    await expect(api.hosts.metrics("nope")).rejects.toMatchObject({ status: 404 });
  });

  // ----- PLAN §19 hosts and targets: mock-only until the daemon has them -----

  it("shows a host token once and revokes it", async () => {
    const api = mock();
    const made = await api.hosts.create("lab2");
    expect(made.token).toMatch(/^thk_/);
    expect(made).toMatchObject({ name: "lab2", status: "offline", online: false, sessions: 0, capabilities: {}, last_seen: null });
    // As the daemon's `hosts::offline_reason`.
    expect(made.offline_reason).toMatch(/^never connected/);
    expect((await api.hosts.create("lab3", { enrol: true })).offline_reason).toMatch(/^waiting for the host agent/);
    expect(JSON.stringify(await api.hosts.list())).not.toContain(made.token);
    await expect(api.hosts.create("lab2")).rejects.toMatchObject({ code: "conflict" });
    await api.hosts.revoke("lab2");
    expect((await api.hosts.list()).map((h) => h.name)).not.toContain("lab2");
    const lab1 = (await api.hosts.list()).find((h) => h.name === "lab1")!;
    expect(lab1.capabilities).toMatchObject({ docker: { present: true, version: "29.8.0" }, vmlab: { present: true } });
  });

  it("answers GET /api/harnesses?latest=true and the harness update in the daemon's shapes", async () => {
    const api = mock();
    // The fields of `harness_versions::HarnessVersion` (docs/api.md GET /api/harnesses).
    const ROW = ["harness", "package", "pinned", "installed", "error", "checked_at", "latest", "update", "latest_error", "active", "versions", "agents", "auto_update"];
    const r = await api.harnesses(true, true);
    for (const h of r.harnesses) {
      for (const k of Object.keys(h)) expect(ROW).toContain(k);
      expect(h).toHaveProperty("latest");
      expect(h).toHaveProperty("update");
      expect(["none", "minor", "major", null]).toContain(h.update);
      if (h.latest === null) expect(typeof h.latest_error).toBe("string");
      else expect(h.latest!.published_at === null || !Number.isNaN(Date.parse(h.latest!.published_at))).toBe(true);
    }
    // Without `latest`, no npm fields.
    for (const h of (await api.harnesses()).harnesses) for (const k of ["latest", "update", "latest_error"]) expect(h).not.toHaveProperty(k);
    // `harness_install::UpdateResult` (docs/api.md POST /api/harnesses/{name}/update).
    // `note` and `removed` are absent when empty.
    const RESULT = ["harness", "package", "from", "to", "output", "reused", "agents_on_old_version"].sort();
    const one = await api.harnessUpdate("copilot-prompt", { force: true });
    expect(Object.keys(one).sort()).toEqual(RESULT);
    expect(one).toMatchObject({ harness: "copilot-prompt", package: "copilot-custom-system-prompt", reused: false, agents_on_old_version: [] });
    // A live agent on the harness does not refuse the update.
    const claude = await api.harnessUpdate("claude", { version: "2.1.320" });
    expect(claude.agents_on_old_version).toEqual(expect.arrayContaining([expect.objectContaining({ agent: "terrarium/lead", version: "2.1.310" })]));
    for (const a of claude.agents_on_old_version) expect(Object.keys(a).sort()).toEqual(["agent", "id", "version"]);
    const npm = await api.harnessUpdate("claude", { version: "99.1.0" }).catch((e: unknown) => e);
    expect(npm).toMatchObject({ status: 422, code: "invalid", details: { package: "@anthropic-ai/claude-code", version: "99.1.0", from: "2.1.320" } });
    expect(Object.keys((npm as { details: object }).details).sort()).toEqual(["from", "output", "package", "version"]);
    // `harness_auto_update::HarnessAutoUpdate` and `ScheduleInfo` (docs/api.md "Auto-update and pins").
    const AUTO = ["policy", "pinned_version", "previous", "latest", "checked_at", "result", "detail", "changelog"].sort();
    for (const h of r.harnesses) expect(Object.keys(h.auto_update!).sort()).toEqual(AUTO);
    expect(Object.keys(r.auto_update!).sort()).toEqual(["next_check_at", "schedule"]);
    // `HarnessPinResult`: `output`, `note`, and `removed` are absent when empty.
    const un = await api.harnessUnpin("claude");
    expect(Object.keys(un).sort()).toEqual(["agents_on_old_version", "from", "harness", "pinned_version", "policy", "to"]);
    // `CheckItem`: `from` and `to` only on an install.
    const check = await api.harnessCheck();
    expect(Object.keys(check).sort()).toEqual(["checked_at", "results"]);
    for (const i of check.results) {
      const keys = ["harness", "result", "detail", "latest", "mailed", ...(i.result === "updated" ? ["from", "to"] : [])].sort();
      expect(Object.keys(i).sort()).toEqual(keys);
    }
  });

  it("runs the target lifecycle with the daemon's states, and answers each verb with the executor report", async () => {
    const api = mock();
    const state = async (name: string) => (await api.targets.list()).find((x) => x.name === name)!;
    const STATES = ["running", "stopped", "absent", "unknown"];
    for (const x of await api.targets.list()) expect(STATES).toContain(x.state);
    // lab-win11: a kept ephemeral vmlab target, stopped.
    expect(await api.targets.action("lab-win11", "up")).toEqual({ target: "lab-win11", verb: "up", state: "running" });
    expect((await state("lab-win11")).state).toBe("running");
    expect(await api.targets.action("lab-win11", "snapshot")).toEqual({ target: "lab-win11", verb: "snapshot", state: "running", snapshot: "terrarium", scope: "machine" });
    expect((await state("lab-win11")).snapshots).toEqual(["terrarium-clean", "terrarium"]);
    expect(await api.targets.action("lab-win11", "restore")).toMatchObject({ verb: "restore", state: "running", snapshot: "terrarium" });
    expect(await api.targets.action("lab-win11", "down")).toMatchObject({ state: "stopped" });
    // pg-test: ephemeral docker without keep; down destroys it.
    expect(await api.targets.action("pg-test", "down")).toEqual({ target: "pg-test", verb: "down", state: "absent" });
    expect(await api.targets.action("pg-test", "status")).toEqual({ target: "pg-test", verb: "status", state: "absent" });
    expect((await api.audit({ action: "target.down" })).items.at(-1)).toMatchObject({ target: "pg-test", detail: { verb: "down", state: "absent" } });
    // host and ssh: up and down are no-ops; status follows the host agent link.
    expect(await api.targets.action("build-box", "up")).toMatchObject({ state: "unknown" });
    expect(await api.targets.action("pi-runner", "status")).toMatchObject({ state: "unknown" });
    await expect(api.targets.action("build-box", "snapshot")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.action("pg-test", "snapshot")).rejects.toMatchObject({ status: 422, code: "invalid" });
    // A permanent vmlab target stops; it is not destroyed. A restore needs the snapshot.
    await expect(api.targets.action("dev-vm", "down")).resolves.toMatchObject({ state: "stopped" });
    await expect(api.targets.action("ci-box", "up")).resolves.toMatchObject({ state: "running" });
    api.world.hostSnapshots["lab-win11"] = [];
    await expect(api.targets.action("lab-win11", "restore")).rejects.toMatchObject({ status: 422, code: "invalid", details: { verb: "vmlab.snapshot_restore" } });
    const opened = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    expect(opened).toMatchObject({ target: "pg-test", kind: "shell" });
    await api.targets.closeSession("pg-test", opened.session_id);
    await expect(api.targets.closeSession("pg-test", opened.session_id)).rejects.toMatchObject({ status: 404 });
  });

  it("lists and deletes snapshots in the daemon's shapes (docs/api.md §17.3)", async () => {
    const api = mock();
    const LIST = ["target", "scope", "machines", "snapshots", "dropped"];
    const ROW = ["name", "kind", "taken", "machines", "managed"].sort();
    // lab-win11: terrarium-clean is Terrarium's, pre-upgrade is not.
    const l = await api.targets.snapshots("lab-win11");
    expect(Object.keys(l).sort()).toEqual([...LIST].sort());
    expect(l).toMatchObject({ target: "lab-win11", scope: "machine", machines: ["win11"], dropped: [] });
    for (const r of l.snapshots) expect(Object.keys(r).sort()).toEqual(ROW);
    expect(l.snapshots.map((s) => [s.name, s.managed])).toEqual([["terrarium-clean", true], ["pre-upgrade", false]]);
    // The lab scope says whether the lab runs.
    expect(await api.targets.snapshots("lab-win11", { scope: "lab" })).toMatchObject({ scope: "lab", lab_running: true });
    // A take with a name and the lab scope answers the scope; the list shows it as managed.
    await api.targets.action("dev-vm", "snapshot", { name: "pre", scope: "lab" });
    expect((await api.targets.snapshots("dev-vm")).snapshots.find((s) => s.name === "pre")).toMatchObject({ managed: true, machines: ["vm1"] });
    await expect(api.targets.action("dev-vm", "snapshot", { name: "x", scope: "machine" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.action("dev-vm", "snapshot", { name: "-x" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    // A stored name that the host lost goes, with target.reconcile.
    api.world.hostSnapshots["dev-vm"] = api.world.hostSnapshots["dev-vm"]!.filter((r) => r.name !== "terrarium");
    expect((await api.targets.snapshots("dev-vm")).dropped).toEqual(["terrarium"]);
    expect((await api.targets.list()).find((t) => t.name === "dev-vm")!.snapshots).toEqual(["pre"]);
    expect((await api.audit({ action: "target.reconcile" })).items.at(-1)).toMatchObject({ target: "dev-vm", detail: { snapshots_dropped: ["terrarium"] } });
    // Delete: terrarium-clean with snapshot_before_task needs force.
    await expect(api.targets.deleteSnapshot("lab-win11", "terrarium-clean")).rejects.toMatchObject({ status: 409, code: "conflict" });
    const d = await api.targets.deleteSnapshot("lab-win11", "terrarium-clean", { force: true });
    expect(d).toEqual({ target: "lab-win11", verb: "snapshot_delete", snapshot: "terrarium-clean", scope: "machine", machines: ["win11"] });
    expect((await api.targets.list()).find((t) => t.name === "lab-win11")!.snapshots).toEqual([]);
    expect((await api.audit({ action: "target.snapshot_delete" })).items.at(-1)).toMatchObject({ target: "lab-win11", detail: { snapshot: "terrarium-clean" } });
    await api.targets.deleteSnapshot("lab-win11", "pre-upgrade");
    await expect(api.targets.deleteSnapshot("lab-win11", "nosuch")).rejects.toMatchObject({ status: 422, details: { verb: "vmlab.snapshot_delete" } });
    await expect(api.targets.deleteSnapshot("lab-win11", "nosuch", { scope: "lab" })).rejects.toMatchObject({ status: 404, code: "not_found" });
    // A docker target has none; a lab_from_workspace target needs the holder.
    await expect(api.targets.snapshots("pg-test")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.snapshots("terrarium-ws-lab")).rejects.toMatchObject({ status: 422, code: "invalid" });
    const ws = await api.targets.snapshots("terrarium-ws-lab", { agent: "terrarium/runner-dev" });
    expect(ws).toMatchObject({ agent: "terrarium/runner-dev", snapshots: [] });
    await api.targets.action("terrarium-ws-lab", "snapshot", { agent: "terrarium/runner-dev", name: "s1" });
    expect((await api.targets.snapshots("terrarium-ws-lab", { agent: "terrarium/runner-dev" })).snapshots.map((s) => s.name)).toEqual(["s1"]);
  });

  it("refuses a verb on a target whose host agent is offline with 409 conflict", async () => {
    const api = mock();
    api.world.targets.push({ id: "pi-box", name: "pi-box", kind: "docker", host: "pi-cluster", image: "alpine:3", scope: "global", state: "stopped", ephemeral: false, snapshots: [] });
    await expect(api.targets.action("pi-box", "up")).rejects.toMatchObject({ status: 409, code: "conflict" });
  });
});

describe("target displays (docs/api.md §17.10)", () => {
  it("marks the targets with a screen as has_display", async () => {
    const api = mock();
    const by = new Map((await api.targets.list()).map((t) => [t.name, t]));
    expect(by.get("desk-x11")?.has_display).toBe(true);
    expect(by.get("lab-win11")?.has_display).toBe(true);
    expect(by.get("win-vnc")?.has_display).toBe(true);
    expect(by.get("build-box")?.has_display).toBe(false);
    expect(by.get("pg-test")?.has_display).toBe(false);
  });

  it("answers a screenshot in the JSON shape with a PNG in png_base64", async () => {
    const api = mock();
    const s = await api.targets.screenshot("desk-x11", { scale: 0.5 });
    expect(s).toMatchObject({ target: "desk-x11", op: "screenshot", format: "png", width: 640, height: 400, screen: { width: 1280, height: 800 }, scale: 0.5 });
    expect(typeof s.bytes).toBe("number");
    expect(s.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(atob(s.png_base64).slice(1, 4)).toBe("PNG");
    await expect(api.targets.screenshot("desk-x11", { scale: 2 })).rejects.toMatchObject({ status: 422 });
  });

  it("answers info and input, a vnc screen, and 422 for a target with no display", async () => {
    const api = mock();
    const info = await api.targets.displayInfo("lab-win11");
    expect(info).toMatchObject({ target: "lab-win11", backend: "vmlab", width: 1280, height: 800, ocr: "vmlab", input: { allowed: true } });
    const r = await api.targets.displayOp("desk-x11", "click", { x: 10, y: 20 });
    expect(r).toMatchObject({ target: "desk-x11", op: "click", ok: true });
    const typed = await api.targets.displayOp("desk-x11", "type", { text: "secret" });
    expect(typed).toMatchObject({ chars: 6 });
    const vnc = await api.targets.displayInfo("win-vnc");
    expect(vnc).toMatchObject({ target: "win-vnc", backend: "vnc", ops: expect.arrayContaining(["scroll"]) });
    expect(await api.targets.screenshot("win-vnc")).toMatchObject({ target: "win-vnc", format: "png" });
    await expect(api.targets.screenshot("build-box")).rejects.toMatchObject({ status: 422 });
  });
});

describe("target scope (docs/api.md §17.2)", () => {
  it("puts scope on every target: global without a project, project with one", async () => {
    const api = mock();
    for (const t of await api.targets.list()) expect(t.scope).toBe(t.project ? "project" : "global");
  });

  it("lists the project's targets and the global ones on GET /api/projects/{name}/targets, in the target shape", async () => {
    const api = mock();
    const list = await api.projects.targets("terrarium");
    expect(list.map((t) => `${t.name}:${t.scope}`).sort()).toEqual(["build-box:global", "ci-box:global", "desk-x11:global", "dev-vm:global", "lab-win11:project", "pi-runner:global", "terrarium-net-lab:project", "terrarium-ws-lab:project", "win-vnc:global"]);
    expect(list.every((t) => !t.project || t.project === "terrarium")).toBe(true);
    expect(list[0]).toHaveProperty("access", "operator");
    expect(list[0]).toHaveProperty("host_online");
    await expect(api.projects.targets("nope")).rejects.toMatchObject({ status: 404 });
  });

  it("creates a project target and a global target with 201 shapes, source api", async () => {
    const api = mock();
    const p = await api.targets.create({ name: "pg2", kind: "docker", project: "webshop", host: "lab1", image: "postgres:17", ephemeral: true });
    expect(p).toMatchObject({ id: "webshop-pg2", name: "webshop-pg2", scope: "project", project: "webshop", source: "api", ephemeral: true, keep: false, snapshots: [], access: "operator", runtime: {} });
    const g = await api.targets.create({ name: "vm2", kind: "vmlab", host: "lab1", lab: "/labs/x/vmlab.wcl", snapshot_before_task: true });
    expect(g).toMatchObject({ scope: "global", snapshot_before_task: true });
    expect(g).not.toHaveProperty("project");
    expect(g.name).toBe("vm2");
    expect((await api.projects.targets("webshop")).map((t) => t.name)).toEqual(expect.arrayContaining(["webshop-pg2", "vm2"]));
    expect((await api.projects.targets("terrarium")).map((t) => t.name)).not.toContain("webshop-pg2");
    expect((await api.audit({ action: "target.create" })).items.at(-1)).toMatchObject({ target: "vm2" });
  });

  it("refuses a bad definition, a taken name, and an unknown project", async () => {
    const api = mock();
    await expect(api.targets.create({ name: "x", kind: "docker", host: "lab1" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.create({ name: "x", kind: "ssh", address: "-oProxy" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.create({ name: "x", kind: "docker", host: "lab1", container: "c", ephemeral: true })).rejects.toMatchObject({ status: 422 });
    await expect(api.targets.create({ name: "dev-vm", kind: "host", host: "lab1" })).rejects.toMatchObject({ status: 409, code: "conflict" });
    await expect(api.targets.create({ name: "x", kind: "host", host: "lab1", project: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("answers 403 forbidden for a global target when the caller is not a sysadmin, and still takes a project target", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, denyGlobalTargets: true });
    await expect(api.targets.create({ name: "g", kind: "host", host: "lab1" })).rejects.toMatchObject({ status: 403, code: "forbidden" });
    await expect(api.targets.create({ name: "p", kind: "host", host: "lab1", project: "terrarium" })).resolves.toMatchObject({ scope: "project" });
  });
});

describe("mode 1 targets (docs/api.md §17.2, §17.3, §17.7, §17.8)", () => {
  const runner = (api: ReturnType<typeof mock>) => api.world.agents.find((a) => a.name === "runner-dev")!;
  const lead = (api: ReturnType<typeof mock>) => api.world.agents.find((a) => a.name === "lead")!;

  it("shows runtime on every target, and the lab fields of each source", async () => {
    const api = mock();
    const list = await api.targets.list();
    for (const t of list) expect(t).toHaveProperty("runtime");
    const by = (n: string) => list.find((t) => t.name === n)!;
    expect(by("pg-test").runtime).toEqual({ attached: ["webshop/shop-lead"] });
    expect(by("terrarium-net-lab")).toMatchObject({ kind: "vmlab", lab_repo: "terrarium", lab_ref: "main", lab_path: "labs/net", machine: "router", runtime: {} });
    expect(by("terrarium-ws-lab")).toMatchObject({ lab_from_workspace: true, lab_path: "lab" });
    expect(by("terrarium-ws-lab").runtime!.instances![runner(api).id]).toMatchObject({ agent_name: "terrarium/runner-dev", state: "running", attached: ["terrarium/runner-dev"] });
    expect(by("terrarium-ws-lab").runtime!.instances![runner(api).id]!.lab).toMatch(/\/terrarium-ws-lab\.[^/]+\/lab$/);
  });

  it("validates the vmlab sources as the daemon does, with 422 invalid", async () => {
    const api = mock();
    const bad = [
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab: "/l", lab_repo: "terrarium", machine: "m" },
      { name: "x", kind: "vmlab" as const, host: "lab1", lab_repo: "terrarium", machine: "m" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_from_workspace: true },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_from_workspace: true, machine: "m", snapshot_before_task: true },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab: "/l", lab_path: "a" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_repo: "terrarium", machine: "m", lab_path: "../up" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_repo: "terrarium", machine: "m", lab_ref: "-x" },
      { name: "x", kind: "docker" as const, host: "lab1", image: "a", project: "terrarium", lab_repo: "terrarium" },
    ];
    for (const b of bad) await expect(api.targets.create(b)).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("prefixes a project target made through the API, keeps a prefixed name, and keeps a global name", async () => {
    const api = mock();
    expect((await api.targets.create({ name: "box", kind: "host", host: "lab1", project: "terrarium" })).name).toBe("terrarium-box");
    expect((await api.targets.create({ name: "terrarium-box2", kind: "host", host: "lab1", project: "terrarium" })).name).toBe("terrarium-box2");
    expect((await api.targets.create({ name: "box", kind: "host", host: "lab1" })).name).toBe("box");
    await expect(api.targets.create({ name: "box", kind: "host", host: "lab1", project: "terrarium" })).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("refuses a target past TERRARIUM_MAX_TARGETS_PER_PROJECT with 409 conflict and {project, targets, max}", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, maxTargetsPerProject: 4 });
    // terrarium has lab-win11, terrarium-net-lab, terrarium-ws-lab.
    await api.targets.create({ name: "a", kind: "host", host: "lab1", project: "terrarium" });
    const e = await api.targets.create({ name: "b", kind: "host", host: "lab1", project: "terrarium" }).catch((x: unknown) => x as ApiError);
    expect(e).toMatchObject({ status: 409, code: "conflict", details: { project: "terrarium", targets: 4, max: 4 } });
    await expect(api.targets.create({ name: "g", kind: "host", host: "lab1" })).resolves.toMatchObject({ scope: "global" });
  });

  it("shows max_running_targets on a host, and refuses an up past it with 429 rate_limited", async () => {
    const api = mock();
    expect((await api.hosts.list()).find((h) => h.name === "lab1")).toHaveProperty("max_running_targets", 4);
    // lab1 runs pg-test, dev-vm, and one instance of terrarium-ws-lab: 3 of 4.
    await expect(api.targets.action("ci-box", "up")).resolves.toMatchObject({ state: "running" });
    const e = (await api.targets.action("lab-win11", "up").catch((x: unknown) => x)) as ApiError;
    expect(e).toMatchObject({ status: 429, code: "rate_limited" });
    expect(e.message).toBe("host `lab1` runs 4 targets, and its max_running_targets is 4; bring one down first");
    // An up of a target that runs already does not count against the cap.
    await expect(api.targets.action("ci-box", "up")).resolves.toMatchObject({ state: "running" });
    await api.targets.action("pg-test", "down");
    await expect(api.targets.action("lab-win11", "up")).resolves.toMatchObject({ state: "running" });
  });

  it("brings a lab_repo target up in a worktree: runtime.lab and runtime.head", async () => {
    const api = mock();
    await api.targets.action("terrarium-net-lab", "up");
    const t = (await api.targets.list()).find((x) => x.name === "terrarium-net-lab")!;
    expect(t.state).toBe("running");
    expect(t.runtime).toMatchObject({ lab: expect.stringMatching(/terrarium-net-lab\/lab$/), head: expect.stringMatching(/^[0-9a-f]{40}$/) });
    await api.targets.action("terrarium-net-lab", "down");
    expect((await api.targets.list()).find((x) => x.name === "terrarium-net-lab")!.runtime).toEqual({});
  });

  it("runs a verb on a lab_from_workspace target on one holder's instance, and needs agent", async () => {
    const api = mock();
    await expect(api.targets.action("terrarium-ws-lab", "up")).rejects.toMatchObject({ status: 422, code: "invalid" });
    const r = await api.targets.action("terrarium-ws-lab", "up", { agent: "terrarium/lead" });
    expect(r).toEqual({ target: "terrarium-ws-lab", verb: "up", state: "running" });
    const inst = (await api.targets.list()).find((x) => x.name === "terrarium-ws-lab")!.runtime!.instances!;
    expect(Object.keys(inst).sort()).toEqual([lead(api).id, runner(api).id].sort());
    expect(inst[lead(api).id]).toMatchObject({ agent_name: "terrarium/lead", state: "running", attached: ["terrarium/lead"] });
    // An ephemeral target without keep: down removes the holder's instance.
    expect(await api.targets.action("terrarium-ws-lab", "down", { agent: lead(api).id })).toMatchObject({ state: "absent" });
    expect((await api.audit({ action: "target.down" })).items.at(-1)).toMatchObject({ target: "terrarium-ws-lab", detail: { agent: lead(api).id } });
  });

  it("refuses an exclusive lease on a lab_from_workspace target with 422 invalid", async () => {
    const api = mock();
    await expect(api.targets.grant("terrarium-ws-lab", { agent: "terrarium/lead", exclusive: true })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.grant("terrarium-ws-lab", { agent: "terrarium/lead" })).resolves.toMatchObject({ exclusive: false, state: "active" });
  });
});

/** A "daemon" that answers upgrades with the rules of ./ws, for the real transport. */
const daemon = (rules: HandshakeRules): MakeRawSocket => (url, protocols = []) => new FakePty({ protocol: acceptHandshake({ url, protocols }, rules) });
/** A daemon from before F4: it reads `?token=` only and sends no subprotocol back, so a browser fails the handshake. */
const oldDaemon: MakeRawSocket = (url, protocols = []) =>
  new FakePty({ protocol: protocols.length ? null : acceptHandshake({ url, protocols }, { token: "T", queryToken: true }) });

const outcome = (s: AuthSocket | PtySocket) =>
  new Promise<string>((resolve) => {
    s.addEventListener("open", () => resolve(`open:${(s as AuthSocket).protocol}`));
    s.addEventListener("close", () => resolve("closed"));
  });

describe("target leases (docs/api.md §17.7)", () => {
  const LEASE_KEYS = ["agent", "agent_name", "exclusive", "expires_at", "granted_at", "granted_by", "id", "project", "reason", "revoked_at", "revoked_by", "state", "target"];
  const byName = (api: ReturnType<typeof mock>, name: string) => api.world.agents.find((a) => a.name === name)!;
  const frames = async (api: ReturnType<typeof mock>) => {
    const out: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => out.push(e));
    await wait();
    return { out, close: () => s.close() };
  };

  it("lists the active leases newest first in the documented shape; ?all=true adds the ended ones", async () => {
    const api = mock();
    const active = await api.targets.leases("lab-win11");
    expect(active).toHaveLength(1);
    expect(Object.keys(active[0]!).sort()).toEqual(LEASE_KEYS);
    expect(active[0]).toMatchObject({ target: "lab-win11", agent_name: "terrarium/runner-dev", exclusive: true, state: "active", reason: "TER-12" });
    expect(active[0]!.granted_by).toMatch(/^agent:/);
    const all = await api.targets.leases("lab-win11", { all: true });
    expect(all.map((l) => l.state)).toEqual(["active", "expired"]);
    expect(all[1]).toMatchObject({ revoked_by: "system" });
    const mine = await api.agents.leases("terrarium/adapter-codex", { all: true });
    expect(mine.map((l) => `${l.target}:${l.state}`)).toEqual(["build-box:active", "lab-win11:expired"]);
    expect((await api.agents.leases("terrarium/adapter-codex")).map((l) => l.target)).toEqual(["build-box"]);
  });

  it("marks each target with access and lease for the caller", async () => {
    const t = (await mock().targets.list())[0]!;
    expect(t).toHaveProperty("access", "operator");
    expect(t).toHaveProperty("lease", null);
  });

  it("grants with 201 the lease, and writes a lease event {change, target, lease_id, expires_at, exclusive, state} to the holder", async () => {
    const api = mock();
    const lead = byName(api, "lead");
    const f = await frames(api);
    const l = await api.targets.grant("build-box", { agent: "terrarium/lead", until: "30m", reason: "TER-3" });
    expect(l).toMatchObject({ target: "build-box", agent: lead.id, agent_name: "terrarium/lead", project: "terrarium", granted_by: "operator", exclusive: false, state: "active", reason: "TER-3", revoked_at: null });
    expect(Date.parse(l.expires_at!) - Date.now()).toBeGreaterThan(29 * 60_000);
    const e = f.out.find((x) => x.kind === "lease")!;
    expect(e).toMatchObject({ kind: "lease", agent: "lead", agent_id: lead.id, project: "terrarium", actor: "operator" });
    expect(e.data).toEqual({ agent_id: lead.id, change: "granted", target: "build-box", lease_id: l.id, expires_at: l.expires_at, exclusive: false, state: "active" });
    expect((await api.audit({ action: "target.grant" })).items.at(-1)).toMatchObject({ target: "build-box", detail: { lease: l.id, agent: lead.id } });
    f.close();
  });

  it("refuses a clash with 409 lease_held and the holder in details; a second lease of one agent is 409 conflict", async () => {
    const api = mock();
    const runner = byName(api, "runner-dev");
    const held = api.world.leases.find((x) => x.target === "lab-win11" && x.state === "active")!;
    const e = await api.targets.grant("lab-win11", { agent: "terrarium/lead" }).catch((x: unknown) => x as ApiError);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 409, code: "lease_held" });
    expect((e as ApiError).details).toEqual({ target: "lab-win11", holder: "terrarium/runner-dev", holder_id: runner.id, lease_id: held.id, exclusive: true, expires_at: held.expires_at });
    // A shared lease on build-box: an exclusive grant next to it is lease_held too.
    await expect(api.targets.grant("build-box", { agent: "terrarium/lead", exclusive: true })).rejects.toMatchObject({ status: 409, code: "lease_held", details: { holder: "terrarium/adapter-codex", exclusive: false, expires_at: null } });
    await expect(api.targets.grant("build-box", { agent: "terrarium/adapter-codex" })).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("refuses a project target for an agent of another project, and a bad or past until, with 422 invalid", async () => {
    const api = mock();
    await expect(api.targets.grant("pg-test", { agent: "terrarium/lead" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.grant("build-box", { agent: "terrarium/lead", until: "soon" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.grant("build-box", { agent: "terrarium/lead", until: "2020-01-01T00:00:00Z" })).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("extends with PATCH {until} and revokes with DELETE, each with a lease event; then the lease is gone from the active list", async () => {
    const api = mock();
    const runner = byName(api, "runner-dev");
    const f = await frames(api);
    const ext = await api.targets.extendLease("lab-win11", runner.id, null);
    expect(ext).toMatchObject({ expires_at: null, state: "active" });
    const end = await api.targets.revokeLease("lab-win11", "terrarium/runner-dev");
    expect(end).toMatchObject({ id: ext.id, state: "revoked", revoked_by: "operator" });
    expect(end.revoked_at).toEqual(expect.any(String));
    expect(f.out.filter((x) => x.kind === "lease").map((x) => [x.data.change, x.data.state])).toEqual([
      ["extended", "active"],
      ["ended", "revoked"],
    ]);
    expect(await api.targets.leases("lab-win11")).toEqual([]);
    await expect(api.targets.revokeLease("lab-win11", runner.id)).rejects.toMatchObject({ status: 404 });
    // The exclusive holder is gone: a new grant works.
    expect((await api.targets.grant("lab-win11", { agent: "terrarium/lead", exclusive: true })).exclusive).toBe(true);
    f.close();
  });

  it("ends a lease at its time as system, with an ended event and state expired", async () => {
    let clock = Date.now();
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, now: () => clock });
    const l = await api.targets.grant("build-box", { agent: "terrarium/lead", until: "90s" });
    const f = await frames(api);
    clock += 91_000;
    const all = await api.targets.leases("build-box", { all: true });
    expect(all.find((x) => x.id === l.id)).toMatchObject({ state: "expired", revoked_by: "system" });
    expect(f.out.find((x) => x.kind === "lease")?.data).toMatchObject({ change: "ended", lease_id: l.id, state: "expired" });
    expect(f.out.find((x) => x.kind === "lease")?.actor).toBe("system");
    f.close();
  });
});

describe("WebSocket auth contract (docs/security-review.md F4)", () => {
  const transport = (make: MakeRawSocket, token: string | null, allowQueryToken = false) =>
    createHttpTransport({ getToken: () => token, location: { protocol: "http:", host: "d" }, makeSocket: make, allowQueryToken });

  it("authenticates with bearer.<token>, and the daemon echoes it", async () => {
    const t = transport(daemon({ token: "T", queryToken: false }), "T");
    const s = t.openSocket(t.wsUrl("/api/events"));
    expect(await outcome(s)).toBe(`open:${bearerProtocol("T")}`);
    expect(s.url).not.toContain("token");
    expect(s.mode).toBe("subprotocol");
  });

  it("is refused with a wrong token, and does not try ?token= by default", async () => {
    const t = transport(daemon({ token: "T", queryToken: true }), "wrong");
    const s = t.openSocket(t.wsUrl("/api/events"));
    expect(await outcome(s)).toBe("closed");
    expect(s.failure).toBe("refused");
  });

  it("falls back to ?token= against an old daemon only with VITE_ALLOW_QUERY_TOKEN", async () => {
    const strict = transport(oldDaemon, "T");
    expect(await outcome(strict.openSocket(strict.wsUrl("/api/events")))).toBe("closed");
    const dev = transport(oldDaemon, "T", true);
    const s = dev.openSocket(dev.wsUrl("/api/events"));
    expect(await outcome(s)).toBe("open:");
    expect(s.mode).toBe("query");
  });

  it("the daemon refuses ?token= when TERRARIUM_WS_QUERY_TOKEN=0", () => {
    expect(acceptHandshake({ url: "ws://d/api/events?token=T", protocols: [] }, { token: "T", queryToken: false })).toBeNull();
    expect(acceptHandshake({ url: "ws://d/api/events?token=T", protocols: [] }, { token: "T", queryToken: true })).toBe("");
    expect(acceptHandshake({ url: "ws://d/api/events", protocols: ["bearer.T"] }, { token: "T", queryToken: false })).toBe("bearer.T");
    expect(acceptHandshake({ url: "ws://d/api/events?token=T", protocols: ["bearer.X"] }, { token: "T", queryToken: true })).toBeNull();
  });

  it("the mock's agent PTY, login PTY, exec console, and events use the subprotocol", async () => {
    const api = mock();
    const live = (await api.agents.list()).find((a) => ["idle", "working"].includes(a.status))!;
    const login = await api.subscriptions.startLogin({ kind: "claude" });
    const exec = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    const want = `open:${bearerProtocol(MOCK_TOKEN)}`;
    expect(await outcome(api.agents.pty(live.id))).toBe(want);
    expect(await outcome(api.subscriptions.loginPty(login.id))).toBe(want);
    expect(await outcome(api.targets.sessionPty("pg-test", exec.session_id))).toBe(want);
    const frames: AgentEvent[] = [];
    const stream = api.events.stream({ since: api.world.seq - 1 });
    stream.subscribe((e) => frames.push(e));
    await wait();
    expect(frames).toHaveLength(1);
    stream.close();
  });
});

describe("session contract (docs/api.md §1.1)", () => {
  /** The browser mock: the mock checks what the transport would send. */
  const browser = (extra: MockOptions = {}) => {
    const c = { token: null as string | null, csrf: null as string | null, unauthorized: 0 };
    const api = createMockApi({
      latencyMs: 0,
      eventsIntervalMs: 0,
      stepMs: 0,
      client: { token: () => c.token, csrf: () => c.csrf, onUnauthorized: () => void c.unauthorized++ },
      ...extra,
    });
    const signIn = async () => {
      const g = await api.session.create(MOCK_TOKEN);
      c.csrf = g!.csrf;
      return g!;
    };
    return { api, c, signIn };
  };

  it("the password login: a cookie for the right password, the same 401 for each wrong one, 409 with no password", async () => {
    const { api, c } = browser();
    const g = await api.session.login(MOCK_PASSWORD);
    expect(g.csrf).toMatch(/^csrf_/);
    c.csrf = g.csrf;
    expect((await api.projects.list()).length).toBeGreaterThan(0);
    const e1 = await api.session.login("wrong").catch((e: unknown) => e);
    const e2 = await api.session.login("wrong 2").catch((e: unknown) => e);
    expect(e1).toBeInstanceOf(AuthError);
    expect((e1 as AuthError).message).toBe((e2 as AuthError).message);
    const none = browser({ password: null });
    await expect(none.api.session.login("x")).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("GET /api/session answers the cookie's CSRF value, or null with no session", async () => {
    const { api, c, signIn } = browser();
    expect(await api.session.current()).toBeNull();
    const { csrf } = await signIn();
    expect(await api.session.current()).toMatchObject({ kind: "browser", csrf, password_set: true });
    c.csrf = null;
    c.token = MOCK_TOKEN;
    expect(await api.session.current()).toMatchObject({ kind: "token" });
  });

  it("POST /api/session answers {csrf, exp, expires_at} for the right token, and 401 for a wrong one", async () => {
    const t = Date.parse("2026-10-24T10:00:00Z");
    const { api } = browser({ now: () => t, sessionTtlMs: 3600_000 });
    const g = (await api.session.create(MOCK_TOKEN))!;
    expect(Object.keys(g).sort()).toEqual(["csrf", "exp", "expires_at"]);
    expect(g.expires_at).toBe("2026-10-24T11:00:00Z");
    expect(g.exp).toBe(Date.parse(g.expires_at) / 1000);
    const err = await api.session.create("wrong").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AuthError);
    expect(err).toMatchObject({ status: 401, code: "unauthorized" });
  });

  it("the cookie authenticates reads; a mutating call needs X-CSRF-Token, else 403", async () => {
    const { api, c, signIn } = browser();
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
    expect(c.unauthorized).toBe(1);
    const { csrf } = await signIn();
    expect((await api.projects.list()).length).toBeGreaterThan(0);
    c.csrf = null;
    await expect(api.cards.create("terrarium", { title: "no csrf" })).rejects.toMatchObject({ status: 403, code: "forbidden" });
    c.csrf = "wrong";
    await expect(api.cards.create("terrarium", { title: "bad csrf" })).rejects.toMatchObject({ status: 403 });
    c.csrf = csrf;
    expect((await api.cards.create("terrarium", { title: "with csrf" })).title).toBe("with csrf");
  });

  it("guards every mutating Api method of the mock with the CSRF check", async () => {
    const { api, c, signIn } = browser();
    await signIn();
    c.csrf = null;
    const verbs = httpVerbs();
    const checked: string[] = [];
    const pending: Promise<void>[] = [];
    eachMethod(api, (name, call) => {
      const verb = verbs.get(name);
      if (!verb || !isMutating(verb) || name.startsWith("session.")) return;
      checked.push(name);
      pending.push(
        (call() as Promise<unknown>).then(
          () => expect.fail(`${name} took a mutating call without X-CSRF-Token`),
          (e: unknown) => expect((e as ApiError).status, name).toBe(403),
        ),
      );
    });
    await Promise.all(pending);
    // All but the session.* methods (the sign-in and the logout), which check themselves.
    expect(checked.length).toBe([...verbs].filter(([k, v]) => isMutating(v) && !k.startsWith("session.")).length);
    expect(checked).toEqual(expect.arrayContaining(["account.setPassword", "account.logoutEverywhere"]));
  });

  it("every method of the HTTP client has a verb, and the mock has every method", () => {
    const verbs = httpVerbs();
    const mockNames: string[] = [];
    eachMethod(mock(), (name) => void mockNames.push(name));
    for (const name of verbs.keys()) expect(mockNames).toContain(name);
    expect(verbs.size + NOT_TRANSPORT.size).toBe(mockNames.filter((n) => !n.startsWith("world.") && !n.startsWith("sim.")).length);
  });

  it("the bearer token still works without a session", async () => {
    const { api, c } = browser();
    c.token = MOCK_TOKEN;
    expect((await api.cards.create("terrarium", { title: "bearer" })).title).toBe("bearer");
    c.token = "wrong";
    await expect(api.projects.list()).rejects.toBeInstanceOf(AuthError);
  });

  it("the cookie ends at expires_at: 401", async () => {
    let t = Date.parse("2026-10-24T10:00:00Z");
    const { api, signIn } = browser({ now: () => t, sessionTtlMs: 60_000 });
    await signIn();
    await api.projects.list();
    t += 60_000;
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
  });

  it("DELETE /api/session needs the CSRF header, then clears the cookie", async () => {
    const { api, c, signIn } = browser();
    const { csrf } = await signIn();
    c.csrf = null;
    await expect(api.session.remove()).rejects.toMatchObject({ status: 403 });
    c.csrf = csrf;
    await api.session.remove();
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
  });

  it("a daemon without sessions: POST /api/session is 404, which the client answers as null", async () => {
    const { api } = browser({ sessions: false });
    expect(await api.session.create(MOCK_TOKEN)).toBeNull();
    await expect(api.session.remove()).resolves.toBeUndefined();
  });

  it("keeps the cookie in the jar across a reload, as a browser does", async () => {
    const m = new Map<string, string>();
    const jar = { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), removeItem: (k: string) => void m.delete(k) };
    const first = browser({ cookieJar: jar });
    const { csrf } = await first.signIn();
    const again = browser({ cookieJar: jar });
    again.c.csrf = csrf;
    expect((await again.api.cards.create("terrarium", { title: "after reload" })).id).toMatch(/^TER-/);
  });

  it("WebSockets: with the cookie, no subprotocol and the dashboard Origin; without it, refused", async () => {
    const { api, signIn } = browser();
    const live = (await (async () => {
      await signIn();
      return api.agents.list();
    })()).find((a) => ["idle", "working"].includes(a.status))!;
    const login = await api.subscriptions.startLogin({ kind: "claude" });
    const exec = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    const sockets = [api.agents.pty(live.id), api.subscriptions.loginPty(login.id), api.targets.sessionPty("pg-test", exec.session_id)];
    expect(await Promise.all(sockets.map(outcome))).toEqual(["open:", "open:", "open:"]);
    expect(sockets.map((s) => (s as AuthSocket).mode)).toEqual(["cookie", "cookie", "cookie"]);
    const frames: AgentEvent[] = [];
    const stream = api.events.stream({ since: api.world.seq - 1 });
    stream.subscribe((e) => frames.push(e));
    await wait();
    expect(frames).toHaveLength(1);
    stream.close();

    await api.session.remove().catch(() => undefined);
    const signedOut = browser();
    expect(await outcome(signedOut.api.agents.pty(live.id))).toBe("closed");
  });

  it("the daemon takes a cookie upgrade only from the dashboard Origin", () => {
    const h = { url: "ws://d/api/events", protocols: [] as string[], cookie: true };
    expect(acceptHandshake({ ...h, origin: MOCK_ORIGIN }, MOCK_RULES)).toBe("");
    expect(acceptHandshake({ ...h, origin: "https://evil.example" }, MOCK_RULES)).toBeNull();
    expect(acceptHandshake(h, MOCK_RULES)).toBeNull();
    expect(acceptHandshake({ ...h, origin: MOCK_ORIGIN }, { ...MOCK_RULES, origins: [] })).toBeNull();
    // The bearer subprotocol wins over the cookie, as the daemon reads Authorization first.
    expect(acceptHandshake({ ...h, origin: "https://evil.example", protocols: [`bearer.${MOCK_TOKEN}`] }, MOCK_RULES)).toBe(`bearer.${MOCK_TOKEN}`);
    // No cookie and no token: refused, and `?token=` is off by default.
    expect(acceptHandshake({ url: `ws://d/api/events?token=${MOCK_TOKEN}`, protocols: [], origin: MOCK_ORIGIN }, MOCK_RULES)).toBeNull();
  });
});


describe("mail parity with docs/api.md §7 (the mail log's filters)", () => {
  it("lists oldest first by ID, and the cursor asks for IDs after it", async () => {
    const api = mock();
    const all = (await api.mail.list({ limit: 1000 })).items;
    expect(all.map((m) => m.id)).toEqual(all.map((m) => m.id).sort());
    const first = await api.mail.list({ limit: 4 });
    expect(first.next).toBe(first.items.at(-1)!.id);
    const second = await api.mail.list({ limit: 4, cursor: first.next! });
    expect(second.items[0]!.id > first.next!).toBe(true);
    // A cursor that is not a row still means "after this ID", as `id > ?` does.
    const after = await api.mail.list({ cursor: `${all[2]!.id}0` });
    expect(after.items.map((m) => m.id)).toEqual(all.slice(3).map((m) => m.id));
    expect((await api.mail.list({ cursor: all.at(-1)!.id })).items).toEqual([]);
  });

  it("filters by to, from, thread, and state; `system` is no filter (404); a bad state is 400", async () => {
    const api = mock();
    const to = (await api.mail.list({ to: "operator" })).items;
    expect(to.length).toBeGreaterThan(0);
    expect(to.every((m) => m.to === "operator" && m.to_kind === "operator")).toBe(true);
    const from = (await api.mail.list({ from: "operator" })).items;
    expect(from.every((m) => m.from === "operator" && m.from_kind === "operator")).toBe(true);
    // The daemon answers 404 "no agent `system`"; the log filters `system` in the browser.
    await expect(api.mail.list({ from: "system" })).rejects.toMatchObject({ status: 404 });
    const sys = (await api.mail.list({ limit: 1000 })).items.filter((m) => m.from === "system");
    expect(sys.length).toBeGreaterThan(0);
    expect(sys.every((m) => m.from_kind === "system" && m.from_name === "system")).toBe(true);
    const unread = (await api.mail.list({ state: "unread" })).items;
    expect(unread.every((m) => m.state === "queued" || m.state === "delivered")).toBe(true);
    const th = (await api.mail.list({ thread: to[0]!.thread_id })).items;
    expect(th.every((m) => m.thread_id === to[0]!.thread_id)).toBe(true);
    await expect(api.mail.list({ state: "bogus" as "read" })).rejects.toMatchObject({ status: 400 });
  });

  it("sets read_by on the first read and on a reply", async () => {
    const api = mock();
    const m = (await api.mail.list({ to: "operator", state: "unread" })).items[0]!;
    expect(m.read_by).toBeUndefined();
    const read = await api.mail.markRead(m.id);
    expect(read).toMatchObject({ state: "read", read_by: "operator", read_by_name: "operator" });
    expect(read.read_at).toBeTruthy();
    const again = (await api.mail.list({ to: "operator", state: "unread" })).items[0]!;
    await api.mail.reply(again.id, "Seen.");
    expect(await api.mail.get(again.id)).toMatchObject({ state: "replied", read_by: "operator" });
  });

  it("filters by project, time, and free text on the server", async () => {
    const api = mock();
    const all = (await api.mail.list({ limit: 1000 })).items;
    const webshop = (await api.mail.list({ project: "webshop", limit: 1000 })).items;
    expect(webshop.length).toBeGreaterThan(0);
    expect(webshop.every((m) => [m.from_name, m.to_name].some((n) => n === "project:webshop" || n.startsWith("webshop/")))).toBe(true);
    await expect(api.mail.list({ project: "nope" })).rejects.toMatchObject({ status: 404 });
    const mid = all[Math.floor(all.length / 2)]!.created_at;
    const since = (await api.mail.list({ since: mid, limit: 1000 })).items;
    const until = (await api.mail.list({ until: mid, limit: 1000 })).items;
    expect(since.every((m) => m.created_at >= mid)).toBe(true);
    expect(until.every((m) => m.created_at <= mid)).toBe(true);
    expect(since.length + until.length).toBeGreaterThanOrEqual(all.length);
    await expect(api.mail.list({ since: "yesterday" })).rejects.toMatchObject({ status: 400 });
    const flaky = (await api.mail.list({ q: "FLAKY", limit: 1000 })).items;
    expect(flaky.length).toBeGreaterThan(0);
    expect(flaky.every((m) => /flaky/i.test(m.subject + m.body + m.from_name + m.to_name))).toBe(true);
  });

  it("refuses mail to `system` and mail as `system`", async () => {
    const api = mock();
    await expect(api.mail.send({ to: "system", body: "hi" })).rejects.toMatchObject({ status: 422 });
    await expect(api.mail.send({ from: "system", to: "operator", body: "hi" })).rejects.toMatchObject({ status: 403 });
  });

  it("holds agent mail past the loop limit, flags the event, marks the thread paused, and notices the operator once", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const a = await api.mail.send({ from: "terrarium/lead", to: "terrarium/runner-dev", subject: "loop", body: "1" });
    for (let i = 2; i <= 6; i++) await api.mail.send({ from: i % 2 ? "terrarium/lead" : "terrarium/runner-dev", to: i % 2 ? "terrarium/runner-dev" : "terrarium/lead", thread_id: a.thread_id, body: String(i) });
    expect((await api.mail.list({ thread: a.thread_id })).items.every((m) => !m.thread_paused && m.state === "queued")).toBe(true);
    const held = await api.mail.send({ from: "terrarium/lead", to: "terrarium/runner-dev", thread_id: a.thread_id, body: "7" });
    await api.mail.send({ from: "terrarium/runner-dev", to: "terrarium/lead", thread_id: a.thread_id, body: "8" });
    await wait();
    expect(held).toMatchObject({ state: "delivered", thread_paused: true });
    const ev = frames.find((e) => e.kind === "mail" && e.data.message_id === held.id)!;
    expect(ev.data).toMatchObject({ held: true, thread_state: "paused" });
    expect(Object.keys(ev.data).sort()).toEqual(["agent_id", "from", "from_name", "held", "message_id", "subject", "thread_id", "thread_state", "to", "to_name"]);
    const thread = (await api.mail.list({ thread: a.thread_id })).items;
    expect(thread.every((m) => m.thread_paused === true)).toBe(true);
    const notices = thread.filter((m) => m.from === "system");
    expect(notices).toHaveLength(1);
    expect(notices[0]).toMatchObject({ to: "operator", subject: `Thread ${a.thread_id} needs a human`, state: "delivered" });
    // An operator message ends the pause; `thread_paused` is absent when false.
    await api.mail.reply(held.id, "Stop here.");
    expect((await api.mail.list({ thread: a.thread_id })).items.some((m) => "thread_paused" in m)).toBe(false);
    s.close();
  });

  it("sends the blocked-card notice and the quota alert from `system` to the operator", async () => {
    const api = mock();
    await api.cards.move("TER-13", { column: "blocked", comment: "Needs a login." });
    const sys = (await api.mail.list({ to: "operator", limit: 1000 })).items.filter((m) => m.from === "system");
    expect(sys.some((m) => m.subject.startsWith("TER-13 blocked: ") && m.body.includes("Needs a login."))).toBe(true);
    api.sim.setWindow("anthropic-max", "5h", 85);
    const after = (await api.mail.list({ limit: 1000 })).items.filter((m) => m.from === "system");
    expect(after.some((m) => m.subject === "Quota: anthropic-max 5h window at 85%" && m.to === "operator")).toBe(true);
  });
});

describe("schedules (docs/api.md §19, the daemon's api/schedules.rs)", () => {
  const SCHEDULE_KEYS = ["action", "catch_up", "consecutive_skips", "created_at", "created_by", "enabled", "id", "last_run", "max_concurrency", "name", "next_run_at", "project", "run_as", "runs_count", "source", "spec", "updated_at"];
  const RUN_KEYS = ["actor", "error", "event_seq", "finished_at", "id", "output", "schedule", "started_at", "status", "summary", "trigger"];
  const stream = (api: ReturnType<typeof mock>) => {
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    return { frames: () => frames.filter((e) => e.kind === "schedule"), close: () => s.close() };
  };
  const exec = { type: "exec" as const, target: "build-box", cmd: ["make", "test"] };
  const err = (p: Promise<unknown>) => p.then(() => null, (e: unknown) => e as ApiError);

  it("seeds three project schedules across two projects and one global, in the daemon's shape", async () => {
    const api = mock();
    const all = await api.schedules.list({ kind: "time" });
    expect(all.map((s) => s.project ?? "global").sort()).toEqual(["global", "terrarium", "terrarium", "webshop"]);
    for (const s of all) expect(Object.keys(s).sort()).toEqual(SCHEDULE_KEYS);
    const failed = all.find((s) => s.last_run?.status === "failed")!;
    expect(failed.project).toBe("webshop");
    expect(Object.keys(failed.last_run!).sort()).toEqual(["at", "run_id", "status", "summary"]);
    // A file schedule is in projects/<name>.toml, so it has a project.
    expect(all.filter((s) => s.source === "file").every((s) => s.project)).toBe(true);
  });

  it("lists the global schedules and the project's for ?project=, and filters ?enabled=", async () => {
    const api = mock();
    const t = await api.schedules.list({ project: "terrarium" });
    expect(t.every((s) => s.project === null || s.project === "terrarium")).toBe(true);
    expect(t.some((s) => s.project === null)).toBe(true);
    expect(t.some((s) => s.project === "webshop")).toBe(false);
    expect((await api.schedules.list({ enabled: false })).map((s) => s.enabled)).toEqual([false]);
    await expect(api.schedules.list({ project: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("creates each action type with 201 shapes, resolves agent keys, computes next_run_at, and emits created with IDs", async () => {
    const api = mock();
    const ev = stream(api);
    await wait();
    const actions = [
      { type: "mail" as const, to: "terrarium/lead", subject: "", body: "b" },
      { type: "turn" as const, agent: "terrarium/lead", text: "go", start_if_stopped: true },
      exec,
      { type: "agent" as const, agent: "lead", verb: "restart" as const },
      { type: "probe" as const, subscription: "anthropic-max" },
    ];
    for (const [i, action] of actions.entries()) {
      const s = await api.schedules.create({ name: `s${i}`, project: "terrarium", spec: { kind: "cron", expr: "*/5 * * * *", tz: "UTC" }, action });
      expect(s).toMatchObject({ project: "terrarium", enabled: true, catch_up: "skip", source: "api", runs_count: 0, last_run: null, max_concurrency: 1 });
      expect(Date.parse(s.next_run_at!)).toBeGreaterThan(Date.now());
      expect(s.action.type).toBe(action.type);
    }
    expect((await api.schedules.list()).find((s) => s.name === "s3")!.action).toEqual({ type: "agent", agent: "terrarium/lead", verb: "restart" });
    const e = ev.frames().find((x) => x.data.change === "created")!;
    expect(e).toMatchObject({ kind: "schedule", project: "terrarium", summary: "schedule terrarium/s0: created" });
    expect(Object.keys(e.data).sort()).toEqual(["change", "name", "project", "schedule"]);
    expect(typeof e.data.schedule).toBe("string");
    const g = await api.schedules.create({ name: "once", project: null, spec: { kind: "at", at: new Date(Date.now() + 3_600_000).toISOString() }, action: { type: "mail", to: "operator", subject: "Hi", body: "" } });
    expect(g.project).toBeNull();
    expect((await api.schedules.create({ name: "iv", project: "webshop", spec: { kind: "interval", every: "1h30m" }, action: exec })).next_run_at).toBeTruthy();
    expect(api.world.audit.at(-1)).toMatchObject({ action: "schedule.create", target: "iv" });
    ev.close();
  });

  it("refuses bad fields with 422 invalid, details.field, and the message `<field>: ...`; a cron error quotes the expression", async () => {
    const api = mock();
    const ok = { name: "x", project: "terrarium", spec: { kind: "interval" as const, every: "15m" }, action: exec };
    const bad = (patch: object) => err(api.schedules.create({ ...ok, ...patch } as never)).then((e) => e!);
    const cron = await bad({ spec: { kind: "cron", expr: "61 * * * *", tz: "UTC" } });
    expect(cron).toMatchObject({ status: 422, code: "invalid", details: { field: "spec.expr" } });
    expect(cron.message).toMatch(/^spec\.expr: bad cron expression `61 \* \* \* \*`/);
    expect(await bad({ spec: { kind: "cron", expr: "0 9 * * *", tz: "Mars/Base" } })).toMatchObject({ status: 422, details: { field: "spec.tz" } });
    expect(await bad({ spec: { kind: "interval", every: "30s" } })).toMatchObject({ details: { field: "spec.every" } });
    expect(await bad({ spec: { kind: "at", at: "2000-01-01T00:00:00Z" } })).toMatchObject({ details: { field: "spec.at" } });
    expect(await bad({ name: "Bad Name" })).toMatchObject({ details: { field: "name" } });
    expect(await bad({ action: { type: "turn", agent: "nobody", text: "x" } })).toMatchObject({ details: { field: "action.agent" } });
    expect(await bad({ action: { type: "exec", target: "nope", cmd: ["ls"] } })).toMatchObject({ details: { field: "action.target" } });
    expect(await bad({ action: { type: "exec", target: "build-box", cmd: [] } })).toMatchObject({ details: { field: "action.cmd" } });
    expect(await bad({ action: { ...exec, timeout_secs: 4000 } })).toMatchObject({ details: { field: "action.timeout_secs" } });
    expect(await bad({ action: { type: "probe", subscription: "nope" } })).toMatchObject({ details: { field: "action.subscription" } });
    expect(await bad({ project: "nope" })).toMatchObject({ details: { field: "project" } });
    await api.schedules.create(ok);
    expect(await bad({})).toMatchObject({ status: 409, code: "conflict" });
  });

  it("answers 403 forbidden: a member writes nothing and sees no global schedule; a project_admin stays in its project; a global schedule needs a sysadmin", async () => {
    const member = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "terrarium" } });
    const list = await member.schedules.list({ project: "terrarium" });
    expect(list.every((s) => s.project === "terrarium")).toBe(true);
    const t = list.find((s) => s.name === "tests")!;
    await expect(member.schedules.disable(t.id)).rejects.toMatchObject({ status: 403, code: "forbidden" });
    await expect(member.schedules.run(t.id)).rejects.toMatchObject({ status: 403 });
    await expect(member.schedules.create({ name: "m", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: exec })).rejects.toMatchObject({ status: 403 });
    await expect(member.schedules.list({ project: "webshop" })).rejects.toMatchObject({ status: 403 });
    const admin = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "project_admin", project: "terrarium" } });
    const probe = { type: "probe" as const, subscription: "anthropic-max" };
    await expect(admin.schedules.create({ name: "g", project: null, spec: { kind: "interval", every: "1h" }, action: probe })).rejects.toMatchObject({ status: 403, message: expect.stringContaining("sysadmin") });
    await expect(admin.schedules.create({ name: "w", project: "webshop", spec: { kind: "interval", every: "1h" }, action: probe })).rejects.toMatchObject({ status: 403 });
    await expect(admin.schedules.create({ name: "x", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "mail", to: "webshop/shop-lead", subject: "", body: "x" } })).rejects.toMatchObject({ status: 403, message: expect.stringContaining("leaves the project") });
    await expect(admin.schedules.create({ name: "y", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "exec", target: "pg-test", cmd: ["ls"] } })).rejects.toMatchObject({ status: 403 });
    expect((await admin.schedules.create({ name: "a", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: exec })).project).toBe("terrarium");
    // The operator may reach across projects.
    const op = mock();
    expect((await op.schedules.create({ name: "z", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "exec", target: "pg-test", cmd: ["ls"] } })).action).toMatchObject({ target: "pg-test" });
  });

  it("patches (no project in the body), enables, disables (next_run_at null), and deletes, with an event for each", async () => {
    const api = mock();
    const ev = stream(api);
    await wait();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const p = await api.schedules.update(s.id, { spec: { kind: "cron", expr: "0 9 * * *", tz: "UTC" } });
    expect(p.spec).toEqual({ kind: "cron", expr: "0 9 * * *", tz: "UTC" });
    expect(p.action).toEqual(s.action);
    expect(new Date(p.next_run_at!).getUTCHours()).toBe(9);
    await expect(api.schedules.update(s.id, { project: "webshop" } as never)).rejects.toMatchObject({ status: 422 });
    expect(await api.schedules.disable(s.id)).toMatchObject({ enabled: false, next_run_at: null });
    expect((await api.schedules.enable(s.id)).next_run_at).toBeTruthy();
    await api.schedules.remove(s.id);
    await expect(api.schedules.get(s.id)).rejects.toMatchObject({ status: 404 });
    expect(ev.frames().map((e) => e.data.change)).toEqual(["updated", "disabled", "enabled", "deleted"]);
    const file = (await api.schedules.list()).find((x) => x.source === "file")!;
    for (const call of [api.schedules.remove(file.id), api.schedules.update(file.id, { enabled: false }), api.schedules.disable(file.id)])
      await expect(call).rejects.toMatchObject({ status: 409, code: "file_managed", message: expect.stringContaining("projects/terrarium.toml") });
    ev.close();
  });

  it("runs now: run_started at once, the answer is the run when it ends, then run_finished with {run, status, trigger}", async () => {
    // A fake clock: the run takes 15 ms, so "not finished yet" holds on any machine.
    const tick = fakeClock();
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleRunMs: 15 });
    const ev = stream(api);
    await tick();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const pending = api.schedules.run(s.id);
    await tick(2);
    expect(ev.frames().map((e) => e.data.change)).toEqual(["run_started"]);
    expect(ev.frames()[0]!.data).toMatchObject({ schedule: s.id, status: "running", trigger: "manual" });
    expect(typeof ev.frames()[0]!.data.run).toBe("string");
    // A second run while one goes on is skipped at once.
    const overlap = await api.schedules.run(s.id);
    expect(overlap).toMatchObject({ status: "skipped", summary: expect.stringContaining("still running") });
    await tick(15);
    const r = await pending;
    expect(Object.keys(r).sort()).toEqual(RUN_KEYS);
    expect(r).toMatchObject({ schedule: s.id, status: "ok", trigger: "manual", summary: "exec on build-box: exit 0" });
    expect(r.finished_at).toBeTruthy();
    expect(r.output).toMatchObject({ target: "build-box", exit_code: 0, stdout: expect.stringContaining("make test") });
    const finished = ev.frames().filter((e) => e.data.change === "run_finished").at(-1)!;
    expect(finished.data).toMatchObject({ schedule: s.id, run: r.id, status: "ok", trigger: "manual" });
    expect(finished.summary).toBe("schedule terrarium/tests: ok (exec on build-box: exit 0)");
    const after = await api.schedules.get(s.id);
    expect(after.runs_count).toBe(s.runs_count + 2);
    expect(after.last_run).toMatchObject({ run_id: r.id, status: "ok", at: r.finished_at });
    const failing = await api.schedules.create({ name: "f", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "exec", target: "build-box", cmd: ["false"] } });
    const failed = api.schedules.run(failing.id);
    await tick(15);
    expect(await failed).toMatchObject({ status: "failed", error: "exit code 1" });
    expect((await api.schedules.get(failing.id)).last_run).toMatchObject({ status: "failed" });
    ev.close();
  });

  it("pages runs newest first with a cursor", async () => {
    const api = mock();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const p1 = await api.schedules.runs(s.id, { limit: 10 });
    expect(p1.items).toHaveLength(10);
    expect(p1.items[0]!.started_at > p1.items[1]!.started_at).toBe(true);
    expect(p1.next).toBe(p1.items.at(-1)!.id);
    const p2 = await api.schedules.runs(s.id, { limit: 10, cursor: p1.next! });
    const p3 = await api.schedules.runs(s.id, { limit: 10, cursor: p2.next! });
    expect(p3.items).toHaveLength(5);
    expect(p3.next).toBeNull();
    expect(new Set([...p1.items, ...p2.items, ...p3.items].map((r) => r.id)).size).toBe(25);
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}`);
      return new Response(init.method === "DELETE" ? null : "[]", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.schedules.list({ project: "terrarium", enabled: true });
    await api.schedules.runs("S1", { limit: 20, cursor: "R9" });
    await api.schedules.run("S1");
    await api.schedules.enable("S1");
    await api.schedules.disable("S1");
    await api.schedules.update("S1", { enabled: false });
    await api.schedules.remove("S1");
    expect(calls).toEqual([
      "GET /api/schedules?project=terrarium&enabled=true&limit=1000",
      "GET /api/schedules/S1/runs?limit=20&cursor=R9",
      "POST /api/schedules/S1/run",
      "POST /api/schedules/S1/enable",
      "POST /api/schedules/S1/disable",
      "PATCH /api/schedules/S1",
      "DELETE /api/schedules/S1",
    ]);
  });

  it("gives each subscription default_models, as the daemon does", async () => {
    const api = mock();
    const subs = await api.subscriptions.list();
    for (const s of subs) expect(s.default_models).toBeTypeOf("object");
    const zai = subs.find((s) => s.name === "zai-key")!;
    expect(zai.default_models).toEqual({ pi: "glm-5.3-flash", claude: "glm-5.3" });
    expect(subs.find((s) => s.name === "kimi-key")!.default_models).toEqual({ pi: "kimi-for-coding" });
    // pi refuses a Claude plan: no pi default.
    expect(subs.find((s) => s.name === "anthropic-max")!.default_models).toEqual({});
  });

  it("adds subscription_detail to GET /api/agents/{id} only", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "est", project: "terrarium", harness: "pi", subscription: "zai-key" });
    expect(a.subscription_detail).toBeUndefined();
    const shown = await api.agents.get(a.id);
    expect(Object.keys(shown.subscription_detail!).sort()).toEqual(["default_model", "harnesses", "login_status", "name", "plan", "provider"]);
    expect((await api.agents.list()).every((x) => x.subscription_detail === undefined)).toBe(true);
  });

  it("answers GET /api/subscriptions/{name}/models in the daemon's shape", async () => {
    const api = mock();
    const zai = await api.subscriptions.models("zai-key");
    expect(zai.source).toBe("provider");
    expect(zai.models.map((m) => m.id)).toContain("glm-5.3-flash");
    expect(typeof zai.fetched_at).toBe("string");
    const pi = await api.subscriptions.models("openrouter-key", { refresh: true });
    expect(pi.source).toBe("pi");
    expect(pi.models[0]).toHaveProperty("context");
    const plan = await api.subscriptions.models("copilot-pro");
    expect(plan.source).toBe("plan");
    expect(plan.models.map((m) => m.id)).toContain("claude-sonnet-4.6");
    expect((await api.subscriptions.models("chatgpt-pro")).source).toBe("plan");
    const fb = await api.subscriptions.models("gemini");
    expect(fb.source).toBe("fallback");
    expect(fb.notes?.length).toBeGreaterThan(0);
    await expect(api.subscriptions.models("nope")).rejects.toMatchObject({ status: 404 });
  });
});

describe("triggers (docs/api.md §19.4)", () => {
  const on = { kind: "on" as const, kinds: ["ci.run.failed"] };
  const mail = { type: "mail" as const, to: "terrarium/lead", subject: "CI failed: {{event.data.repo}}", body: "Run: {{event.data.run_url}}" };
  const err = (p: Promise<unknown>) => p.then(() => null, (e: unknown) => e as ApiError);

  it("filters ?kind=time|trigger and answers 400 for another kind", async () => {
    const api = mock();
    const t = await api.schedules.list({ kind: "trigger" });
    expect(t.length).toBeGreaterThan(0);
    expect(t.every((s) => s.spec.kind === "on" && s.next_run_at === null)).toBe(true);
    expect((await api.schedules.list({ kind: "time" })).some((s) => s.spec.kind === "on")).toBe(false);
    await expect(api.schedules.list({ kind: "weekly" as never })).rejects.toMatchObject({ status: 400 });
  });

  it("gives each trigger run its event, and every run event_seq", async () => {
    const api = mock();
    const t = (await api.schedules.list({ kind: "trigger" }))[0]!;
    const runs = (await api.schedules.runs(t.id)).items;
    expect(runs.every((r) => r.trigger === "event" && typeof r.event_seq === "number" && r.event?.seq === r.event_seq)).toBe(true);
    expect(Object.keys(runs[0]!.event!).sort()).toEqual(["kind", "seq", "summary"]);
    const time = (await api.schedules.list({ kind: "time" }))[0]!;
    const r = (await api.schedules.runs(time.id)).items[0]!;
    expect(r.event_seq).toBeNull();
    expect(r).not.toHaveProperty("event");
  });

  it("creates a trigger with the default max_per_hour, and refuses turn, agent, and probe on the field action", async () => {
    const api = mock();
    const s = await api.schedules.create({ name: "t1", project: "terrarium", spec: on, action: mail });
    expect(s.spec).toEqual({ kind: "on", kinds: ["ci.run.failed"], max_per_hour: 30 });
    expect(s.next_run_at).toBeNull();
    for (const action of [
      { type: "turn" as const, agent: "terrarium/lead", text: "go" },
      { type: "agent" as const, agent: "terrarium/lead", verb: "restart" as const },
      { type: "probe" as const, subscription: "anthropic-max" },
    ])
      expect(await err(api.schedules.create({ name: "t2", project: "terrarium", spec: on, action }))).toMatchObject({ status: 422, details: { field: "action" } });
  });

  it("names the spec field of a bad trigger, and refuses an unknown placeholder", async () => {
    const api = mock();
    const bad = (spec: object, action: object = mail) => err(api.schedules.create({ name: "t", project: "terrarium", spec: { ...on, ...spec } as never, action: action as never }));
    expect(await bad({ kinds: [] })).toMatchObject({ details: { field: "spec.kinds" } });
    expect(await bad({ match: [{ path: "branch", op: "eq", value: "main" }] })).toMatchObject({ details: { field: "spec.match" } });
    expect(await bad({ match: [{ path: "data.repo", op: "in", value: "x" }] })).toMatchObject({ details: { field: "spec.match" } });
    expect(await bad({ dedupe: { key: "k", window: "8d" } })).toMatchObject({ details: { field: "spec.dedupe.window" } });
    expect(await bad({ escalate: { key: "k", after: 0 } })).toMatchObject({ details: { field: "spec.escalate.after" } });
    expect(await bad({ max_per_hour: 601 })).toMatchObject({ details: { field: "spec.max_per_hour" } });
    expect(await bad({}, { ...mail, subject: "{{event.nope}}" })).toMatchObject({ details: { field: "action.subject" } });
  });

  it("takes a card action on a project, not on a global schedule", async () => {
    const api = mock();
    const card = { type: "card" as const, op: "open" as const, key: "ci:{{event.data.repo}}", title: "CI failed" };
    expect((await api.schedules.create({ name: "c", project: "terrarium", spec: on, action: card })).action).toEqual(card);
    expect(await err(api.schedules.create({ name: "c", project: null, spec: on, action: card }))).toMatchObject({ details: { field: "action" } });
    expect(await err(api.schedules.create({ name: "c2", project: "terrarium", spec: on, action: { ...card, key: " " } }))).toMatchObject({ details: { field: "action.key" } });
    expect(await err(api.schedules.create({ name: "c3", project: "terrarium", spec: on, action: { ...card, column: "done" } }))).toMatchObject({ details: { field: "action.column" } });
    // A time schedule may use the card action too.
    expect((await api.schedules.create({ name: "weekly", project: "terrarium", spec: { kind: "cron", expr: "0 9 * * 1", tz: "UTC" }, action: { type: "card", op: "open", key: "review" } })).action.type).toBe("card");
  });
});

describe("event sources (docs/api.md §20)", () => {
  const SOURCE_KEYS = ["created_at", "created_by", "enabled", "hook_note", "hook_path", "hook_url", "id", "kind", "last_error", "last_seen", "name", "project", "received", "setup", "updated_at"];

  it("lists in the daemon's shape, with no secret", async () => {
    const api = mock();
    const all = await api.eventSources.list();
    expect(all.length).toBeGreaterThan(0);
    for (const s of all) {
      expect(Object.keys(s).sort()).toEqual(SOURCE_KEYS);
      expect(s.hook_path).toBe(`/hooks/${s.project ?? "_"}/${s.name}`);
    }
    expect((await api.eventSources.list({ project: "webshop" })).every((s) => s.project === null || s.project === "webshop")).toBe(true);
  });

  it("answers the secret only at create and rotate, with the setup steps of the kind", async () => {
    const api = mock();
    const s = await api.eventSources.create({ name: "fj", project: "terrarium", kind: "forgejo" });
    expect(s.secret).toMatch(/^[0-9a-f]{64}$/);
    expect(s.setup.join(" ")).toContain("application/json");
    expect(await api.eventSources.get(s.id)).not.toHaveProperty("secret");
    const r = await api.eventSources.rotate(s.id);
    expect(r.secret).not.toBe(s.secret);
    expect((await api.eventSources.list()).every((x) => !("secret" in x))).toBe(true);
    expect((await api.eventSources.update(s.id, { enabled: false })).enabled).toBe(false);
    await api.eventSources.remove(s.id);
    await expect(api.eventSources.get(s.id)).rejects.toMatchObject({ status: 404 });
  });

  it("refuses a bad name, a taken name, a global source from a project_admin, and any write from a member", async () => {
    const api = mock();
    await expect(api.eventSources.create({ name: "Bad Name", project: "terrarium", kind: "github" })).rejects.toMatchObject({ status: 422, details: { field: "name" } });
    await expect(api.eventSources.create({ name: "gh", project: "terrarium", kind: "github" })).rejects.toMatchObject({ status: 409 });
    const admin = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "project_admin", project: "terrarium" } });
    await expect(admin.eventSources.create({ name: "g", project: null, kind: "generic" })).rejects.toMatchObject({ status: 403 });
    expect((await admin.eventSources.create({ name: "g", project: "terrarium", kind: "generic" })).project).toBe("terrarium");
    const member = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "terrarium" } });
    const gh = (await member.eventSources.list({ project: "terrarium" })).find((x) => x.name === "gh")!;
    await expect(member.eventSources.rotate(gh.id)).rejects.toMatchObject({ status: 403 });
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}`);
      return new Response(init.method === "DELETE" ? null : "[]", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.schedules.list({ project: "terrarium", kind: "trigger" });
    await api.eventSources.list({ project: "terrarium" });
    await api.eventSources.create({ name: "gh", kind: "github", project: "terrarium" });
    await api.eventSources.update("S1", { enabled: false });
    await api.eventSources.rotate("terrarium/gh");
    await api.eventSources.remove("S1");
    expect(calls).toEqual([
      "GET /api/schedules?project=terrarium&kind=trigger&limit=1000",
      "GET /api/event-sources?project=terrarium",
      "POST /api/event-sources",
      "PATCH /api/event-sources/S1",
      "POST /api/event-sources/terrarium%2Fgh/rotate",
      "DELETE /api/event-sources/S1",
    ]);
  });
});

describe("agent templates (docs/api.md §21)", () => {
  const TEMPLATE_KEYS = ["created_at", "created_by", "description", "file", "id", "name", "project", "revision", "source", "spec", "updated_at"];

  it("lists in the daemon's shape, global first, a project's with the global ones", async () => {
    const api = mock();
    const all = await api.templates.list();
    expect(all.length).toBeGreaterThan(1);
    for (const t of all) expect(Object.keys(t).sort()).toEqual(TEMPLATE_KEYS);
    expect(all[0]!.project).toBeNull();
    expect((await api.templates.list({ project: "webshop" })).every((t) => t.project === null || t.project === "webshop")).toBe(true);
    expect((await api.templates.list({ scope: "global" })).every((t) => t.project === null)).toBe(true);
  });

  it("normalizes the spec, keeps the name unique in its scope, and counts revisions", async () => {
    const api = mock();
    const t = await api.templates.create({ name: "tester", project: "terrarium", spec: { harness: "pi", roles: [] } });
    expect(t.spec).toEqual({ harness: "pi", policy: { targets: [], mail_to: ["*"] }, restart: "on-failure" });
    expect(t.revision).toBe(1);
    // The same name in another scope is fine; in the same scope it is a conflict.
    expect((await api.templates.create({ name: "tester", project: null, spec: { harness: "claude" } })).project).toBeNull();
    await expect(api.templates.create({ name: "tester", project: "terrarium", spec: { harness: "pi" } })).rejects.toMatchObject({ status: 409 });
    await expect(api.templates.create({ name: "Bad", spec: { harness: "pi" } })).rejects.toMatchObject({ status: 422, details: { field: "name" } });
    await expect(api.templates.create({ name: "x", spec: { harness: "nope" } as never })).rejects.toMatchObject({ status: 422, details: { field: "spec" } });
    await expect(api.templates.create({ name: "x", spec: { harness: "pi", name: "w" } as never })).rejects.toMatchObject({ status: 422, details: { field: "spec" } });
    const u = await api.templates.update("terrarium/tester", { description: "runs the tests" });
    expect(u.revision).toBe(2);
    expect((await api.templates.update(u.id, { description: "runs the tests" })).revision).toBe(2);
    await api.templates.remove(u.id);
    await expect(api.templates.get(u.id)).rejects.toMatchObject({ status: 404 });
  });

  it("serves the WCL file of each template, and a rename moves it", async () => {
    const api = mock();
    const t = await api.templates.create({ name: "docs", project: "terrarium", description: "Docs writer", spec: { harness: "pi", system_prompt: "Line one.\nLine two.\n", policy: { targets: [], mail_to: ["lead"] } } });
    expect(t).toMatchObject({ source: "config", file: "templates/terrarium/docs.wcl" });
    const f = await api.templates.wcl(t.id);
    expect(f).toMatchObject({ path: "templates/terrarium/docs.wcl", on_disk: true });
    expect(f.text).toContain('description = "Docs writer"');
    expect(f.text).toContain("system_prompt = <<'PROMPT'\n  Line one.\n  Line two.\n  PROMPT\n");
    expect(f.text).toContain('policy {\n  targets = []\n  mail_to = ["lead"]\n}\n');
    expect((await api.templates.update(t.id, { name: "writer" })).file).toBe("templates/terrarium/writer.wcl");
  });

  it("resolves the project's template before the global one, and global: forces the global one", async () => {
    const api = mock();
    await api.templates.create({ name: "worker", project: "terrarium", spec: { harness: "pi" } });
    expect((await api.templates.resolve("worker", "terrarium")).agent).toMatchObject({ project: "terrarium", harness: "pi" });
    const g = await api.templates.resolve("global:worker", "terrarium");
    expect(g.template.project).toBeNull();
    expect(g.agent.harness).toBe("claude");
    expect((await api.templates.resolve("worker", "webshop")).template.project).toBeNull();
    await expect(api.templates.resolve("reviewer", "webshop")).rejects.toMatchObject({ status: 404 });
    await expect(api.templates.resolve("terrarium/reviewer", "webshop")).rejects.toMatchObject({ status: 422, code: "invalid", details: { field: "template" } });
  });

  it("makes an agent from a template: the body wins per field, policy merges per key, null clears", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "r1", project: "terrarium", template: "reviewer", model: null, policy: { mail_to: ["*"] } as never, restart: "never" });
    expect(a).toMatchObject({ harness: "codex", prompt: "worker", restart: "never", policy: { targets: ["build-box"], mail_to: ["*"] } });
    expect(a.model).toBeUndefined();
    expect(a.template).toMatchObject({ name: "reviewer", project: "terrarium", revision: 1 });
    await expect(api.agents.create({ name: "r2", project: "terrarium", template: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("keeps system_prompt: create, template override and clear, patch, from-agent, the 32 KiB limit", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "sp1", project: "terrarium", harness: "claude", system_prompt: "Be brief." });
    expect(a.system_prompt).toBe("Be brief.");
    expect((await api.agents.create({ name: "sp2", project: "terrarium", harness: "claude", system_prompt: "" })).system_prompt).toBeUndefined();
    await expect(api.agents.create({ name: "sp3", project: "terrarium", harness: "claude", system_prompt: "a".repeat(32769) })).rejects.toMatchObject({ status: 422, code: "invalid" });
    // Patch: a new text, then null clears it.
    expect((await api.agents.update(a.id, { system_prompt: "Be terse." })).system_prompt).toBe("Be terse.");
    await expect(api.agents.update(a.id, { system_prompt: "a".repeat(32769) })).rejects.toMatchObject({ status: 422, code: "invalid" });
    const t = await api.templates.fromAgent(a.id, { name: "sp-tpl" });
    expect(t.spec.system_prompt).toBe("Be terse.");
    expect((await api.agents.update(a.id, { system_prompt: null })).system_prompt).toBeUndefined();
    // A template's system_prompt: copied at the create; the body wins; null clears.
    expect((await api.templates.resolve("sp-tpl", "terrarium")).agent.system_prompt).toBe("Be terse.");
    expect((await api.agents.create({ name: "sp4", project: "terrarium", template: "sp-tpl" })).system_prompt).toBe("Be terse.");
    expect((await api.agents.create({ name: "sp5", project: "terrarium", template: "sp-tpl", system_prompt: "Mine." })).system_prompt).toBe("Mine.");
    expect((await api.agents.create({ name: "sp6", project: "terrarium", template: "sp-tpl", system_prompt: null })).system_prompt).toBeUndefined();
    await expect(api.templates.create({ name: "big", project: null, spec: { harness: "claude", system_prompt: "a".repeat(32769) } })).rejects.toMatchObject({ status: 422, details: { field: "spec" } });
    // An empty one is left out of the spec.
    expect((await api.templates.create({ name: "empty-sp", project: null, spec: { harness: "claude", system_prompt: "" } })).spec).not.toHaveProperty("system_prompt");
  });

  it("keeps persona: create, patch, null back to by role, templates, the name check", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "pe1", project: "terrarium", harness: "claude", persona: "leader" });
    expect(a.persona).toBe("leader");
    expect((await api.agents.create({ name: "pe2", project: "terrarium", harness: "claude", persona: "" })).persona).toBeUndefined();
    await expect(api.agents.create({ name: "pe3", project: "terrarium", harness: "claude", persona: "../x" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    expect((await api.agents.update(a.id, { persona: "none" })).persona).toBe("none");
    const t = await api.templates.fromAgent(a.id, { name: "pe-tpl" });
    expect(t.spec.persona).toBe("none");
    expect((await api.agents.update(a.id, { persona: null })).persona).toBeUndefined();
    expect((await api.agents.create({ name: "pe4", project: "terrarium", template: "pe-tpl" })).persona).toBe("none");
    expect((await api.agents.create({ name: "pe5", project: "terrarium", template: "pe-tpl", persona: null })).persona).toBeUndefined();
    await expect(api.templates.create({ name: "bad-pe", project: null, spec: { harness: "claude", persona: "a b" } })).rejects.toMatchObject({ status: 422 });
  });

  it("the HTTP client sends the documented config repo routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}${init.body ? ` ${String(init.body)}` : ""}`);
      return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.config.status();
    await api.config.diff();
    await api.config.diff("agents/a b.toml");
    await api.config.log(5);
    await api.config.commit({ message: "m" });
    await api.config.pull();
    await api.config.pull({ apply: true });
    await api.config.pull({ apply: true, approve_grants: true });
    await api.config.push();
    await api.config.sync({ message: "m" });
    await api.config.sync({ apply: true, approve_grants: true });
    await api.config.abort();
    await api.config.setRemote({ url: "git@h:c.git", branch: "main" });
    await api.config.clearRemote();
    await api.config.defaults();
    await api.config.acceptDefault("prompts/leader.md");
    await api.config.keepDefault("prompts/leader.md");
    expect(calls).toEqual([
      "GET /api/config/status",
      "GET /api/config/diff",
      "GET /api/config/diff?path=agents%2Fa+b.toml",
      "GET /api/config/log?limit=5",
      'POST /api/config/commit {"message":"m"}',
      "POST /api/config/pull {}",
      'POST /api/config/pull {"apply":true}',
      'POST /api/config/pull {"apply":true,"approve_grants":true}',
      "POST /api/config/push",
      'POST /api/config/sync {"message":"m"}',
      'POST /api/config/sync {"apply":true,"approve_grants":true}',
      "POST /api/config/abort",
      'PUT /api/config/remote {"url":"git@h:c.git","branch":"main"}',
      "DELETE /api/config/remote",
      "GET /api/config/defaults",
      'POST /api/config/defaults/accept {"path":"prompts/leader.md"}',
      'POST /api/config/defaults/keep {"path":"prompts/leader.md"}',
    ]);
  });

  it("holds a new or wider grant of a secret with a value until the operator gives approve_grants (§25, S8)", async () => {
    const api = mock();
    await api.config.commit({ message: "local" });
    const grants = (r: { secrets?: { changes: { action: string; secret: string; approval?: string }[] } }) =>
      (r.secrets?.changes ?? []).filter((c) => c.action === "grant" && c.approval).map((c) => `${c.secret} ${c.approval}`);
    const p = await api.config.pull();
    expect(grants(p)).toEqual(["global/gh needed", "terrarium/db needed"]);
    // The grant of a placeholder needs no approval.
    expect(p.secrets!.changes).toContainEqual({ action: "grant", secret: "terrarium/npm", grantee: "agent:terrarium/lead", level: "use" });
    expect(p.secrets!.changes).toContainEqual(expect.objectContaining({ secret: "terrarium/db", level: "read", was: { level: "use" } }));
    // An apply without approval holds them, and the export stays pending.
    const held = await api.config.pull({ apply: true });
    expect(grants(held)).toEqual(["global/gh held", "terrarium/db held"]);
    expect((await api.config.status()).export_pending).toBe(true);
    // An agent cannot approve them.
    const agent = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "sysadmin" } });
    await expect(agent.config.pull({ apply: true, approve_grants: true })).rejects.toMatchObject({ status: 403, code: "forbidden" });
    await expect(agent.config.sync({ apply: true, approve_grants: true })).rejects.toMatchObject({ status: 403, code: "forbidden" });
    // The operator approves them.
    const ok = await api.config.pull({ apply: true, approve_grants: true });
    expect(grants(ok)).toEqual(["global/gh approved", "terrarium/db approved"]);
    expect((await api.config.status()).export_pending).toBe(false);
  });

  it("makes a template from an agent, in the agent's project by default", async () => {
    const api = mock();
    const t = await api.templates.fromAgent("terrarium/lead", { name: "lead-copy" });
    const lead = await api.agents.get("terrarium/lead");
    expect(t.project).toBe("terrarium");
    expect(t.spec.harness).toBe(lead.harness);
    expect((await api.templates.fromAgent("terrarium/lead", { name: "lead-copy", project: null })).project).toBeNull();
  });

  it("follows the rights: everyone reads global; a project_admin manages its project's; a member only reads", async () => {
    const admin = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "project_admin", project: "terrarium" } });
    await expect(admin.templates.create({ name: "g", project: null, spec: { harness: "pi" } })).rejects.toMatchObject({ status: 403 });
    await expect(admin.templates.create({ name: "w", project: "webshop", spec: { harness: "pi" } })).rejects.toMatchObject({ status: 403 });
    expect((await admin.templates.create({ name: "w", spec: { harness: "pi" } })).project).toBe("terrarium");
    const member = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "webshop" } });
    const seen = await member.templates.list();
    expect(seen.some((t) => t.project === null)).toBe(true);
    expect(seen.some((t) => t.project === "terrarium")).toBe(false);
    await expect(member.templates.update("global:worker", { description: "x" })).rejects.toMatchObject({ status: 403 });
    await expect(member.templates.get("terrarium/reviewer")).rejects.toMatchObject({ status: 403 });
  });

  it("emits template events", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const t = await api.templates.create({ name: "ev", project: "terrarium", spec: { harness: "pi" } });
    await wait();
    const e = frames.find((x) => x.kind === "template")!;
    expect(e).toMatchObject({ project: "terrarium", data: { change: "created", template: t.id, name: "ev", key: "terrarium/ev", revision: 1 } });
    s.close();
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}`);
      return new Response(init.method === "DELETE" ? null : "[]", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.templates.list({ scope: "global" });
    await api.templates.create({ name: "w", spec: { harness: "pi" } });
    await api.templates.get("global:worker");
    await api.templates.update("terrarium/w", { description: "d" });
    await api.templates.resolve("worker", "terrarium");
    await api.templates.fromAgent("terrarium/lead", { name: "l" });
    await api.templates.remove("T1");
    expect(calls).toEqual([
      "GET /api/templates?scope=global",
      "POST /api/templates",
      "GET /api/templates/global%3Aworker",
      "PATCH /api/templates/terrarium%2Fw",
      "GET /api/templates/worker/resolve?project=terrarium",
      "POST /api/templates/from-agent/terrarium%2Flead",
      "DELETE /api/templates/T1",
    ]);
  });
});

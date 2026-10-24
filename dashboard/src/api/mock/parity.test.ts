// The mock must answer in the daemon's shapes (docs/api.md), so mock mode and
// real mode never diverge.
import { describe, expect, it } from "vitest";
import { ApiError } from "../http";
import { createMockApi } from ".";
import type { AgentEvent } from "../types";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));

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
    const m = (await api.inbox.list("terrarium")).items[0]!;
    const sent = await api.inbox.reply("terrarium", m.id, "Accepted.");
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
    await wait(20);
    const statuses = frames.filter((e) => e.kind === "status");
    expect(statuses.map((e) => e.data.status)).toEqual(["starting", "idle"]);
    const e = statuses[0]!;
    expect(e).toMatchObject({ kind: "status", actor: "system", agent: "sysadmin", agent_id: a.id, project: "terrarium" });
    expect(e.data).toMatchObject({ agent_id: a.id, status: "starting", from: "stopped", reason: null });
    expect(typeof e.seq).toBe("number");
    expect(statuses[1]!.seq).toBeGreaterThan(e.seq);
    await expect(api.agents.start(a.id)).rejects.toMatchObject({ code: "already_running", status: 409 });
    s.close();
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
    await expect(api.targets.action("pg-test", "up")).rejects.toMatchObject({ status: 404 });
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
    expect(c.cards.removed.sort()).toEqual(["TER-12", "TER-16"]);
  });
});

import { describe, expect, it } from "vitest";
import { ApiError } from "../http";
import { ptyInput, ptyResize } from "../sockets";
import { createMockApi } from ".";
import { FakePty } from "./pty";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0 });
const tick = () => new Promise((r) => setTimeout(r, 0));

describe("mock API", () => {
  it("serves every project's board with the default columns", async () => {
    const api = mock();
    const board = await api.board.get("terrarium");
    expect(board.columns.map((c) => c.name)).toEqual(["backlog", "ready", "in_progress", "review", "blocked", "done"]);
    expect(board.cards.length).toBeGreaterThan(5);
  });

  it("enforces the board rules from §13", async () => {
    const api = mock();
    // TER-14 depends on TER-12, which is not done.
    const err = await api.cards.update("TER-14", { column: "in_progress" }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).code).toBe("dependencies_unmet");
    expect((err as ApiError).details).toEqual({ blocked_by: ["TER-12"] });
    expect((err as ApiError).message).toMatch(/depends on TER-12/);
    // Blocked needs a comment.
    await expect(api.cards.update("TER-13", { column: "blocked" })).rejects.toBeInstanceOf(ApiError);
    const moved = await api.cards.update("TER-13", { column: "blocked", comment: "need a login" });
    expect(moved.column).toBe("blocked");
    expect(moved.comments.at(-1)?.body).toBe("need a login");
  });

  it("refuses a move past the WIP limit", async () => {
    const api = mock();
    await api.cards.update("TER-17", { column: "in_progress" });
    await api.cards.update("TER-18", { column: "in_progress" });
    const err = await api.cards.update("TER-13", { column: "in_progress" }).catch((e: unknown) => e);
    expect((err as ApiError).code).toBe("wip_limit");
    expect((err as ApiError).details).toEqual({ column: "in_progress", wip_limit: 3 });
  });

  it("edits the columns with the rules of docs/api.md §8.1", async () => {
    const api = mock();
    const b = await api.board.get("terrarium");
    expect(b.columns.find((c) => c.name === "in_progress")).toMatchObject({ role: "working", title: "in progress" });
    const bad = async (columns: Parameters<typeof api.board.update>[1]) => {
      const err = await api.board.update("terrarium", columns).catch((e: unknown) => e);
      expect(err).toBeInstanceOf(ApiError);
      return err as ApiError;
    };
    const keep = b.columns.map((c) => ({ name: c.name }));
    expect((await bad(keep.slice(1))).status).toBe(422);
    expect((await bad([{ name: "backlog", wip_limit: 2 }, ...keep.slice(1)])).message).toMatch(/no WIP/);
    expect((await bad(keep.map((c) => (c.name === "in_progress" ? { ...c, role: null } : c)))).message).toMatch(/exactly one/);
    expect((await bad(keep.map((c) => (c.name === "review" ? { ...c, role: "blocked" as const } : c)))).message).toMatch(/at most one/);
    // A column with cards cannot go: the error lists them.
    const busy = await bad(keep.filter((c) => c.name !== "review"));
    expect(busy.status).toBe(409);
    expect((busy.details as { cards: string[] }).cards.length).toBeGreaterThan(0);
    // Rename in_progress to doing; claim-like moves follow the role; a limit under the count is allowed.
    const next = await api.board.update(
      "terrarium",
      keep.map((c) => (c.name === "in_progress" ? { name: "doing", rename_from: "in_progress", title: "Doing", wip_limit: 1 } : c.name === "blocked" ? { name: "stuck", rename_from: "blocked" } : c)),
    );
    const doing = next.columns.find((c) => c.name === "doing")!;
    expect(doing).toMatchObject({ role: "working", title: "Doing", wip_limit: 1 });
    expect(doing.count).toBeGreaterThanOrEqual(1);
    expect(next.columns.find((c) => c.name === "stuck")?.role).toBe("blocked");
    expect(next.cards.some((c) => c.column === "in_progress")).toBe(false);
    // The alias `blocked` names the role column, and it needs a comment.
    await expect(api.cards.update("TER-13", { column: "blocked" })).rejects.toBeInstanceOf(ApiError);
    expect((await api.cards.update("TER-13", { column: "blocked", comment: "a login" })).column).toBe("stuck");
    // A new card in a full column is refused.
    const full = await api.cards.create("terrarium", { title: "x", column: "working" }).catch((e: unknown) => e);
    expect((full as ApiError).code).toBe("wip_limit");
  });

  it("archives a version: no lane, not in the default list, history kept", async () => {
    const api = mock();
    const names = async (q?: Parameters<typeof api.versions.list>[1]) => (await api.versions.list("terrarium", q)).map((v) => v.name).sort();
    expect(await names()).not.toContain("spike-pty");
    expect(await names({ archived: "true" })).toEqual(["spike-pty"]);
    expect(await names({ archived: "all" })).toContain("spike-pty");
    const a = await api.versions.archive("terrarium/v0.1.0");
    expect(a).toMatchObject({ archived: true, status: "in_review" });
    expect(a.reviews.length).toBeGreaterThan(0);
    expect(await names()).not.toContain("v0.1.0");
    const u = await api.versions.unarchive(a.id);
    expect(u.archived).toBe(false);
    expect(u.archived_at).toBeUndefined();
    expect(await names()).toContain("v0.1.0");
    await expect(api.versions.list("terrarium", { archived: "maybe" as "all" })).rejects.toBeInstanceOf(ApiError);
  });

  it("emits events to subscribers when a card moves", async () => {
    const api = mock();
    const seen: string[] = [];
    const stream = api.events.stream({ since: api.world.seq });
    stream.subscribe((e) => seen.push(e.summary));
    await tick();
    await api.cards.update("TER-17", { column: "ready" });
    expect(seen).toContain("operator moved TER-17 to ready");
    stream.close();
  });

  it("sends an audit frame for an audited action, as the daemon does with ?audit=1", async () => {
    const api = mock();
    const rows: string[] = [];
    const stream = api.events.stream({ since: api.world.seq });
    stream.onAudit((a) => rows.push(`${a.action} ${a.target}`));
    await tick();
    const name = api.world.hosts[0]!.name;
    await api.hosts.revoke(name);
    expect(rows).toEqual([`host.revoke ${name}`]);
    stream.close();
  });

  it("returns copies, so callers cannot mutate the world", async () => {
    const api = mock();
    const agents = await api.agents.list();
    agents[0]!.name = "changed";
    expect((await api.agents.list())[0]!.name).not.toBe("changed");
  });

  it("completes a Claude login after the code is pasted", async () => {
    const api = mock();
    const before = (await api.subscriptions.list()).length;
    const s = await api.subscriptions.startLogin({ kind: "claude" });
    await tick();
    const waiting = await api.subscriptions.loginState(s.id);
    expect(waiting.state).toBe("waiting");
    expect(waiting.url).toMatch(/^https:/);
    await api.subscriptions.loginInput(s.id, "code-123");
    await tick();
    const done = await api.subscriptions.loginState(s.id);
    expect(done.state).toBe("done");
    expect((await api.subscriptions.list()).length).toBe(before + 1);
  });

  it("makes a second account on a second login with no name, and a re-login keeps the account", async () => {
    const api = mock();
    const login = async (name?: string) => {
      const s = await api.subscriptions.startLogin({ kind: "claude", ...(name ? { name } : {}) });
      await tick();
      await api.subscriptions.loginInput(s.id, "code-123");
      await tick();
      return (await api.subscriptions.loginState(s.id)).subscription;
    };
    expect(await login()).toBe("claude-pro");
    expect(await login()).toBe("claude-pro-2");
    expect(await login("claude-pro")).toBe("claude-pro");
    expect((await api.subscriptions.get("claude-pro-2")).account).toMatchObject({ harness: "claude", has_login: true, source: "login" });
  });

  it("returns the memory graph to depth 1", async () => {
    const api = mock();
    const g = await api.memories.graph("MEM-4", 1);
    expect(g.nodes.map((n) => n.id).sort()).toEqual(["MEM-1", "MEM-4", "MEM-5"]);
  });
});

describe("FakePty", () => {
  it("echoes input, answers a line, and records resize", async () => {
    const pty = new FakePty({ banner: ["hello"], prompt: "> ", onLine: (l) => [`got ${l}`] });
    const out: string[] = [];
    const dec = new TextDecoder();
    pty.addEventListener("message", (ev) => out.push(dec.decode(ev.data as ArrayBuffer)));
    await tick();
    expect(pty.readyState).toBe(FakePty.OPEN);
    pty.send(ptyResize(120, 40));
    expect(pty.size).toEqual({ cols: 120, rows: 40 });
    pty.send(ptyInput("ls\r"));
    const text = out.join("");
    expect(text).toContain("hello");
    expect(text).toContain("ls");
    expect(text).toContain("got ls");
    pty.close();
    expect(pty.readyState).toBe(FakePty.CLOSED);
  });

  it("ignores frames that are not protocol JSON", async () => {
    const pty = new FakePty();
    await tick();
    pty.send("raw keystrokes");
    expect(pty.received).toEqual(["raw keystrokes"]);
  });
});

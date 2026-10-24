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
    await expect(api.cards.update("TER-14", { column: "in_progress" })).rejects.toThrow(/depends on TER-12/);
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
    await expect(api.cards.update("TER-13", { column: "in_progress" })).rejects.toThrow(/WIP limit/);
  });

  it("emits events to subscribers when a card moves", async () => {
    const api = mock();
    const seen: string[] = [];
    api.events().subscribe((e) => seen.push(e.summary));
    await api.cards.update("TER-17", { column: "ready" });
    expect(seen).toContain("operator moved TER-17 to ready");
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
    const s = await api.subscriptions.startLogin("claude");
    await tick();
    const waiting = await api.subscriptions.loginState(s.id);
    expect(waiting.state).toBe("waiting_for_user");
    expect(waiting.url).toMatch(/^https:/);
    await api.subscriptions.loginInput(s.id, "code-123");
    await tick();
    const done = await api.subscriptions.loginState(s.id);
    expect(done.state).toBe("done");
    expect((await api.subscriptions.list()).length).toBe(before + 1);
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

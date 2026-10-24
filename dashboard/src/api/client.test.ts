import { describe, expect, it } from "vitest";
import { createHttpApi } from "./client";
import type { RequestOptions, Transport } from "./http";

function recorder() {
  const calls: { method: string; path: string; opts?: RequestOptions }[] = [];
  const sockets: string[] = [];
  const t: Transport = {
    async request<T>(method: string, path: string, opts?: RequestOptions) {
      calls.push({ method, path, opts });
      return undefined as T;
    },
    async page<T>(path: string, query?: Record<string, unknown>) {
      calls.push({ method: "GET", path, opts: { query: query as RequestOptions["query"] } });
      return { items: [] as T[], next: null };
    },
    wsUrl: (path, query) => `ws://test${path}${query?.since !== undefined ? `?since=${query.since}` : ""}`,
  };
  const fakeSocket = (url: string) => {
    sockets.push(url);
    return { binaryType: "blob", readyState: 0, addEventListener() {}, removeEventListener() {}, send() {}, close() {} } as unknown as WebSocket;
  };
  return { api: createHttpApi(t, fakeSocket), calls, sockets };
}

describe("createHttpApi", () => {
  it("maps calls onto the §17 routes", async () => {
    const { api, calls } = recorder();
    await api.health();
    await api.agents.list("terrarium");
    await api.agents.restart("01JB");
    await api.board.get("my project");
    await api.cards.update("TER-42", { column: "review", position: 0 });
    await api.memories.graph("MEM-1");
    await api.wiki.diff("PG-1", 2, 3);
    await api.versions.transition("VER-1", "reject", "tests fail");
    await api.subscriptions.loginInput("LOGIN-1", "abc");
    await api.inbox.list("webshop");
    await api.targets.action("TGT-1", "snapshot");

    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual([
      "GET /api/health",
      "GET /api/agents",
      "POST /api/agents/01JB/restart",
      "GET /api/projects/my%20project/board",
      "PATCH /api/cards/TER-42",
      "GET /api/memories/MEM-1/graph",
      "GET /api/wiki/PG-1/diff",
      "POST /api/versions/VER-1/reject",
      "POST /api/subscriptions/login/LOGIN-1/input",
      "GET /api/projects/webshop/inbox",
      "POST /api/targets/TGT-1/snapshot",
    ]);
    expect(calls[1]!.opts?.query).toMatchObject({ project: "terrarium", limit: 1000 });
    expect(calls[4]!.opts?.body).toEqual({ column: "review", position: 0 });
    expect(calls[5]!.opts?.query).toEqual({ depth: 1 });
    expect(calls[6]!.opts?.query).toEqual({ a: 2, b: 3 });
    expect(calls[7]!.opts?.body).toEqual({ comment: "tests fail" });
  });

  it("opens PTY sockets on the right paths with arraybuffer frames", () => {
    const { api, sockets } = recorder();
    const s = api.agents.pty("01JB");
    api.subscriptions.loginPty("LOGIN-9");
    expect(sockets).toEqual(["ws://test/api/agents/01JB/pty", "ws://test/api/subscriptions/login/LOGIN-9/pty"]);
    expect(s.binaryType).toBe("arraybuffer");
  });
});

describe("createHttpApi, docs/api.md specifics", () => {
  it("encodes a project/name agent key as one segment", async () => {
    const { api, calls } = recorder();
    await api.agents.get("terrarium/worker");
    await api.agents.roles("terrarium/worker");
    expect(calls.map((c) => c.path)).toEqual(["/api/agents/terrarium%2Fworker", "/api/agents/terrarium%2Fworker/roles"]);
  });

  it("sends confirm=true on project delete and on a confirmed agent delete", async () => {
    const { api, calls } = recorder();
    await api.projects.remove("ops");
    await api.agents.remove("A1", true);
    await api.agents.remove("A2");
    expect(calls.map((c) => c.opts?.query)).toEqual([{ confirm: true }, { confirm: true }, undefined]);
  });

  it("uses the inbox reply and read routes", async () => {
    const { api, calls } = recorder();
    await api.inbox.reply("ops", "M1", "ok");
    await api.inbox.markRead("ops", "M1");
    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual(["POST /api/projects/ops/inbox/M1/reply", "POST /api/projects/ops/inbox/M1/read"]);
  });

  it("passes the cursor through page methods", async () => {
    const { api, calls } = recorder();
    await api.mail.list({ state: "unread", cursor: "01M" });
    await api.events.history({ since: 42, kind: "status" });
    expect(calls[0]).toMatchObject({ path: "/api/mail", opts: { query: { state: "unread", cursor: "01M" } } });
    expect(calls[1]).toMatchObject({ path: "/api/events", opts: { query: { since: 42, kind: "status" } } });
  });
});

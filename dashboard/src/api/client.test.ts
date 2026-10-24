import { describe, expect, it } from "vitest";
import { createHttpApi, type HttpApiOptions } from "./client";
import { AuthSocket } from "./sockets";
import type { RequestOptions, Transport } from "./http";

function recorder(opts: HttpApiOptions = {}, answer: unknown = undefined) {
  const calls: { method: string; path: string; opts?: RequestOptions }[] = [];
  const sockets: string[] = [];
  const raw: { url: string; protocols?: string[] }[] = [];
  const t: Transport = {
    async request<T>(method: string, path: string, opts?: RequestOptions) {
      calls.push({ method, path, opts });
      return answer as T;
    },
    async page<T>(path: string, query?: Record<string, unknown>) {
      calls.push({ method: "GET", path, opts: { query: query as RequestOptions["query"] } });
      return { items: [] as T[], next: null };
    },
    wsUrl: (path, query) => `ws://test${path}${query?.since !== undefined ? `?since=${query.since}` : ""}`,
    openSocket: (url) => {
      sockets.push(url);
      return new AuthSocket(url, {
        token: "tok",
        make: (u, protocols) => {
          raw.push({ url: u, protocols });
          return { binaryType: "blob", readyState: 0, protocol: "", addEventListener() {}, removeEventListener() {}, send() {}, close() {} };
        },
      });
    },
  };
  return { api: createHttpApi(t, opts), calls, sockets, raw };
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

  it("opens every socket with the bearer subprotocol, arraybuffer frames, and no token in the URL", () => {
    const { api, sockets, raw } = recorder();
    const s = api.agents.pty("01JB");
    api.subscriptions.loginPty("LOGIN-9");
    api.targets.sessionPty("box", "S1");
    api.events.stream({ since: 5 }).close();
    expect(sockets).toEqual([
      "ws://test/api/agents/01JB/pty",
      "ws://test/api/subscriptions/login/LOGIN-9/pty",
      "ws://test/api/targets/box/sessions/S1/ws",
      "ws://test/api/events?since=5",
    ]);
    expect(raw.map((r) => r.protocols)).toEqual([["bearer.tok"], ["bearer.tok"], ["bearer.tok"], ["bearer.tok"]]);
    expect(raw.every((r) => !r.url.includes("token"))).toBe(true);
    expect(s.binaryType).toBe("arraybuffer");
  });

  it("puts a signed artifact path on the artifact origin", async () => {
    const signed = { url: "/a/U1/r.txt?exp=1&sig=s", exp: 1, expires_at: "" };
    const { api } = recorder({ artifactOrigin: "https://art.example:8081" }, signed);
    expect((await api.artifacts.url("U1")).url).toBe("https://art.example:8081/a/U1/r.txt?exp=1&sig=s");
    const same = recorder({}, signed);
    expect((await same.api.artifacts.url("U1")).url).toBe("/a/U1/r.txt?exp=1&sig=s");
    const abs = recorder({ artifactOrigin: "https://art.example" }, { ...signed, url: "https://other.example/a/U1/r.txt" });
    expect((await abs.api.artifacts.url("U1")).url).toBe("https://other.example/a/U1/r.txt");
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

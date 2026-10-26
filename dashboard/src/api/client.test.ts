import { describe, expect, it, vi } from "vitest";
import { createHttpApi, type HttpApiOptions } from "./client";
import { AuthSocket } from "./sockets";
import { createHttpTransport, CSRF_HEADER, isMutating, type RequestOptions, type Transport } from "./http";
import { eachMethod, httpVerbs, NOT_TRANSPORT } from "./mock/verbs";

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
    async stream(path: string, query: Record<string, unknown> | undefined) {
      calls.push({ method: "SSE", path, opts: { query: query as RequestOptions["query"] } });
      return (async function* () {})();
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
  it("maps the transcript calls (docs/api.md §6): a page with after and limit, and the SSE stream with after", async () => {
    const { api, calls } = recorder();
    await api.agents.transcript("terrarium/lead", "01SES", { after: 734, limit: 500 });
    expect(calls.at(-1)).toMatchObject({ method: "GET", path: "/api/agents/terrarium%2Flead/sessions/01SES/transcript", opts: { query: { after: 734, limit: 500 } } });
    const s = api.agents.transcriptStream("terrarium/lead", "01SES", { after: 9 });
    expect(calls.at(-1)).toMatchObject({ method: "SSE", path: "/api/agents/terrarium%2Flead/sessions/01SES/transcript/stream", opts: { query: { after: 9 } } });
    s.close();
  });

  it("reads the transcript stream with fetch: the credentials of a request, and Accept text/event-stream", async () => {
    const seen: { url: string; init: RequestInit }[] = [];
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new TextEncoder().encode('event: status\ndata: {"status":"idle"}\n\n'));
        c.close();
      },
    });
    const t = createHttpTransport({
      getToken: () => "tok",
      fetch: async (url, init) => {
        seen.push({ url: String(url), init: init ?? {} });
        return new Response(body, { status: 200, headers: { "Content-Type": "text/event-stream" } });
      },
    });
    const chunks: string[] = [];
    for await (const c of await t.stream("/api/x/stream", { after: 3 }, new AbortController().signal)) chunks.push(c);
    expect(chunks.join("")).toContain("event: status");
    expect(seen[0]!.url).toBe("/api/x/stream?after=3");
    expect(seen[0]!.init.headers).toMatchObject({ Accept: "text/event-stream", Authorization: "Bearer tok" });
    expect(seen[0]!.init.credentials).toBe("same-origin");
    const refused = createHttpTransport({ getToken: () => null, fetch: async () => new Response('{"error":{"code":"not_found","message":"no such API route"}}', { status: 404 }) });
    await expect(refused.stream("/api/y", undefined, new AbortController().signal)).rejects.toMatchObject({ status: 404, code: "not_found" });
  });

  it("derives scope from project when the daemon sends no scope, and keeps a scope it sends", async () => {
    const rows = [{ name: "dbox" }, { name: "vm", project: "lab" }, { name: "vlab", project: null, scope: "global" }];
    const t = {
      async request<T>() {
        return { name: "new", project: "lab" } as T;
      },
      async page<T>() {
        return { items: rows as T[], next: null };
      },
    } as unknown as Transport;
    const api = createHttpApi(t);
    expect((await api.targets.list()).map((x) => `${x.name}:${x.scope}`)).toEqual(["dbox:global", "vm:project", "vlab:global"]);
    expect((await api.projects.targets("lab")).map((x) => x.scope)).toEqual(["global", "project", "global"]);
    expect((await api.targets.create({ name: "new", kind: "host", host: "h", project: "lab" })).scope).toBe("project");
  });

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

describe("cookie session on every client method", () => {
  /** The real transport with a session, over a fetch that records each request. */
  const sessionApi = (status = 200) => {
    const seen: { method: string; url: string; headers: Record<string, string> }[] = [];
    const f = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
      seen.push({ method: init!.method!, url: String(url), headers: init!.headers as Record<string, string> });
      return new Response("[]", { status });
    });
    const t = createHttpTransport({
      fetch: f,
      getToken: () => null,
      getCsrf: () => "csrf-1",
      location: { protocol: "http:", host: "d" },
      makeSocket: () => ({ readyState: 0, protocol: "", binaryType: "blob", send() {}, close() {}, addEventListener() {}, removeEventListener() {} }),
    });
    return { api: createHttpApi(t), seen };
  };

  it("sends X-CSRF-Token on every mutating method, and no token on any", async () => {
    const { api, seen } = sessionApi();
    const mutating: string[] = [];
    const pending: Promise<unknown>[] = [];
    eachMethod(api, (name, call) => {
      if (NOT_TRANSPORT.has(name)) return;
      const before = seen.length;
      const out = call();
      if (out instanceof Promise) pending.push(out.catch(() => undefined));
      else (out as { close?: () => void } | undefined)?.close?.();
      // The transport calls fetch before its first await.
      for (const r of seen.slice(before)) {
        // Only the sign-in itself carries the typed token.
        if (name === "session.create") expect(r.headers.Authorization).toBe("Bearer x");
        else expect(r.headers.Authorization, `${name} sent a token`).toBeUndefined();
        if (isMutating(r.method)) {
          mutating.push(name);
          expect(r.headers[CSRF_HEADER], `${name} (${r.method}) has no CSRF header`).toBe("csrf-1");
        } else {
          expect(r.headers[CSRF_HEADER], `${name} (GET) sent the CSRF header`).toBeUndefined();
        }
      }
    });
    await Promise.all(pending);
    // Every method the verb table calls mutating went through the check above.
    const want = [...httpVerbs()].filter(([, v]) => isMutating(v)).map(([k]) => k);
    expect(mutating.sort()).toEqual(want.sort());
    expect(want.length).toBeGreaterThan(50);
    expect(want).toEqual(expect.arrayContaining(["cards.create", "cards.update", "agents.setRoles", "wiki.save", "projects.remove", "session.remove"]));
  });

  it("opens every socket without a subprotocol with a session", () => {
    const made: (string[] | undefined)[] = [];
    const t = createHttpTransport({
      getToken: () => null,
      getCsrf: () => "c",
      location: { protocol: "http:", host: "d" },
      makeSocket: (_u, protocols) => {
        made.push(protocols);
        return { readyState: 0, protocol: "", binaryType: "blob", send() {}, close() {}, addEventListener() {}, removeEventListener() {} };
      },
    });
    const api = createHttpApi(t);
    api.agents.pty("A");
    api.subscriptions.loginPty("L");
    api.targets.sessionPty("box", "S");
    api.events.stream().close();
    expect(made).toEqual([undefined, undefined, undefined, undefined]);
  });
});

describe("session endpoints", () => {
  it("posts the typed token once to POST /api/session and answers the grant", async () => {
    const f = vi.fn(async (_u: string | URL | Request, _i?: RequestInit) => new Response(JSON.stringify({ csrf: "c", expires_at: "2026-10-25T00:00:00Z" }), { status: 201 }));
    const api = createHttpApi(createHttpTransport({ fetch: f, getToken: () => null }));
    expect(await api.session.create("typed")).toEqual({ csrf: "c", expires_at: "2026-10-25T00:00:00Z" });
    const [url, init] = f.mock.calls[0]!;
    expect(url).toBe("/api/session");
    expect(init!.method).toBe("POST");
    expect((init!.headers as Record<string, string>).Authorization).toBe("Bearer typed");
  });

  it("answers null when the daemon has no sessions (404 or 405)", async () => {
    for (const status of [404, 405]) {
      const f = vi.fn(async () => new Response(JSON.stringify({ error: { code: "not_found", message: "no such API route" } }), { status }));
      const api = createHttpApi(createHttpTransport({ fetch: f, getToken: () => null }));
      expect(await api.session.create("t")).toBeNull();
      await expect(api.session.remove()).resolves.toBeUndefined();
    }
  });

  it("a wrong token rejects, and does not call onUnauthorized", async () => {
    const onUnauthorized = vi.fn();
    const f = vi.fn(async () => new Response(JSON.stringify({ error: { code: "unauthorized", message: "no" } }), { status: 401 }));
    const api = createHttpApi(createHttpTransport({ fetch: f, getToken: () => null, onUnauthorized }));
    await expect(api.session.create("bad")).rejects.toMatchObject({ status: 401 });
    expect(onUnauthorized).not.toHaveBeenCalled();
  });

  it("sign-out sends DELETE /api/session with the CSRF header", async () => {
    const f = vi.fn(async (_u: string | URL | Request, _i?: RequestInit) => new Response(null, { status: 204 }));
    const api = createHttpApi(createHttpTransport({ fetch: f, getToken: () => null, getCsrf: () => "c9" }));
    await api.session.remove();
    const [url, init] = f.mock.calls[0]!;
    expect(`${init!.method} ${String(url)}`).toBe("DELETE /api/session");
    expect((init!.headers as Record<string, string>)[CSRF_HEADER]).toBe("c9");
  });
});


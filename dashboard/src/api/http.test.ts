import { describe, expect, it, vi } from "vitest";
import { ApiError, AuthError, buildQuery, createHttpTransport, CSRF_HEADER, isMutating } from "./http";
import type { MakeRawSocket } from "./sockets";
import { FakePty } from "./mock/pty";

function fakeFetch(status: number, body: unknown) {
  return vi.fn(async (_url: string | URL | Request, _init?: RequestInit) =>
    new Response(body === undefined ? "" : typeof body === "string" ? body : JSON.stringify(body), { status }),
  );
}

describe("buildQuery", () => {
  it("drops empty values and encodes the rest", () => {
    expect(buildQuery({ q: "a b", tag: undefined, all: true, n: 0, x: "" })).toBe("?q=a+b&all=true&n=0");
    expect(buildQuery({})).toBe("");
    expect(buildQuery()).toBe("");
  });
});

describe("createHttpTransport", () => {
  it("keeps a same-origin remote proxy prefix on HTTP, SSE, bytes, and sockets", async () => {
    const f = fakeFetch(200, []);
    const t = createHttpTransport({
      baseUrl: "/api/remotes/lab/proxy/", fetch: f, getToken: () => null,
      getCsrf: () => "home-csrf", location: { protocol: "https:", host: "home.test" },
    });
    await t.request("PATCH", "/api/agents/A1", { body: { model: "test" } });
    await t.page("/api/projects");
    await t.bytes("/api/repos/R1/file");
    await t.stream("/api/events", { since: 7 }, new AbortController().signal);
    expect(f.mock.calls.map(([u]) => u)).toEqual([
      "/api/remotes/lab/proxy/api/agents/A1", "/api/remotes/lab/proxy/api/projects",
      "/api/remotes/lab/proxy/api/repos/R1/file", "/api/remotes/lab/proxy/api/events?since=7",
    ]);
    expect(f.mock.calls[0]![1]?.headers).toMatchObject({ "X-CSRF-Token": "home-csrf" });
    expect(t.wsUrl("/api/events", { since: 7 })).toBe("wss://home.test/api/remotes/lab/proxy/api/events?since=7");
    expect(t.wsUrl("/api/agents/A1/pty")).toBe("wss://home.test/api/remotes/lab/proxy/api/agents/A1/pty");
  });

  it("sends the bearer token and a JSON body", async () => {
    const f = fakeFetch(200, { ok: 1 });
    const t = createHttpTransport({ fetch: f, getToken: () => "s3cret" });
    const res = await t.request<{ ok: number }>("POST", "/api/cards/TER-1/comments", { body: { body: "hi" } });
    expect(res).toEqual({ ok: 1 });
    const [url, init] = f.mock.calls[0]!;
    expect(url).toBe("/api/cards/TER-1/comments");
    expect(init?.method).toBe("POST");
    const headers = init?.headers as Record<string, string>;
    expect(headers.Authorization).toBe("Bearer s3cret");
    expect(headers["Content-Type"]).toBe("application/json");
    expect(init?.body).toBe(JSON.stringify({ body: "hi" }));
  });

  it("omits the Authorization header without a token", async () => {
    const f = fakeFetch(200, []);
    const t = createHttpTransport({ fetch: f, getToken: () => null });
    await t.request("GET", "/api/projects", { query: { q: "x" } });
    const [url, init] = f.mock.calls[0]!;
    expect(url).toBe("/api/projects?q=x");
    expect((init?.headers as Record<string, string>).Authorization).toBeUndefined();
  });

  it("calls onUnauthorized and throws AuthError on 401", async () => {
    const onUnauthorized = vi.fn();
    const t = createHttpTransport({ fetch: fakeFetch(401, { error: "bad token" }), getToken: () => "x", onUnauthorized });
    await expect(t.request("GET", "/api/agents")).rejects.toBeInstanceOf(AuthError);
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it("uses the daemon's error message for other failures", async () => {
    const t = createHttpTransport({ fetch: fakeFetch(409, { error: "Column in_progress is at its WIP limit (3)." }), getToken: () => "x" });
    const err = await t.request("PATCH", "/api/cards/TER-1", { body: {} }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(409);
    expect((err as ApiError).message).toMatch(/WIP limit/);
  });

  it("returns undefined for an empty 204 body", async () => {
    const t = createHttpTransport({ fetch: vi.fn(async () => new Response(null, { status: 204 })), getToken: () => null });
    await expect(t.request("DELETE", "/api/cards/TER-1")).resolves.toBeUndefined();
  });

  it("builds ws URLs without the token", () => {
    const t = createHttpTransport({ getToken: () => "tok", location: { protocol: "https:", host: "terra.local:8443" } });
    expect(t.wsUrl("/api/agents/A1/pty")).toBe("wss://terra.local:8443/api/agents/A1/pty");
    expect(t.wsUrl("/api/events", { since: 3 })).toBe("wss://terra.local:8443/api/events?since=3");
    const t2 = createHttpTransport({ baseUrl: "http://localhost:8080/", getToken: () => null });
    expect(t2.wsUrl("/api/events")).toBe("ws://localhost:8080/api/events");
  });

  it("opens sockets with the bearer subprotocol and the current token", () => {
    const made: { url: string; protocols?: string[] }[] = [];
    let token = "one";
    const t = createHttpTransport({
      getToken: () => token,
      location: { protocol: "http:", host: "d" },
      makeSocket: (url, protocols) => {
        made.push({ url, protocols });
        return { readyState: 0, protocol: "", binaryType: "blob", send() {}, close() {}, addEventListener() {}, removeEventListener() {} };
      },
    });
    t.openSocket(t.wsUrl("/api/events"));
    token = "two";
    t.openSocket(t.wsUrl("/api/events"));
    expect(made).toEqual([
      { url: "ws://d/api/events", protocols: ["bearer.one"] },
      { url: "ws://d/api/events", protocols: ["bearer.two"] },
    ]);
  });

  it("with a session: no Authorization, X-CSRF-Token on POST/PUT/PATCH/DELETE only, cookies on the same origin", async () => {
    const f = fakeFetch(200, {});
    const t = createHttpTransport({ fetch: f, getToken: () => null, getCsrf: () => "c1" });
    for (const m of ["GET", "POST", "PUT", "PATCH", "DELETE"]) await t.request(m, "/api/x");
    for (const [i, m] of ["GET", "POST", "PUT", "PATCH", "DELETE"].entries()) {
      const init = f.mock.calls[i]![1]!;
      const headers = init.headers as Record<string, string>;
      expect(headers.Authorization).toBeUndefined();
      expect(headers[CSRF_HEADER]).toBe(m === "GET" ? undefined : "c1");
      expect(init.credentials).toBe("same-origin");
    }
    expect(["post", "Put", "DELETE"].every(isMutating)).toBe(true);
    expect(["GET", "HEAD", "OPTIONS"].some(isMutating)).toBe(false);
  });

  it("sends the explicit bearer of a sign-in, and a quiet 401 does not call onUnauthorized", async () => {
    const onUnauthorized = vi.fn();
    const f = fakeFetch(401, { error: { code: "unauthorized", message: "missing or wrong bearer token" } });
    const t = createHttpTransport({ fetch: f, getToken: () => null, onUnauthorized });
    await expect(t.request("POST", "/api/session", { bearer: "typed", quiet401: true })).rejects.toBeInstanceOf(AuthError);
    expect((f.mock.calls[0]![1]!.headers as Record<string, string>).Authorization).toBe("Bearer typed");
    expect(onUnauthorized).not.toHaveBeenCalled();
  });
});

describe("WebSocket mode selection", () => {
  const record = () => {
    const made: { url: string; protocols?: string[] }[] = [];
    const make: MakeRawSocket = (url, protocols) => {
      made.push({ url, protocols });
      return { readyState: 0, protocol: "", binaryType: "blob", send() {}, close() {}, addEventListener() {}, removeEventListener() {} };
    };
    return { made, make };
  };

  it("a session opens sockets without a subprotocol (cookie auth)", () => {
    const { made, make } = record();
    const t = createHttpTransport({ getToken: () => null, getCsrf: () => "c1", location: { protocol: "http:", host: "d" }, makeSocket: make });
    const s = t.openSocket(t.wsUrl("/api/events"));
    expect(made).toEqual([{ url: "ws://d/api/events", protocols: undefined }]);
    expect(s.url).not.toContain("token");
    expect(s.readyState).toBe(0);
  });

  it("a cookie socket opens as mode `cookie` when the daemon echoes no subprotocol", async () => {
    const t = createHttpTransport({
      getToken: () => null,
      getCsrf: () => "c1",
      location: { protocol: "http:", host: "d" },
      makeSocket: () => new FakePty({ protocol: "" }),
    });
    const s = t.openSocket(t.wsUrl("/api/events"));
    await new Promise<void>((r) => s.addEventListener("open", () => r()));
    expect(s.mode).toBe("cookie");
    expect(s.failure).toBeNull();
  });

  it("bearer mode offers bearer.<token>", () => {
    const { made, make } = record();
    const t = createHttpTransport({ getToken: () => "T", getCsrf: () => null, location: { protocol: "http:", host: "d" }, makeSocket: make });
    t.openSocket(t.wsUrl("/api/events"));
    expect(made[0]!.protocols).toEqual(["bearer.T"]);
  });

  it("follows the auth state from one socket to the next", () => {
    const { made, make } = record();
    let token: string | null = "T";
    let csrf: string | null = null;
    const t = createHttpTransport({ getToken: () => token, getCsrf: () => csrf, location: { protocol: "http:", host: "d" }, makeSocket: make });
    t.openSocket(t.wsUrl("/api/events"));
    token = null;
    csrf = "c1";
    t.openSocket(t.wsUrl("/api/events"));
    expect(made.map((m) => m.protocols)).toEqual([["bearer.T"], undefined]);
  });
});


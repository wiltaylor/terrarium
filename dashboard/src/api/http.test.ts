import { describe, expect, it, vi } from "vitest";
import { ApiError, AuthError, buildQuery, createHttpTransport } from "./http";

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
});

// The system prompt library of the mock answers in the daemon's shapes
// (`api/system_prompts.rs`, docs/api.md §28), and the HTTP client sends the
// documented routes.
import { describe, expect, it } from "vitest";
import { createHttpTransport } from "../http";
import { createMockApi } from ".";

const mock = (o: Parameters<typeof createMockApi>[0] = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });

describe("system prompts (mock)", () => {
  it("lists the seed: the personas by role, then the others", async () => {
    const list = await mock().systemPrompts.list();
    expect(list.map((e) => e.name)).toEqual(["leader", "reviewer", "worker"]);
    expect(list.find((e) => e.name === "worker")).toMatchObject({ builtin: true, source: "default", path: "system-prompts/worker.md" });
    expect(list.find((e) => e.name === "reviewer")).toMatchObject({ builtin: false, source: "config" });
  });

  it("creates, refuses a taken name unless replace, a bad name, an empty text, and a secret", async () => {
    const api = mock();
    const d = await api.systemPrompts.create({ name: "terse", text: "Be terse.", description: "Short" });
    expect(d).toMatchObject({ name: "terse", description: "Short", text: "Be terse.\n", users: [] });
    await expect(api.systemPrompts.create({ name: "terse", text: "x" })).rejects.toMatchObject({ status: 409 });
    expect(await api.systemPrompts.create({ name: "terse", text: "Again.", replace: true })).toMatchObject({ text: "Again.\n" });
    for (const body of [{ name: "Bad", text: "x" }, { name: "none", text: "x" }, { name: "e", text: " " }, { name: "leak", text: `ghp_${"a1B2c3D4e5".repeat(4)}` }]) {
      await expect(api.systemPrompts.create(body)).rejects.toMatchObject({ status: 422, code: "invalid" });
    }
    expect((await api.systemPrompts.list()).map((e) => e.name)).not.toContain("leak");
    const up = await api.systemPrompts.update("terse", { description: "" });
    expect(up).toMatchObject({ description: "", text: "Again.\n" });
  });

  it("lists the users, refuses a delete in use with details.users, and deletes with force", async () => {
    const api = mock();
    const [a] = await api.agents.list();
    await api.agents.update(a!.id, { persona: "reviewer" });
    const d = await api.systemPrompts.get("reviewer");
    expect(d.users).toEqual([expect.objectContaining({ kind: "agent", id: a!.id, name: `${a!.project}/${a!.name}` })]);
    const e = await api.systemPrompts.remove("reviewer").catch((x: unknown) => x);
    expect(e).toMatchObject({ status: 409, code: "conflict" });
    expect((e as { details: { users: unknown[] } }).details.users).toHaveLength(1);
    await api.systemPrompts.remove("reviewer", { force: true });
    await expect(api.systemPrompts.get("reviewer")).rejects.toMatchObject({ status: 404 });
    // leader and worker never go.
    await expect(api.systemPrompts.remove("worker", { force: true })).rejects.toMatchObject({ status: 409 });
  });

  it("follows the rights: a member reads nothing; a project_admin adds, but does not change leader", async () => {
    await expect(mock({ scheduleCaller: { role: "member", project: "terrarium" } }).systemPrompts.list()).rejects.toMatchObject({ status: 403 });
    const lead = mock({ scheduleCaller: { role: "project_admin", project: "terrarium" } });
    await lead.systemPrompts.create({ name: "mine", text: "Mine." });
    await expect(lead.systemPrompts.update("leader", { text: "x" })).rejects.toMatchObject({ status: 403 });
    await expect(mock({ configRepo: false }).systemPrompts.list()).rejects.toMatchObject({ status: 501 });
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({
      baseUrl: "http://d",
      getToken: () => null,
      fetch: (async (url: string, init: RequestInit) => {
        calls.push(`${init.method} ${String(url).replace("http://d", "")}${init.body ? ` ${String(init.body)}` : ""}`);
        return new Response(init.method === "DELETE" ? null : "{}", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
      }) as unknown as typeof fetch,
    });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.systemPrompts.list();
    await api.systemPrompts.get("terse");
    await api.systemPrompts.create({ name: "a", text: "t", description: "d" });
    await api.systemPrompts.update("a", { text: "u" });
    await api.systemPrompts.remove("a");
    await api.systemPrompts.remove("a", { force: true });
    expect(calls).toEqual([
      "GET /api/system-prompts",
      "GET /api/system-prompts/terse",
      'POST /api/system-prompts {"name":"a","text":"t","description":"d"}',
      'PUT /api/system-prompts/a {"text":"u"}',
      "DELETE /api/system-prompts/a",
      "DELETE /api/system-prompts/a?force=true",
    ]);
  });
});

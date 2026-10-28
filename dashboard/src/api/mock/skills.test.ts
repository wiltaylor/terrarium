// The skills of the mock answer in the daemon's shapes (`api/skills.rs`),
// and the HTTP client sends the documented routes.
import { describe, expect, it } from "vitest";
import { createHttpTransport } from "../http";
import { createMockApi } from ".";
import { checkSkill, newSkillMd } from "./skills";
import { waitFor } from "../../test/wait";

const mock = (o: Parameters<typeof createMockApi>[0] = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });

describe("skills (mock)", () => {
  it("lists the seed: fern-check, and a skill that fails the checks", async () => {
    const api = mock();
    const list = await api.skills.list();
    expect(list.map((s) => s.name)).toEqual(["broken-skill", "fern-check", "release-notes"]);
    const fern = list.find((s) => s.name === "fern-check")!;
    expect(fern).toMatchObject({ description: "When asked for the deployment checklist code, reply with FERN-42", files: 3 });
    expect(fern).not.toHaveProperty("errors");
    expect(fern.bytes).toBeGreaterThan(0);
    expect(list.find((s) => s.name === "broken-skill")!.errors!.length).toBeGreaterThan(0);
  });

  it("answers a skill with skill_md and file_list; text only for a UTF-8 file", async () => {
    const d = await mock().skills.get("fern-check");
    expect(d.skill_md).toMatch(/^---\nname: fern-check\n/);
    expect(d.file_list.map((f) => f.path)).toEqual(["assets/fern.png", "reference/checklist.md"]);
    expect(d.file_list[0]).not.toHaveProperty("text");
    expect(d.file_list[1]!.text).toMatch(/Checklist/);
    await expect(mock().skills.get("nope")).rejects.toMatchObject({ status: 404 });
  });

  it("creates from a description, refuses a taken name and failed checks with details.problems", async () => {
    const api = mock();
    const d = await api.skills.create({ name: "new-one", description: "Use it to test." });
    expect(d.skill_md).toBe(newSkillMd("new-one", "Use it to test."));
    expect(d.files).toBe(1);
    await expect(api.skills.create({ name: "new-one", description: "x" })).rejects.toMatchObject({ status: 409 });
    const bad = await api.skills.create({ name: "Bad_Name", description: "" }).catch((e: unknown) => e);
    expect(bad).toMatchObject({ status: 422, code: "invalid" });
    expect((bad as { details: { problems: string[] } }).details.problems).toEqual([expect.stringMatching(/Bad_Name/), expect.stringMatching(/description/)]);
    await expect(api.skills.create({ name: "x", description: "d", files: { "a.exe": "MZ" } })).rejects.toMatchObject({ status: 422, details: { problems: [expect.stringMatching(/a\.exe/)] } });
    await expect(api.skills.create({ name: "y", skill_md: `${newSkillMd("y", "d")}sk-ant-api03-abcdefghijkl\n` })).rejects.toMatchObject({ status: 422 });
  });

  it("updates SKILL.md; files absent keeps them, files present replaces them; delete answers 404 after", async () => {
    const api = mock();
    const before = await api.skills.get("fern-check");
    const md = before.skill_md.replace("FERN-42", "FERN-43");
    const u = await api.skills.update("fern-check", { skill_md: md });
    expect(u.skill_md).toBe(md);
    expect(u.file_list).toEqual(before.file_list);
    const r = await api.skills.update("fern-check", { files: { "notes.txt": "hi", "img.png": { base64: "iVBORw0KGgo=" } } });
    expect(r.file_list.map((f) => [f.path, f.size, f.text])).toEqual([
      ["img.png", 8, undefined],
      ["notes.txt", 2, "hi"],
    ]);
    await expect(api.skills.update("fern-check", { skill_md: "no front matter" })).rejects.toMatchObject({ status: 422 });
    await api.skills.remove("fern-check");
    await expect(api.skills.get("fern-check")).rejects.toMatchObject({ status: 404 });
    await expect(api.skills.remove("fern-check")).rejects.toMatchObject({ status: 404 });
  });

  it("answers the usage: agents by project/agent and templates, with all for *", async () => {
    const api = mock();
    const u = await api.skills.usage("fern-check");
    expect(u.agents).toEqual(expect.arrayContaining([expect.objectContaining({ name: "terrarium/runner-dev", all: false }), expect.objectContaining({ name: "webshop/shop-lead", all: true })]));
    await api.templates.create({ name: "sk", project: "terrarium", spec: { harness: "pi", skills: ["fern-check"] } });
    expect((await api.skills.usage("fern-check")).templates).toEqual([expect.objectContaining({ name: "terrarium/sk", all: false })]);
    expect((await api.skills.usage("release-notes")).agents.map((a) => a.name)).toEqual(["webshop/shop-lead"]);
  });

  it("answers 501 not_available with no config repo, and 403 for writes by a member", async () => {
    const none = mock({ configRepo: false });
    await expect(none.skills.list()).rejects.toMatchObject({ status: 501, code: "not_available" });
    await expect(none.skills.create({ name: "a", description: "b" })).rejects.toMatchObject({ status: 501 });
    const member = mock({ scheduleCaller: { role: "member", project: "terrarium" } });
    expect(await member.skills.list()).toEqual([]);
    await expect(member.skills.create({ name: "a", description: "b" })).rejects.toMatchObject({ status: 403 });
  });

  it("takes skills on an agent: create, PATCH ([] clears), 422 for an unknown name, restart to apply on a live agent", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "sk1", project: "terrarium", harness: "claude", skills: ["fern-check"] });
    expect(a.skills).toEqual(["fern-check"]);
    await expect(api.agents.create({ name: "sk2", project: "terrarium", harness: "claude", skills: ["nope"] })).rejects.toMatchObject({ status: 422, details: { field: "skills" } });
    expect((await api.agents.update(a.id, { skills: ["*"] })).skills).toEqual(["*"]);
    // A stopped agent gets the list at its next start: no restart flag.
    expect((await api.agents.get(a.id)).skills_restart).toBeUndefined();
    const cleared = await api.agents.update(a.id, { skills: [] });
    expect(cleared).not.toHaveProperty("skills");
    await expect(api.agents.update(a.id, { skills: ["gone"] })).rejects.toMatchObject({ status: 422 });
    // A live agent: the flag stays until the next session.
    const live = await api.agents.get("terrarium/reviewer");
    expect((await api.agents.update(live.id, { skills: ["release-notes"] })).skills_restart).toBe(true);
    await api.agents.restart(live.id);
    await waitFor(async () => expect((await api.agents.get(live.id)).skills_restart).toBeUndefined());
  });

  it("keeps skills in a template spec, and an agent from the template gets them", async () => {
    const api = mock();
    const t = await api.templates.create({ name: "sk", project: "terrarium", spec: { harness: "pi", skills: ["fern-check"] } });
    expect(t.spec.skills).toEqual(["fern-check"]);
    expect((await api.templates.create({ name: "sk0", project: "terrarium", spec: { harness: "pi", skills: [] } })).spec).not.toHaveProperty("skills");
    expect((await api.agents.create({ name: "from-sk", project: "terrarium", template: "sk" })).skills).toEqual(["fern-check"]);
    expect((await api.agents.create({ name: "from-sk2", project: "terrarium", template: "sk", skills: [] })).skills).toBeUndefined();
    const lead = await api.agents.get("terrarium/runner-dev");
    expect((await api.templates.fromAgent(lead.id, { name: "rd" })).spec.skills).toEqual(["fern-check"]);
  });

  it("checks a skill as the daemon does", () => {
    const ok = { skill_md: newSkillMd("ok", "Fine."), files: new Map() };
    expect(checkSkill("ok", ok)).toEqual([]);
    expect(checkSkill("ok", { skill_md: newSkillMd("other", "Fine."), files: new Map() })).toEqual([expect.stringMatching(/folder name/)]);
    expect(checkSkill("ok", { skill_md: newSkillMd("ok", "a".repeat(1025)), files: new Map() })).toEqual([expect.stringMatching(/1024/)]);
    const many = new Map(Array.from({ length: 64 }, (_, i) => [`f${i}.md`, { text: "", size: 0 }]));
    expect(checkSkill("ok", { ...ok, files: many })).toEqual([expect.stringMatching(/65 files/)]);
    expect(checkSkill("ok", { ...ok, files: new Map([["big.txt", { size: 1024 * 1024 }]]) })).toEqual([expect.stringMatching(/1 MiB/)]);
    expect(checkSkill("ok", { ...ok, files: new Map([["../x.md", { text: "", size: 0 }]]) })).toEqual([expect.stringMatching(/not valid/)]);
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
    await api.skills.list();
    await api.skills.get("fern-check");
    await api.skills.create({ name: "a", description: "d" });
    await api.skills.update("a b", { skill_md: "x" });
    await api.skills.usage("a");
    await api.skills.remove("a");
    await api.agents.update("A1", { skills: [] });
    expect(calls).toEqual([
      "GET /api/skills",
      "GET /api/skills/fern-check",
      'POST /api/skills {"name":"a","description":"d"}',
      'PUT /api/skills/a%20b {"skill_md":"x"}',
      "GET /api/skills/a/usage",
      "DELETE /api/skills/a",
      'PATCH /api/agents/A1 {"skills":[]}',
    ]);
  });
});

// The mock must answer in the daemon's shapes (docs/api.md), so mock mode and
// real mode never diverge.
import { describe, expect, it } from "vitest";
import { ApiError, AuthError, createHttpTransport, isMutating } from "../http";
import { createMockApi, type MockOptions } from ".";
import type { AgentEvent } from "../types";
import { bearerProtocol, type AuthSocket, type MakeRawSocket, type PtySocket } from "../sockets";
import { FakePty } from "./pty";
import { acceptHandshake, MOCK_ORIGIN, MOCK_RULES, MOCK_TOKEN, type HandshakeRules } from "./ws";
import { eachMethod, httpVerbs, NOT_TRANSPORT } from "./verbs";

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

  it("names the tool call ID data.tool_id, not data.id (the event's own id)", async () => {
    const api = mock();
    const page = await api.events.history({ since: 0, limit: 1000 });
    const tools = page.items.filter((e) => e.kind === "tool_use" || e.kind === "tool_finished");
    expect(tools.length).toBeGreaterThan(0);
    for (const e of tools) {
      expect(typeof e.data.tool_id).toBe("string");
      expect(e.data).not.toHaveProperty("id");
    }
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

  // ----- docs/api.md §16: subscriptions, quota, usage, login sessions -----

  const SUB_KEYS = ["name", "provider", "harnesses", "alert_at", "source", "login_status", "probe_status", "created_at", "updated_at", "windows", "local_estimate_only", "has_api_key", "held_agents"];

  it("answers subscriptions in the §16.1 shape", async () => {
    const api = mock();
    const list = await api.subscriptions.list();
    expect(list.length).toBeGreaterThan(0);
    for (const s of list) {
      for (const k of SUB_KEYS) expect(s).toHaveProperty(k);
      expect(s).not.toHaveProperty("status");
      expect(s).not.toHaveProperty("history");
      expect(["logged_in", "expiring", "expired", "missing", "unknown", "api_key"]).toContain(s.login_status);
      expect(["ok", "failed", "none"]).toContain(s.probe_status);
      expect(["file", "api"]).toContain(s.source);
      expect(s.local_estimate_only).toBe(!s.probe);
      for (const w of s.windows) {
        expect(Object.keys(w).sort()).toEqual(["at", "resets_at", "used_pct", "window"]);
      }
    }
    const failed = list.find((s) => s.probe_status === "failed")!;
    expect(failed.probe_error).toBeTruthy();
    expect(failed.last_success_at).toBeTruthy();
    const samples = await api.subscriptions.quota("anthropic-max", { window: "5h" });
    expect(samples.length).toBeGreaterThan(1);
    expect(samples[0]).toMatchObject({ subscription: "anthropic-max", window: "5h" });
    expect(samples[0]!.at <= samples.at(-1)!.at).toBe(true);
    expect(samples[0]).toHaveProperty("raw");
  });

  it("creates and deletes subscriptions with the §16.3 errors", async () => {
    const api = mock();
    const s = await api.subscriptions.create({ name: "claude-team", provider: "anthropic", harnesses: ["claude"], probe: "anthropic-oauth-usage" });
    expect(s).toMatchObject({ name: "claude-team", source: "api", login_status: "unknown", probe_status: "none", alert_at: 80, windows: [], local_estimate_only: false, has_api_key: false, held_agents: 0 });
    await expect(api.subscriptions.create({ name: "claude-team", provider: "anthropic" })).rejects.toMatchObject({ status: 409, code: "conflict" });
    await expect(api.subscriptions.create({ name: "x1", provider: "anthropic", probe: "nope" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.create({ name: "login", provider: "anthropic" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.remove("anthropic-max")).rejects.toMatchObject({ status: 409, code: "file_managed" });
    await api.subscriptions.remove("claude-team");
    await expect(api.subscriptions.get("claude-team")).rejects.toMatchObject({ status: 404 });
  });

  it("answers a failed probe with 200 and probe_status failed, and a good probe with a quota.sample event", async () => {
    const api = mock();
    const fresh = await api.subscriptions.create({ name: "codex-new", provider: "openai", harnesses: ["codex"], probe: "openai-codex-usage" });
    const bad = await api.subscriptions.probe(fresh.name);
    expect(bad).toMatchObject({ subscription: "codex-new", probe_status: "failed", windows: [] });
    expect(typeof bad.error).toBe("string");
    expect((await api.subscriptions.get("codex-new")).probe_status).toBe("failed");
    const none = await api.subscriptions.probe("openrouter-key");
    expect(none).toEqual({ subscription: "openrouter-key", probe_status: "none", windows: [] });

    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    const ok = await api.subscriptions.probe("anthropic-max");
    expect(ok.probe_status).toBe("ok");
    expect(ok.windows[0]).toHaveProperty("raw");
    const e = frames.find((x) => x.kind === "quota.sample")!;
    expect(e.data).toMatchObject({ subscription: "anthropic-max" });
    expect((e.data.windows as unknown[]).length).toBe(2);
    st.close();
  });

  it("answers verify as {subscription, status} and logout as the subscription", async () => {
    const api = mock();
    const v = await api.subscriptions.verify("anthropic-max");
    expect(v.subscription.name).toBe("anthropic-max");
    expect(v.status).toMatchObject({ logged_in: true, harness: "claude", provider: "anthropic" });
    const key = await api.subscriptions.verify("openrouter-key");
    expect(key.status).toEqual({ logged_in: true, detail: "a stored API key" });
    expect((await api.subscriptions.logout("anthropic-max")).login_status).toBe("missing");
    await expect(api.subscriptions.logout("openrouter-key")).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("stores a Z.ai key as a claude+pi subscription with plan, api_key status, and no probe", async () => {
    const api = mock();
    const s = await api.subscriptions.setApiKey("glm", { provider: "zai", key: "zk-never-shown", plan: "api" });
    expect(s).toMatchObject({
      name: "glm", provider: "zai", plan: "api", harnesses: ["claude", "pi"], env_var: "ZAI_API_KEY", source: "api",
      login_status: "api_key", probe_status: "none", windows: [], local_estimate_only: true, has_api_key: true, held_agents: 0,
    });
    expect(s).not.toHaveProperty("probe");
    expect(JSON.stringify(s)).not.toContain("zk-never-shown");
    expect((await api.subscriptions.setApiKey("glm2", { provider: "zai", key: "k" })).plan).toBe("coding");
    await expect(api.subscriptions.setApiKey("glm3", { provider: "zai", key: "k", plan: "Max 20x" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.setApiKey("or2", { provider: "openrouter", key: "k", plan: "api" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.subscriptions.create({ name: "glm4", provider: "zai", plan: "pro" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    expect((await api.subscriptions.verify("glm")).subscription.login_status).toBe("api_key");
    await expect(api.subscriptions.logout("glm")).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("stores an API key write-only: no answer has the key", async () => {
    const api = mock();
    const secret = "sk-test-0123456789-secret";
    const s = await api.subscriptions.setApiKey("groq-key", { provider: "groq", key: secret });
    expect(s).toMatchObject({ name: "groq-key", source: "api", env_var: "GROQ_API_KEY", has_api_key: true, harnesses: ["pi"], local_estimate_only: true });
    expect(JSON.stringify(s)).not.toContain(secret);
    expect(JSON.stringify(await api.subscriptions.list())).not.toContain(secret);
    expect(JSON.stringify(await api.audit({ action: "subscription.api_key" }))).not.toContain(secret);
    expect((await api.subscriptions.setApiKey("gh-pat", { provider: "github", key: "github_pat_x" })).harnesses).toEqual(["copilot"]);
    await expect(api.subscriptions.setApiKey("compat", { provider: "openai-compatible", key: "k" })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.setApiKey("nope", { provider: "acme", key: "k" })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.setApiKey("groq-key", { provider: "groq", key: " " })).rejects.toMatchObject({ code: "invalid" });
  });

  it("detects logins with no subscription, with a suggested definition", async () => {
    const api = mock();
    const found = await api.subscriptions.detect();
    expect(found.length).toBe(1);
    const d = found[0]!;
    expect(d).toMatchObject({ kind: "pi", harness: "pi", provider: "openai-codex", pi_provider: "openai-codex", logged_in: true });
    expect(d.suggested).toMatchObject({ provider: "openai-codex", harnesses: ["pi"] });
    await api.subscriptions.create({ ...d.suggested, name: d.suggested.name! });
    expect(await api.subscriptions.detect()).toEqual([]);
  });

  it("emits subscription.offer as {login, suggested}", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    api.sim.offerLogins();
    const e = frames.find((x) => x.kind === "subscription.offer")!;
    expect(Object.keys(e.data).sort()).toEqual(["login", "suggested"]);
    expect(e.data.login).toMatchObject({ harness: "pi", provider: "openai-codex", logged_in: true });
    st.close();
  });

  it("runs a device-code login session: starting, waiting with url and device_code, a PTY that prints them, cancel", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0 });
    const s = await api.subscriptions.startLogin({ kind: "codex" });
    expect(s).toMatchObject({ kind: "codex", state: "starting", done: false });
    expect(Object.keys(s).sort()).toEqual(["done", "id", "instructions", "kind", "started_at", "state"]);
    await wait();
    const w = await api.subscriptions.loginState(s.id);
    expect(w).toMatchObject({ state: "waiting", url: "https://auth.openai.com/codex/device", done: false });
    expect(w.device_code).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4,5}$/);

    const pty = api.subscriptions.loginPty(s.id);
    const out: string[] = [];
    pty.addEventListener("message", (ev) => out.push(new TextDecoder().decode(ev.data as ArrayBuffer)));
    await wait();
    const screen = out.join("");
    expect(screen).toContain(w.url!);
    expect(screen).toContain(w.device_code!);
    pty.close();

    await api.subscriptions.cancelLogin(s.id);
    const c = await api.subscriptions.loginState(s.id);
    expect(c).toMatchObject({ state: "cancelled", done: true });
    expect(c.finished_at).toBeTruthy();
    await expect(api.subscriptions.cancelLogin(s.id)).rejects.toMatchObject({ status: 409, code: "not_running" });
    await expect(api.subscriptions.loginInput(s.id, "x")).rejects.toMatchObject({ status: 409, code: "not_running" });
    expect(api.world.audit.map((a) => a.action)).toEqual(expect.arrayContaining(["login.start", "login.cancel", "login.end"]));
  });

  it("finishes a paste-code login: verifying, done, the subscription made and probed, the code never stored", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0 });
    const s = await api.subscriptions.startLogin({ kind: "claude", name: "claude-home" });
    await wait();
    expect((await api.subscriptions.loginState(s.id)).url).toMatch(/^https:\/\/claude\.ai\/oauth/);
    await expect(api.subscriptions.loginInput(s.id, "a\nb")).rejects.toMatchObject({ code: "invalid" });
    await api.subscriptions.loginInput(s.id, "secret-code-123");
    expect((await api.subscriptions.loginState(s.id)).state).toBe("verifying");
    await wait();
    const done = await api.subscriptions.loginState(s.id);
    expect(done).toMatchObject({ state: "done", done: true, subscription: "claude-home" });
    const sub = await api.subscriptions.get("claude-home");
    expect(sub).toMatchObject({ login_status: "logged_in", probe_status: "ok", source: "api" });
    expect(JSON.stringify(api.world.audit)).not.toContain("secret-code-123");
    expect(api.world.audit.find((a) => a.action === "login.input")!.detail).toEqual({ length: 15 });
  });

  it("expires a login after its time, refuses a fifth live session, and needs a provider for pi", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0, loginTtlMs: 5 });
    const s = await api.subscriptions.startLogin({ kind: "copilot" });
    await wait(15);
    expect(await api.subscriptions.loginState(s.id)).toMatchObject({ state: "expired", done: true });
    await expect(api.subscriptions.startLogin({ kind: "pi" })).rejects.toMatchObject({ status: 422 });
    const api2 = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, autoLoginMs: 0 });
    for (let i = 0; i < 4; i++) await api2.subscriptions.startLogin({ kind: "codex" });
    await expect(api2.subscriptions.startLogin({ kind: "codex" })).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("answers GET /api/usage as points oldest first; group keys agents by ULID", async () => {
    const api = mock();
    const flat = await api.usage.series({ bucket: "hour" });
    expect(flat.length).toBeGreaterThan(0);
    expect(Object.keys(flat[0]!).sort()).toEqual(["bucket", "cache_read", "cache_write", "input_tokens", "messages", "output_tokens", "turns"]);
    expect(flat.every((p, i) => i === 0 || flat[i - 1]!.bucket <= p.bucket)).toBe(true);
    expect(flat[0]!.bucket).toMatch(/T\d\d:00:00Z$/);
    const byAgent = await api.usage.series({ project: "terrarium", group: "agent", bucket: "day", since: new Date(Date.now() - 3 * 86_400_000).toISOString() });
    expect(byAgent.every((p) => p.bucket.endsWith("T00:00:00Z"))).toBe(true);
    const lead = await api.agents.get("terrarium/lead");
    expect(byAgent.some((p) => p.key === lead.id)).toBe(true);
    expect(byAgent.every((p) => /^01/.test(p.key!))).toBe(true);
    const one = await api.usage.series({ agent: "terrarium/lead", group: "model" });
    expect(new Set(one.map((p) => p.key))).toEqual(new Set(["claude-opus-5", "claude-haiku-5-5"]));
    await expect(api.usage.series({ since: "yesterday" })).rejects.toMatchObject({ status: 400 });
  });

  it("answers group=card with the title, a card filter, and GET /api/cards/{id}/usage", async () => {
    const api = mock();
    const since = new Date(Date.now() - 7 * 86_400_000).toISOString();
    const byCard = await api.usage.series({ group: "card", bucket: "day", since });
    const withCard = byCard.filter((p) => p.key);
    expect(withCard.length).toBeGreaterThan(0);
    // Samples with no card have no key and no title, like the daemon's null.
    expect(byCard.some((p) => !("key" in p) && !("title" in p))).toBe(true);
    for (const p of withCard) {
      expect(p.key).toMatch(/^[A-Z]+-\d+$/);
      expect(typeof p.title).toBe("string");
    }
    const flat = await api.usage.series({ bucket: "hour" });
    expect(flat.every((p) => !("title" in p))).toBe(true);

    const id = withCard[0]!.key!;
    const filtered = await api.usage.series({ card: id.toLowerCase(), bucket: "day", since });
    const sum = (ps: { input_tokens: number; output_tokens: number }[]) => ps.reduce((a, p) => a + p.input_tokens + p.output_tokens, 0);
    expect(sum(filtered)).toBe(sum(byCard.filter((p) => p.key === id)));

    const u = await api.usage.card(id.toLowerCase());
    expect(Object.keys(u).sort()).toEqual(["agents", "card", "models", "project", "title", "total"]);
    expect(u.card).toBe(id);
    expect(Object.keys(u.total).sort()).toEqual(["cache_read", "cache_write", "input_tokens", "messages", "output_tokens", "turns"]);
    expect(Object.keys(u.models[0]!).sort()).toEqual(["cache_read", "cache_write", "input_tokens", "messages", "model", "output_tokens", "turns"]);
    expect(Object.keys(u.agents[0]!).sort()).toEqual(["cache_read", "cache_write", "id", "input_tokens", "messages", "name", "output_tokens", "turns"]);
    expect(u.agents[0]!.name).toMatch(/^[a-z-]+\/[a-z-]+$/);
    const spend = (t: { input_tokens: number; output_tokens: number }) => t.input_tokens + t.output_tokens;
    expect(u.models.every((m, i) => i === 0 || spend(u.models[i - 1]!) >= spend(m))).toBe(true);
    expect(spend(u.total)).toBe(u.models.reduce((a, m) => a + spend(m), 0));
    expect(spend(u.total)).toBe(u.agents.reduce((a, m) => a + spend(m), 0));
    const later = await api.usage.card(id, { since: "2999-01-01T00:00:00Z" });
    expect(later).toMatchObject({ since: "2999-01-01T00:00:00.000Z", models: [], agents: [], total: { messages: 0 } });
    await expect(api.usage.card("NOPE-1")).rejects.toMatchObject({ status: 404 });
    await expect(api.usage.card(id, { since: "yesterday" })).rejects.toMatchObject({ status: 400 });
    await expect(api.usage.series({ group: "board" as never })).rejects.toMatchObject({ status: 400 });
  });

  it("answers GET /api/usage/me with the subscription, today, and holds", async () => {
    const api = mock();
    const me = await api.usage.me("terrarium/lead");
    expect(me.agent).toMatchObject({ name: "lead", project: "terrarium" });
    expect(me.subscription).toMatchObject({ name: "anthropic-max", probe_status: "ok", local_estimate_only: false });
    expect(Object.keys(me.today.mine).sort()).toEqual(["cache_read", "cache_write", "input_tokens", "messages", "output_tokens", "turns"]);
    expect(me.holds).toEqual([]);
  });

  it("runs the quota policy: quota.alert, quota.blocked per agent, held_agents, then quota.resumed", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const st = api.events.stream({ since: api.world.seq });
    st.subscribe((e) => frames.push(e));
    await wait();
    api.sim.setWindow("anthropic-max", "5h", 85);
    const alert = frames.find((x) => x.kind === "quota.alert")!;
    expect(Object.keys(alert.data).sort()).toEqual(["alert_at", "level", "resets_at", "subscription", "used_pct", "window"]);
    expect(alert.data).toMatchObject({ subscription: "anthropic-max", window: "5h", used_pct: 85, level: "alert", alert_at: 80 });
    api.sim.setWindow("anthropic-max", "5h", 85);
    expect(frames.filter((x) => x.kind === "quota.alert")).toHaveLength(1); // once per window period

    api.sim.setWindow("anthropic-max", "5h", 100);
    expect(frames.filter((x) => x.kind === "quota.alert").at(-1)!.data.level).toBe("exhausted");
    const blocked = frames.filter((x) => x.kind === "quota.blocked");
    const onPlan = (await api.agents.list()).filter((a) => a.subscription === "anthropic-max");
    expect(blocked.map((e) => e.agent_id).sort()).toEqual(onPlan.map((a) => a.id).sort());
    expect(Object.keys(blocked[0]!.data).sort()).toEqual(["agent_id", "cause", "enforced", "reason", "until"]);
    expect(blocked[0]!.data).toMatchObject({ cause: "quota:anthropic-max", enforced: false });
    expect((await api.subscriptions.get("anthropic-max")).held_agents).toBe(onPlan.length);
    expect((await api.usage.me("terrarium/lead")).holds[0]).toMatchObject({ cause: "quota:anthropic-max", enforced: false });
    expect(api.world.mail.some((m) => m.to === "operator" && m.subject.includes("anthropic-max"))).toBe(true);

    api.sim.setWindow("anthropic-max", "5h", 20);
    const resumed = frames.filter((x) => x.kind === "quota.resumed");
    expect(resumed).toHaveLength(onPlan.length);
    expect(resumed[0]!.data).toMatchObject({ cause: "quota:anthropic-max", holds_left: 0 });
    expect((await api.subscriptions.get("anthropic-max")).held_agents).toBe(0);
    st.close();
  });

  it("stores self_report events as {status, note}, apart from the runner status", async () => {
    const api = mock();
    const page = await api.events.history({ kind: "self_report" });
    const e = page.items[0]!;
    expect(e).toMatchObject({ kind: "self_report", agent: "adapter-codex", project: "terrarium" });
    expect(e.data).toMatchObject({ status: "blocked", note: expect.any(String) });
    expect(e.actor).toMatch(/^agent:/);
  });

  it("answers GET /api/events/latest with the newest seq", async () => {
    const api = mock();
    expect(await api.events.latest()).toBe(api.world.seq);
  });

  // ----- PLAN §19 hosts and targets: mock-only until the daemon has them -----

  it("shows a host token once and revokes it", async () => {
    const api = mock();
    const made = await api.hosts.create("lab2");
    expect(made.token).toMatch(/^thk_/);
    expect(made).toMatchObject({ name: "lab2", status: "offline", online: false, sessions: 0, capabilities: {}, last_seen: null });
    expect(JSON.stringify(await api.hosts.list())).not.toContain(made.token);
    await expect(api.hosts.create("lab2")).rejects.toMatchObject({ code: "conflict" });
    await api.hosts.revoke("lab2");
    expect((await api.hosts.list()).map((h) => h.name)).not.toContain("lab2");
    const lab1 = (await api.hosts.list()).find((h) => h.name === "lab1")!;
    expect(lab1.capabilities).toMatchObject({ docker: { present: true, version: "29.8.0" }, vmlab: { present: true } });
  });

  it("answers GET /api/harnesses?latest=true and the harness update in the daemon's shapes", async () => {
    const api = mock();
    // The fields of `harness_versions::HarnessVersion` (docs/api.md GET /api/harnesses).
    const ROW = ["harness", "package", "pinned", "tested", "installed", "in_range", "error", "checked_at", "latest", "update", "latest_error"];
    const r = await api.harnesses(true, true);
    for (const h of r.harnesses) {
      for (const k of Object.keys(h)) expect(ROW).toContain(k);
      expect(h).toHaveProperty("latest");
      expect(h).toHaveProperty("update");
      expect(["none", "minor", "major", null]).toContain(h.update);
      if (h.latest === null) expect(typeof h.latest_error).toBe("string");
      else expect(h.latest!.published_at === null || !Number.isNaN(Date.parse(h.latest!.published_at))).toBe(true);
    }
    // Without `latest`, no npm fields.
    for (const h of (await api.harnesses()).harnesses) for (const k of ["latest", "update", "latest_error"]) expect(h).not.toHaveProperty(k);
    // `harness_install::UpdateResult` (docs/api.md POST /api/harnesses/{name}/update).
    const RESULT = ["harness", "package", "from", "to", "output", "tested", "in_range"].sort();
    const one = await api.harnessUpdate("copilot-prompt", { force: true });
    expect(Object.keys(one).sort()).toEqual(RESULT);
    expect(one).toMatchObject({ harness: "copilot-prompt", package: "copilot-custom-system-prompt", tested: null, in_range: null });
    const conflict = await api.harnessUpdate("claude").catch((e: unknown) => e);
    expect(conflict).toMatchObject({ status: 409, code: "conflict", details: { running: expect.arrayContaining(["terrarium/lead"]) } });
    const npm = await api.harnessUpdate("claude", { version: "99.1.0", force: true }).catch((e: unknown) => e);
    expect(npm).toMatchObject({ status: 422, code: "invalid", details: { package: "@anthropic-ai/claude-code", version: "99.1.0", from: "2.1.310" } });
    expect(Object.keys((npm as { details: object }).details).sort()).toEqual(["from", "output", "package", "version"]);
  });

  it("runs the target lifecycle with the daemon's states, and answers each verb with the executor report", async () => {
    const api = mock();
    const state = async (name: string) => (await api.targets.list()).find((x) => x.name === name)!;
    const STATES = ["running", "stopped", "absent", "unknown"];
    for (const x of await api.targets.list()) expect(STATES).toContain(x.state);
    // lab-win11: a kept ephemeral vmlab target, stopped.
    expect(await api.targets.action("lab-win11", "up")).toEqual({ target: "lab-win11", verb: "up", state: "running" });
    expect((await state("lab-win11")).state).toBe("running");
    expect(await api.targets.action("lab-win11", "snapshot")).toEqual({ target: "lab-win11", verb: "snapshot", state: "running", snapshot: "terrarium", scope: "machine" });
    expect((await state("lab-win11")).snapshots).toEqual(["terrarium-clean", "terrarium"]);
    expect(await api.targets.action("lab-win11", "restore")).toMatchObject({ verb: "restore", state: "running", snapshot: "terrarium" });
    expect(await api.targets.action("lab-win11", "down")).toMatchObject({ state: "stopped" });
    // pg-test: ephemeral docker without keep; down destroys it.
    expect(await api.targets.action("pg-test", "down")).toEqual({ target: "pg-test", verb: "down", state: "absent" });
    expect(await api.targets.action("pg-test", "status")).toEqual({ target: "pg-test", verb: "status", state: "absent" });
    expect((await api.audit({ action: "target.down" })).items.at(-1)).toMatchObject({ target: "pg-test", detail: { verb: "down", state: "absent" } });
    // host and ssh: up and down are no-ops; status follows the host agent link.
    expect(await api.targets.action("build-box", "up")).toMatchObject({ state: "unknown" });
    expect(await api.targets.action("pi-runner", "status")).toMatchObject({ state: "unknown" });
    await expect(api.targets.action("build-box", "snapshot")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.action("pg-test", "snapshot")).rejects.toMatchObject({ status: 422, code: "invalid" });
    // A permanent vmlab target stops; it is not destroyed. A restore needs the snapshot.
    await expect(api.targets.action("dev-vm", "down")).resolves.toMatchObject({ state: "stopped" });
    await expect(api.targets.action("ci-box", "up")).resolves.toMatchObject({ state: "running" });
    api.world.hostSnapshots["lab-win11"] = [];
    await expect(api.targets.action("lab-win11", "restore")).rejects.toMatchObject({ status: 422, code: "invalid", details: { verb: "vmlab.snapshot_restore" } });
    const opened = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    expect(opened).toMatchObject({ target: "pg-test", kind: "shell" });
    await api.targets.closeSession("pg-test", opened.session_id);
    await expect(api.targets.closeSession("pg-test", opened.session_id)).rejects.toMatchObject({ status: 404 });
  });

  it("lists and deletes snapshots in the daemon's shapes (docs/api.md §17.3)", async () => {
    const api = mock();
    const LIST = ["target", "scope", "machines", "snapshots", "dropped"];
    const ROW = ["name", "kind", "taken", "machines", "managed"].sort();
    // lab-win11: terrarium-clean is Terrarium's, pre-upgrade is not.
    const l = await api.targets.snapshots("lab-win11");
    expect(Object.keys(l).sort()).toEqual([...LIST].sort());
    expect(l).toMatchObject({ target: "lab-win11", scope: "machine", machines: ["win11"], dropped: [] });
    for (const r of l.snapshots) expect(Object.keys(r).sort()).toEqual(ROW);
    expect(l.snapshots.map((s) => [s.name, s.managed])).toEqual([["terrarium-clean", true], ["pre-upgrade", false]]);
    // The lab scope says whether the lab runs.
    expect(await api.targets.snapshots("lab-win11", { scope: "lab" })).toMatchObject({ scope: "lab", lab_running: true });
    // A take with a name and the lab scope answers the scope; the list shows it as managed.
    await api.targets.action("dev-vm", "snapshot", { name: "pre", scope: "lab" });
    expect((await api.targets.snapshots("dev-vm")).snapshots.find((s) => s.name === "pre")).toMatchObject({ managed: true, machines: ["vm1"] });
    await expect(api.targets.action("dev-vm", "snapshot", { name: "x", scope: "machine" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.action("dev-vm", "snapshot", { name: "-x" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    // A stored name that the host lost goes, with target.reconcile.
    api.world.hostSnapshots["dev-vm"] = api.world.hostSnapshots["dev-vm"]!.filter((r) => r.name !== "terrarium");
    expect((await api.targets.snapshots("dev-vm")).dropped).toEqual(["terrarium"]);
    expect((await api.targets.list()).find((t) => t.name === "dev-vm")!.snapshots).toEqual(["pre"]);
    expect((await api.audit({ action: "target.reconcile" })).items.at(-1)).toMatchObject({ target: "dev-vm", detail: { snapshots_dropped: ["terrarium"] } });
    // Delete: terrarium-clean with snapshot_before_task needs force.
    await expect(api.targets.deleteSnapshot("lab-win11", "terrarium-clean")).rejects.toMatchObject({ status: 409, code: "conflict" });
    const d = await api.targets.deleteSnapshot("lab-win11", "terrarium-clean", { force: true });
    expect(d).toEqual({ target: "lab-win11", verb: "snapshot_delete", snapshot: "terrarium-clean", scope: "machine", machines: ["win11"] });
    expect((await api.targets.list()).find((t) => t.name === "lab-win11")!.snapshots).toEqual([]);
    expect((await api.audit({ action: "target.snapshot_delete" })).items.at(-1)).toMatchObject({ target: "lab-win11", detail: { snapshot: "terrarium-clean" } });
    await api.targets.deleteSnapshot("lab-win11", "pre-upgrade");
    await expect(api.targets.deleteSnapshot("lab-win11", "nosuch")).rejects.toMatchObject({ status: 422, details: { verb: "vmlab.snapshot_delete" } });
    await expect(api.targets.deleteSnapshot("lab-win11", "nosuch", { scope: "lab" })).rejects.toMatchObject({ status: 404, code: "not_found" });
    // A docker target has none; a lab_from_workspace target needs the holder.
    await expect(api.targets.snapshots("pg-test")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.snapshots("terrarium-ws-lab")).rejects.toMatchObject({ status: 422, code: "invalid" });
    const ws = await api.targets.snapshots("terrarium-ws-lab", { agent: "terrarium/runner-dev" });
    expect(ws).toMatchObject({ agent: "terrarium/runner-dev", snapshots: [] });
    await api.targets.action("terrarium-ws-lab", "snapshot", { agent: "terrarium/runner-dev", name: "s1" });
    expect((await api.targets.snapshots("terrarium-ws-lab", { agent: "terrarium/runner-dev" })).snapshots.map((s) => s.name)).toEqual(["s1"]);
  });

  it("refuses a verb on a target whose host agent is offline with 409 conflict", async () => {
    const api = mock();
    api.world.targets.push({ id: "pi-box", name: "pi-box", kind: "docker", host: "pi-cluster", image: "alpine:3", scope: "global", state: "stopped", ephemeral: false, snapshots: [] });
    await expect(api.targets.action("pi-box", "up")).rejects.toMatchObject({ status: 409, code: "conflict" });
  });
});

describe("target scope (docs/api.md §17.2)", () => {
  it("puts scope on every target: global without a project, project with one", async () => {
    const api = mock();
    for (const t of await api.targets.list()) expect(t.scope).toBe(t.project ? "project" : "global");
  });

  it("lists the project's targets and the global ones on GET /api/projects/{name}/targets, in the target shape", async () => {
    const api = mock();
    const list = await api.projects.targets("terrarium");
    expect(list.map((t) => `${t.name}:${t.scope}`).sort()).toEqual(["build-box:global", "ci-box:global", "dev-vm:global", "lab-win11:project", "pi-runner:global", "terrarium-net-lab:project", "terrarium-ws-lab:project"]);
    expect(list.every((t) => !t.project || t.project === "terrarium")).toBe(true);
    expect(list[0]).toHaveProperty("access", "operator");
    expect(list[0]).toHaveProperty("host_online");
    await expect(api.projects.targets("nope")).rejects.toMatchObject({ status: 404 });
  });

  it("creates a project target and a global target with 201 shapes, source api", async () => {
    const api = mock();
    const p = await api.targets.create({ name: "pg2", kind: "docker", project: "webshop", host: "lab1", image: "postgres:17", ephemeral: true });
    expect(p).toMatchObject({ id: "webshop-pg2", name: "webshop-pg2", scope: "project", project: "webshop", source: "api", ephemeral: true, keep: false, snapshots: [], access: "operator", runtime: {} });
    const g = await api.targets.create({ name: "vm2", kind: "vmlab", host: "lab1", lab: "/labs/x/vmlab.wcl", snapshot_before_task: true });
    expect(g).toMatchObject({ scope: "global", snapshot_before_task: true });
    expect(g).not.toHaveProperty("project");
    expect(g.name).toBe("vm2");
    expect((await api.projects.targets("webshop")).map((t) => t.name)).toEqual(expect.arrayContaining(["webshop-pg2", "vm2"]));
    expect((await api.projects.targets("terrarium")).map((t) => t.name)).not.toContain("webshop-pg2");
    expect((await api.audit({ action: "target.create" })).items.at(-1)).toMatchObject({ target: "vm2" });
  });

  it("refuses a bad definition, a taken name, and an unknown project", async () => {
    const api = mock();
    await expect(api.targets.create({ name: "x", kind: "docker", host: "lab1" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.create({ name: "x", kind: "ssh", address: "-oProxy" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.create({ name: "x", kind: "docker", host: "lab1", container: "c", ephemeral: true })).rejects.toMatchObject({ status: 422 });
    await expect(api.targets.create({ name: "dev-vm", kind: "host", host: "lab1" })).rejects.toMatchObject({ status: 409, code: "conflict" });
    await expect(api.targets.create({ name: "x", kind: "host", host: "lab1", project: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("answers 403 forbidden for a global target when the caller is not a sysadmin, and still takes a project target", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, denyGlobalTargets: true });
    await expect(api.targets.create({ name: "g", kind: "host", host: "lab1" })).rejects.toMatchObject({ status: 403, code: "forbidden" });
    await expect(api.targets.create({ name: "p", kind: "host", host: "lab1", project: "terrarium" })).resolves.toMatchObject({ scope: "project" });
  });
});

describe("mode 1 targets (docs/api.md §17.2, §17.3, §17.7, §17.8)", () => {
  const runner = (api: ReturnType<typeof mock>) => api.world.agents.find((a) => a.name === "runner-dev")!;
  const lead = (api: ReturnType<typeof mock>) => api.world.agents.find((a) => a.name === "lead")!;

  it("shows runtime on every target, and the lab fields of each source", async () => {
    const api = mock();
    const list = await api.targets.list();
    for (const t of list) expect(t).toHaveProperty("runtime");
    const by = (n: string) => list.find((t) => t.name === n)!;
    expect(by("pg-test").runtime).toEqual({ attached: ["webshop/shop-lead"] });
    expect(by("terrarium-net-lab")).toMatchObject({ kind: "vmlab", lab_repo: "terrarium", lab_ref: "main", lab_path: "labs/net", machine: "router", runtime: {} });
    expect(by("terrarium-ws-lab")).toMatchObject({ lab_from_workspace: true, lab_path: "lab" });
    expect(by("terrarium-ws-lab").runtime!.instances![runner(api).id]).toMatchObject({ agent_name: "terrarium/runner-dev", state: "running", attached: ["terrarium/runner-dev"] });
    expect(by("terrarium-ws-lab").runtime!.instances![runner(api).id]!.lab).toMatch(/\/terrarium-ws-lab\.[^/]+\/lab$/);
  });

  it("validates the vmlab sources as the daemon does, with 422 invalid", async () => {
    const api = mock();
    const bad = [
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab: "/l", lab_repo: "terrarium", machine: "m" },
      { name: "x", kind: "vmlab" as const, host: "lab1", lab_repo: "terrarium", machine: "m" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_from_workspace: true },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_from_workspace: true, machine: "m", snapshot_before_task: true },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab: "/l", lab_path: "a" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_repo: "terrarium", machine: "m", lab_path: "../up" },
      { name: "x", kind: "vmlab" as const, host: "lab1", project: "terrarium", lab_repo: "terrarium", machine: "m", lab_ref: "-x" },
      { name: "x", kind: "docker" as const, host: "lab1", image: "a", project: "terrarium", lab_repo: "terrarium" },
    ];
    for (const b of bad) await expect(api.targets.create(b)).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("prefixes a project target made through the API, keeps a prefixed name, and keeps a global name", async () => {
    const api = mock();
    expect((await api.targets.create({ name: "box", kind: "host", host: "lab1", project: "terrarium" })).name).toBe("terrarium-box");
    expect((await api.targets.create({ name: "terrarium-box2", kind: "host", host: "lab1", project: "terrarium" })).name).toBe("terrarium-box2");
    expect((await api.targets.create({ name: "box", kind: "host", host: "lab1" })).name).toBe("box");
    await expect(api.targets.create({ name: "box", kind: "host", host: "lab1", project: "terrarium" })).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("refuses a target past TERRARIUM_MAX_TARGETS_PER_PROJECT with 409 conflict and {project, targets, max}", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, maxTargetsPerProject: 4 });
    // terrarium has lab-win11, terrarium-net-lab, terrarium-ws-lab.
    await api.targets.create({ name: "a", kind: "host", host: "lab1", project: "terrarium" });
    const e = await api.targets.create({ name: "b", kind: "host", host: "lab1", project: "terrarium" }).catch((x: unknown) => x as ApiError);
    expect(e).toMatchObject({ status: 409, code: "conflict", details: { project: "terrarium", targets: 4, max: 4 } });
    await expect(api.targets.create({ name: "g", kind: "host", host: "lab1" })).resolves.toMatchObject({ scope: "global" });
  });

  it("shows max_running_targets on a host, and refuses an up past it with 429 rate_limited", async () => {
    const api = mock();
    expect((await api.hosts.list()).find((h) => h.name === "lab1")).toHaveProperty("max_running_targets", 4);
    // lab1 runs pg-test, dev-vm, and one instance of terrarium-ws-lab: 3 of 4.
    await expect(api.targets.action("ci-box", "up")).resolves.toMatchObject({ state: "running" });
    const e = (await api.targets.action("lab-win11", "up").catch((x: unknown) => x)) as ApiError;
    expect(e).toMatchObject({ status: 429, code: "rate_limited" });
    expect(e.message).toBe("host `lab1` runs 4 targets, and its max_running_targets is 4; bring one down first");
    // An up of a target that runs already does not count against the cap.
    await expect(api.targets.action("ci-box", "up")).resolves.toMatchObject({ state: "running" });
    await api.targets.action("pg-test", "down");
    await expect(api.targets.action("lab-win11", "up")).resolves.toMatchObject({ state: "running" });
  });

  it("brings a lab_repo target up in a worktree: runtime.lab and runtime.head", async () => {
    const api = mock();
    await api.targets.action("terrarium-net-lab", "up");
    const t = (await api.targets.list()).find((x) => x.name === "terrarium-net-lab")!;
    expect(t.state).toBe("running");
    expect(t.runtime).toMatchObject({ lab: expect.stringMatching(/terrarium-net-lab\/lab$/), head: expect.stringMatching(/^[0-9a-f]{40}$/) });
    await api.targets.action("terrarium-net-lab", "down");
    expect((await api.targets.list()).find((x) => x.name === "terrarium-net-lab")!.runtime).toEqual({});
  });

  it("runs a verb on a lab_from_workspace target on one holder's instance, and needs agent", async () => {
    const api = mock();
    await expect(api.targets.action("terrarium-ws-lab", "up")).rejects.toMatchObject({ status: 422, code: "invalid" });
    const r = await api.targets.action("terrarium-ws-lab", "up", { agent: "terrarium/lead" });
    expect(r).toEqual({ target: "terrarium-ws-lab", verb: "up", state: "running" });
    const inst = (await api.targets.list()).find((x) => x.name === "terrarium-ws-lab")!.runtime!.instances!;
    expect(Object.keys(inst).sort()).toEqual([lead(api).id, runner(api).id].sort());
    expect(inst[lead(api).id]).toMatchObject({ agent_name: "terrarium/lead", state: "running", attached: ["terrarium/lead"] });
    // An ephemeral target without keep: down removes the holder's instance.
    expect(await api.targets.action("terrarium-ws-lab", "down", { agent: lead(api).id })).toMatchObject({ state: "absent" });
    expect((await api.audit({ action: "target.down" })).items.at(-1)).toMatchObject({ target: "terrarium-ws-lab", detail: { agent: lead(api).id } });
  });

  it("refuses an exclusive lease on a lab_from_workspace target with 422 invalid", async () => {
    const api = mock();
    await expect(api.targets.grant("terrarium-ws-lab", { agent: "terrarium/lead", exclusive: true })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.grant("terrarium-ws-lab", { agent: "terrarium/lead" })).resolves.toMatchObject({ exclusive: false, state: "active" });
  });
});

/** A "daemon" that answers upgrades with the rules of ./ws, for the real transport. */
const daemon = (rules: HandshakeRules): MakeRawSocket => (url, protocols = []) => new FakePty({ protocol: acceptHandshake({ url, protocols }, rules) });
/** A daemon from before F4: it reads `?token=` only and sends no subprotocol back, so a browser fails the handshake. */
const oldDaemon: MakeRawSocket = (url, protocols = []) =>
  new FakePty({ protocol: protocols.length ? null : acceptHandshake({ url, protocols }, { token: "T", queryToken: true }) });

const outcome = (s: AuthSocket | PtySocket) =>
  new Promise<string>((resolve) => {
    s.addEventListener("open", () => resolve(`open:${(s as AuthSocket).protocol}`));
    s.addEventListener("close", () => resolve("closed"));
  });

describe("target leases (docs/api.md §17.7)", () => {
  const LEASE_KEYS = ["agent", "agent_name", "exclusive", "expires_at", "granted_at", "granted_by", "id", "project", "reason", "revoked_at", "revoked_by", "state", "target"];
  const byName = (api: ReturnType<typeof mock>, name: string) => api.world.agents.find((a) => a.name === name)!;
  const frames = async (api: ReturnType<typeof mock>) => {
    const out: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => out.push(e));
    await wait();
    return { out, close: () => s.close() };
  };

  it("lists the active leases newest first in the documented shape; ?all=true adds the ended ones", async () => {
    const api = mock();
    const active = await api.targets.leases("lab-win11");
    expect(active).toHaveLength(1);
    expect(Object.keys(active[0]!).sort()).toEqual(LEASE_KEYS);
    expect(active[0]).toMatchObject({ target: "lab-win11", agent_name: "terrarium/runner-dev", exclusive: true, state: "active", reason: "TER-12" });
    expect(active[0]!.granted_by).toMatch(/^agent:/);
    const all = await api.targets.leases("lab-win11", { all: true });
    expect(all.map((l) => l.state)).toEqual(["active", "expired"]);
    expect(all[1]).toMatchObject({ revoked_by: "system" });
    const mine = await api.agents.leases("terrarium/adapter-codex", { all: true });
    expect(mine.map((l) => `${l.target}:${l.state}`)).toEqual(["build-box:active", "lab-win11:expired"]);
    expect((await api.agents.leases("terrarium/adapter-codex")).map((l) => l.target)).toEqual(["build-box"]);
  });

  it("marks each target with access and lease for the caller", async () => {
    const t = (await mock().targets.list())[0]!;
    expect(t).toHaveProperty("access", "operator");
    expect(t).toHaveProperty("lease", null);
  });

  it("grants with 201 the lease, and writes a lease event {change, target, lease_id, expires_at, exclusive, state} to the holder", async () => {
    const api = mock();
    const lead = byName(api, "lead");
    const f = await frames(api);
    const l = await api.targets.grant("build-box", { agent: "terrarium/lead", until: "30m", reason: "TER-3" });
    expect(l).toMatchObject({ target: "build-box", agent: lead.id, agent_name: "terrarium/lead", project: "terrarium", granted_by: "operator", exclusive: false, state: "active", reason: "TER-3", revoked_at: null });
    expect(Date.parse(l.expires_at!) - Date.now()).toBeGreaterThan(29 * 60_000);
    const e = f.out.find((x) => x.kind === "lease")!;
    expect(e).toMatchObject({ kind: "lease", agent: "lead", agent_id: lead.id, project: "terrarium", actor: "operator" });
    expect(e.data).toEqual({ agent_id: lead.id, change: "granted", target: "build-box", lease_id: l.id, expires_at: l.expires_at, exclusive: false, state: "active" });
    expect((await api.audit({ action: "target.grant" })).items.at(-1)).toMatchObject({ target: "build-box", detail: { lease: l.id, agent: lead.id } });
    f.close();
  });

  it("refuses a clash with 409 lease_held and the holder in details; a second lease of one agent is 409 conflict", async () => {
    const api = mock();
    const runner = byName(api, "runner-dev");
    const held = api.world.leases.find((x) => x.target === "lab-win11" && x.state === "active")!;
    const e = await api.targets.grant("lab-win11", { agent: "terrarium/lead" }).catch((x: unknown) => x as ApiError);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 409, code: "lease_held" });
    expect((e as ApiError).details).toEqual({ target: "lab-win11", holder: "terrarium/runner-dev", holder_id: runner.id, lease_id: held.id, exclusive: true, expires_at: held.expires_at });
    // A shared lease on build-box: an exclusive grant next to it is lease_held too.
    await expect(api.targets.grant("build-box", { agent: "terrarium/lead", exclusive: true })).rejects.toMatchObject({ status: 409, code: "lease_held", details: { holder: "terrarium/adapter-codex", exclusive: false, expires_at: null } });
    await expect(api.targets.grant("build-box", { agent: "terrarium/adapter-codex" })).rejects.toMatchObject({ status: 409, code: "conflict" });
  });

  it("refuses a project target for an agent of another project, and a bad or past until, with 422 invalid", async () => {
    const api = mock();
    await expect(api.targets.grant("pg-test", { agent: "terrarium/lead" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.grant("build-box", { agent: "terrarium/lead", until: "soon" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.targets.grant("build-box", { agent: "terrarium/lead", until: "2020-01-01T00:00:00Z" })).rejects.toMatchObject({ status: 422, code: "invalid" });
  });

  it("extends with PATCH {until} and revokes with DELETE, each with a lease event; then the lease is gone from the active list", async () => {
    const api = mock();
    const runner = byName(api, "runner-dev");
    const f = await frames(api);
    const ext = await api.targets.extendLease("lab-win11", runner.id, null);
    expect(ext).toMatchObject({ expires_at: null, state: "active" });
    const end = await api.targets.revokeLease("lab-win11", "terrarium/runner-dev");
    expect(end).toMatchObject({ id: ext.id, state: "revoked", revoked_by: "operator" });
    expect(end.revoked_at).toEqual(expect.any(String));
    expect(f.out.filter((x) => x.kind === "lease").map((x) => [x.data.change, x.data.state])).toEqual([
      ["extended", "active"],
      ["ended", "revoked"],
    ]);
    expect(await api.targets.leases("lab-win11")).toEqual([]);
    await expect(api.targets.revokeLease("lab-win11", runner.id)).rejects.toMatchObject({ status: 404 });
    // The exclusive holder is gone: a new grant works.
    expect((await api.targets.grant("lab-win11", { agent: "terrarium/lead", exclusive: true })).exclusive).toBe(true);
    f.close();
  });

  it("ends a lease at its time as system, with an ended event and state expired", async () => {
    let clock = Date.now();
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, now: () => clock });
    const l = await api.targets.grant("build-box", { agent: "terrarium/lead", until: "90s" });
    const f = await frames(api);
    clock += 91_000;
    const all = await api.targets.leases("build-box", { all: true });
    expect(all.find((x) => x.id === l.id)).toMatchObject({ state: "expired", revoked_by: "system" });
    expect(f.out.find((x) => x.kind === "lease")?.data).toMatchObject({ change: "ended", lease_id: l.id, state: "expired" });
    expect(f.out.find((x) => x.kind === "lease")?.actor).toBe("system");
    f.close();
  });
});

describe("WebSocket auth contract (docs/security-review.md F4)", () => {
  const transport = (make: MakeRawSocket, token: string | null, allowQueryToken = false) =>
    createHttpTransport({ getToken: () => token, location: { protocol: "http:", host: "d" }, makeSocket: make, allowQueryToken });

  it("authenticates with bearer.<token>, and the daemon echoes it", async () => {
    const t = transport(daemon({ token: "T", queryToken: false }), "T");
    const s = t.openSocket(t.wsUrl("/api/events"));
    expect(await outcome(s)).toBe(`open:${bearerProtocol("T")}`);
    expect(s.url).not.toContain("token");
    expect(s.mode).toBe("subprotocol");
  });

  it("is refused with a wrong token, and does not try ?token= by default", async () => {
    const t = transport(daemon({ token: "T", queryToken: true }), "wrong");
    const s = t.openSocket(t.wsUrl("/api/events"));
    expect(await outcome(s)).toBe("closed");
    expect(s.failure).toBe("refused");
  });

  it("falls back to ?token= against an old daemon only with VITE_ALLOW_QUERY_TOKEN", async () => {
    const strict = transport(oldDaemon, "T");
    expect(await outcome(strict.openSocket(strict.wsUrl("/api/events")))).toBe("closed");
    const dev = transport(oldDaemon, "T", true);
    const s = dev.openSocket(dev.wsUrl("/api/events"));
    expect(await outcome(s)).toBe("open:");
    expect(s.mode).toBe("query");
  });

  it("the daemon refuses ?token= when TERRARIUM_WS_QUERY_TOKEN=0", () => {
    expect(acceptHandshake({ url: "ws://d/api/events?token=T", protocols: [] }, { token: "T", queryToken: false })).toBeNull();
    expect(acceptHandshake({ url: "ws://d/api/events?token=T", protocols: [] }, { token: "T", queryToken: true })).toBe("");
    expect(acceptHandshake({ url: "ws://d/api/events", protocols: ["bearer.T"] }, { token: "T", queryToken: false })).toBe("bearer.T");
    expect(acceptHandshake({ url: "ws://d/api/events?token=T", protocols: ["bearer.X"] }, { token: "T", queryToken: true })).toBeNull();
  });

  it("the mock's agent PTY, login PTY, exec console, and events use the subprotocol", async () => {
    const api = mock();
    const live = (await api.agents.list()).find((a) => ["idle", "working"].includes(a.status))!;
    const login = await api.subscriptions.startLogin({ kind: "claude" });
    const exec = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    const want = `open:${bearerProtocol(MOCK_TOKEN)}`;
    expect(await outcome(api.agents.pty(live.id))).toBe(want);
    expect(await outcome(api.subscriptions.loginPty(login.id))).toBe(want);
    expect(await outcome(api.targets.sessionPty("pg-test", exec.session_id))).toBe(want);
    const frames: AgentEvent[] = [];
    const stream = api.events.stream({ since: api.world.seq - 1 });
    stream.subscribe((e) => frames.push(e));
    await wait();
    expect(frames).toHaveLength(1);
    stream.close();
  });
});

describe("session contract (docs/api.md §1.1)", () => {
  /** The browser mock: the mock checks what the transport would send. */
  const browser = (extra: MockOptions = {}) => {
    const c = { token: null as string | null, csrf: null as string | null, unauthorized: 0 };
    const api = createMockApi({
      latencyMs: 0,
      eventsIntervalMs: 0,
      stepMs: 0,
      client: { token: () => c.token, csrf: () => c.csrf, onUnauthorized: () => void c.unauthorized++ },
      ...extra,
    });
    const signIn = async () => {
      const g = await api.session.create(MOCK_TOKEN);
      c.csrf = g!.csrf;
      return g!;
    };
    return { api, c, signIn };
  };

  it("POST /api/session answers {csrf, exp, expires_at} for the right token, and 401 for a wrong one", async () => {
    const t = Date.parse("2026-10-24T10:00:00Z");
    const { api } = browser({ now: () => t, sessionTtlMs: 3600_000 });
    const g = (await api.session.create(MOCK_TOKEN))!;
    expect(Object.keys(g).sort()).toEqual(["csrf", "exp", "expires_at"]);
    expect(g.expires_at).toBe("2026-10-24T11:00:00Z");
    expect(g.exp).toBe(Date.parse(g.expires_at) / 1000);
    const err = await api.session.create("wrong").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AuthError);
    expect(err).toMatchObject({ status: 401, code: "unauthorized" });
  });

  it("the cookie authenticates reads; a mutating call needs X-CSRF-Token, else 403", async () => {
    const { api, c, signIn } = browser();
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
    expect(c.unauthorized).toBe(1);
    const { csrf } = await signIn();
    expect((await api.projects.list()).length).toBeGreaterThan(0);
    c.csrf = null;
    await expect(api.cards.create("terrarium", { title: "no csrf" })).rejects.toMatchObject({ status: 403, code: "forbidden" });
    c.csrf = "wrong";
    await expect(api.cards.create("terrarium", { title: "bad csrf" })).rejects.toMatchObject({ status: 403 });
    c.csrf = csrf;
    expect((await api.cards.create("terrarium", { title: "with csrf" })).title).toBe("with csrf");
  });

  it("guards every mutating Api method of the mock with the CSRF check", async () => {
    const { api, c, signIn } = browser();
    await signIn();
    c.csrf = null;
    const verbs = httpVerbs();
    const checked: string[] = [];
    const pending: Promise<void>[] = [];
    eachMethod(api, (name, call) => {
      const verb = verbs.get(name);
      if (!verb || !isMutating(verb) || name.startsWith("session.")) return;
      checked.push(name);
      pending.push(
        (call() as Promise<unknown>).then(
          () => expect.fail(`${name} took a mutating call without X-CSRF-Token`),
          (e: unknown) => expect((e as ApiError).status, name).toBe(403),
        ),
      );
    });
    await Promise.all(pending);
    expect(checked.length).toBe([...verbs.values()].filter(isMutating).length - 2); // all but session.create and session.remove, which check themselves
  });

  it("every method of the HTTP client has a verb, and the mock has every method", () => {
    const verbs = httpVerbs();
    const mockNames: string[] = [];
    eachMethod(mock(), (name) => void mockNames.push(name));
    for (const name of verbs.keys()) expect(mockNames).toContain(name);
    expect(verbs.size + NOT_TRANSPORT.size).toBe(mockNames.filter((n) => !n.startsWith("world.") && !n.startsWith("sim.")).length);
  });

  it("the bearer token still works without a session", async () => {
    const { api, c } = browser();
    c.token = MOCK_TOKEN;
    expect((await api.cards.create("terrarium", { title: "bearer" })).title).toBe("bearer");
    c.token = "wrong";
    await expect(api.projects.list()).rejects.toBeInstanceOf(AuthError);
  });

  it("the cookie ends at expires_at: 401", async () => {
    let t = Date.parse("2026-10-24T10:00:00Z");
    const { api, signIn } = browser({ now: () => t, sessionTtlMs: 60_000 });
    await signIn();
    await api.projects.list();
    t += 60_000;
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
  });

  it("DELETE /api/session needs the CSRF header, then clears the cookie", async () => {
    const { api, c, signIn } = browser();
    const { csrf } = await signIn();
    c.csrf = null;
    await expect(api.session.remove()).rejects.toMatchObject({ status: 403 });
    c.csrf = csrf;
    await api.session.remove();
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
  });

  it("a daemon without sessions: POST /api/session is 404, which the client answers as null", async () => {
    const { api } = browser({ sessions: false });
    expect(await api.session.create(MOCK_TOKEN)).toBeNull();
    await expect(api.session.remove()).resolves.toBeUndefined();
  });

  it("keeps the cookie in the jar across a reload, as a browser does", async () => {
    const m = new Map<string, string>();
    const jar = { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), removeItem: (k: string) => void m.delete(k) };
    const first = browser({ cookieJar: jar });
    const { csrf } = await first.signIn();
    const again = browser({ cookieJar: jar });
    again.c.csrf = csrf;
    expect((await again.api.cards.create("terrarium", { title: "after reload" })).id).toMatch(/^TER-/);
  });

  it("WebSockets: with the cookie, no subprotocol and the dashboard Origin; without it, refused", async () => {
    const { api, signIn } = browser();
    const live = (await (async () => {
      await signIn();
      return api.agents.list();
    })()).find((a) => ["idle", "working"].includes(a.status))!;
    const login = await api.subscriptions.startLogin({ kind: "claude" });
    const exec = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    const sockets = [api.agents.pty(live.id), api.subscriptions.loginPty(login.id), api.targets.sessionPty("pg-test", exec.session_id)];
    expect(await Promise.all(sockets.map(outcome))).toEqual(["open:", "open:", "open:"]);
    expect(sockets.map((s) => (s as AuthSocket).mode)).toEqual(["cookie", "cookie", "cookie"]);
    const frames: AgentEvent[] = [];
    const stream = api.events.stream({ since: api.world.seq - 1 });
    stream.subscribe((e) => frames.push(e));
    await wait();
    expect(frames).toHaveLength(1);
    stream.close();

    await api.session.remove().catch(() => undefined);
    const signedOut = browser();
    expect(await outcome(signedOut.api.agents.pty(live.id))).toBe("closed");
  });

  it("the daemon takes a cookie upgrade only from the dashboard Origin", () => {
    const h = { url: "ws://d/api/events", protocols: [] as string[], cookie: true };
    expect(acceptHandshake({ ...h, origin: MOCK_ORIGIN }, MOCK_RULES)).toBe("");
    expect(acceptHandshake({ ...h, origin: "https://evil.example" }, MOCK_RULES)).toBeNull();
    expect(acceptHandshake(h, MOCK_RULES)).toBeNull();
    expect(acceptHandshake({ ...h, origin: MOCK_ORIGIN }, { ...MOCK_RULES, origins: [] })).toBeNull();
    // The bearer subprotocol wins over the cookie, as the daemon reads Authorization first.
    expect(acceptHandshake({ ...h, origin: "https://evil.example", protocols: [`bearer.${MOCK_TOKEN}`] }, MOCK_RULES)).toBe(`bearer.${MOCK_TOKEN}`);
    // No cookie and no token: refused, and `?token=` is off by default.
    expect(acceptHandshake({ url: `ws://d/api/events?token=${MOCK_TOKEN}`, protocols: [], origin: MOCK_ORIGIN }, MOCK_RULES)).toBeNull();
  });
});


describe("mail parity with docs/api.md §7 (the mail log's filters)", () => {
  it("lists oldest first by ID, and the cursor asks for IDs after it", async () => {
    const api = mock();
    const all = (await api.mail.list({ limit: 1000 })).items;
    expect(all.map((m) => m.id)).toEqual(all.map((m) => m.id).sort());
    const first = await api.mail.list({ limit: 4 });
    expect(first.next).toBe(first.items.at(-1)!.id);
    const second = await api.mail.list({ limit: 4, cursor: first.next! });
    expect(second.items[0]!.id > first.next!).toBe(true);
    // A cursor that is not a row still means "after this ID", as `id > ?` does.
    const after = await api.mail.list({ cursor: `${all[2]!.id}0` });
    expect(after.items.map((m) => m.id)).toEqual(all.slice(3).map((m) => m.id));
    expect((await api.mail.list({ cursor: all.at(-1)!.id })).items).toEqual([]);
  });

  it("filters by to, from, thread, and state; `system` is no filter (404); a bad state is 400", async () => {
    const api = mock();
    const to = (await api.mail.list({ to: "operator" })).items;
    expect(to.length).toBeGreaterThan(0);
    expect(to.every((m) => m.to === "operator" && m.to_kind === "operator")).toBe(true);
    const from = (await api.mail.list({ from: "operator" })).items;
    expect(from.every((m) => m.from === "operator" && m.from_kind === "operator")).toBe(true);
    // The daemon answers 404 "no agent `system`"; the log filters `system` in the browser.
    await expect(api.mail.list({ from: "system" })).rejects.toMatchObject({ status: 404 });
    const sys = (await api.mail.list({ limit: 1000 })).items.filter((m) => m.from === "system");
    expect(sys.length).toBeGreaterThan(0);
    expect(sys.every((m) => m.from_kind === "system" && m.from_name === "system")).toBe(true);
    const unread = (await api.mail.list({ state: "unread" })).items;
    expect(unread.every((m) => m.state === "queued" || m.state === "delivered")).toBe(true);
    const th = (await api.mail.list({ thread: to[0]!.thread_id })).items;
    expect(th.every((m) => m.thread_id === to[0]!.thread_id)).toBe(true);
    await expect(api.mail.list({ state: "bogus" as "read" })).rejects.toMatchObject({ status: 400 });
  });

  it("sets read_by on the first read and on a reply", async () => {
    const api = mock();
    const m = (await api.mail.list({ to: "operator", state: "unread" })).items[0]!;
    expect(m.read_by).toBeUndefined();
    const read = await api.mail.markRead(m.id);
    expect(read).toMatchObject({ state: "read", read_by: "operator", read_by_name: "operator" });
    expect(read.read_at).toBeTruthy();
    const again = (await api.mail.list({ to: "operator", state: "unread" })).items[0]!;
    await api.mail.reply(again.id, "Seen.");
    expect(await api.mail.get(again.id)).toMatchObject({ state: "replied", read_by: "operator" });
  });

  it("refuses mail to `system` and mail as `system`", async () => {
    const api = mock();
    await expect(api.mail.send({ to: "system", body: "hi" })).rejects.toMatchObject({ status: 422 });
    await expect(api.mail.send({ from: "system", to: "operator", body: "hi" })).rejects.toMatchObject({ status: 403 });
  });

  it("holds agent mail past the loop limit, flags the event, marks the thread paused, and notices the operator once", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const a = await api.mail.send({ from: "terrarium/lead", to: "terrarium/runner-dev", subject: "loop", body: "1" });
    for (let i = 2; i <= 6; i++) await api.mail.send({ from: i % 2 ? "terrarium/lead" : "terrarium/runner-dev", to: i % 2 ? "terrarium/runner-dev" : "terrarium/lead", thread_id: a.thread_id, body: String(i) });
    expect((await api.mail.list({ thread: a.thread_id })).items.every((m) => !m.thread_paused && m.state === "queued")).toBe(true);
    const held = await api.mail.send({ from: "terrarium/lead", to: "terrarium/runner-dev", thread_id: a.thread_id, body: "7" });
    await api.mail.send({ from: "terrarium/runner-dev", to: "terrarium/lead", thread_id: a.thread_id, body: "8" });
    await wait();
    expect(held).toMatchObject({ state: "delivered", thread_paused: true });
    const ev = frames.find((e) => e.kind === "mail" && e.data.message_id === held.id)!;
    expect(ev.data).toMatchObject({ held: true, thread_state: "paused" });
    expect(Object.keys(ev.data).sort()).toEqual(["agent_id", "from", "from_name", "held", "message_id", "subject", "thread_id", "thread_state", "to", "to_name"]);
    const thread = (await api.mail.list({ thread: a.thread_id })).items;
    expect(thread.every((m) => m.thread_paused === true)).toBe(true);
    const notices = thread.filter((m) => m.from === "system");
    expect(notices).toHaveLength(1);
    expect(notices[0]).toMatchObject({ to: "operator", subject: `Thread ${a.thread_id} needs a human`, state: "delivered" });
    // An operator message ends the pause; `thread_paused` is absent when false.
    await api.mail.reply(held.id, "Stop here.");
    expect((await api.mail.list({ thread: a.thread_id })).items.some((m) => "thread_paused" in m)).toBe(false);
    s.close();
  });

  it("sends the blocked-card notice and the quota alert from `system` to the operator", async () => {
    const api = mock();
    await api.cards.move("TER-13", { column: "blocked", comment: "Needs a login." });
    const sys = (await api.mail.list({ to: "operator", limit: 1000 })).items.filter((m) => m.from === "system");
    expect(sys.some((m) => m.subject.startsWith("TER-13 blocked: ") && m.body.includes("Needs a login."))).toBe(true);
    api.sim.setWindow("anthropic-max", "5h", 85);
    const after = (await api.mail.list({ limit: 1000 })).items.filter((m) => m.from === "system");
    expect(after.some((m) => m.subject === "Quota: anthropic-max 5h window at 85%" && m.to === "operator")).toBe(true);
  });
});

describe("schedules (docs/api.md §19, the daemon's api/schedules.rs)", () => {
  const SCHEDULE_KEYS = ["action", "catch_up", "consecutive_skips", "created_at", "created_by", "enabled", "id", "last_run", "max_concurrency", "name", "next_run_at", "project", "run_as", "runs_count", "source", "spec", "updated_at"];
  const RUN_KEYS = ["actor", "error", "event_seq", "finished_at", "id", "output", "schedule", "started_at", "status", "summary", "trigger"];
  const stream = (api: ReturnType<typeof mock>) => {
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    return { frames: () => frames.filter((e) => e.kind === "schedule"), close: () => s.close() };
  };
  const exec = { type: "exec" as const, target: "build-box", cmd: ["make", "test"] };
  const err = (p: Promise<unknown>) => p.then(() => null, (e: unknown) => e as ApiError);

  it("seeds three project schedules across two projects and one global, in the daemon's shape", async () => {
    const api = mock();
    const all = await api.schedules.list({ kind: "time" });
    expect(all.map((s) => s.project ?? "global").sort()).toEqual(["global", "terrarium", "terrarium", "webshop"]);
    for (const s of all) expect(Object.keys(s).sort()).toEqual(SCHEDULE_KEYS);
    const failed = all.find((s) => s.last_run?.status === "failed")!;
    expect(failed.project).toBe("webshop");
    expect(Object.keys(failed.last_run!).sort()).toEqual(["at", "run_id", "status", "summary"]);
    // A file schedule is in projects/<name>.toml, so it has a project.
    expect(all.filter((s) => s.source === "file").every((s) => s.project)).toBe(true);
  });

  it("lists the global schedules and the project's for ?project=, and filters ?enabled=", async () => {
    const api = mock();
    const t = await api.schedules.list({ project: "terrarium" });
    expect(t.every((s) => s.project === null || s.project === "terrarium")).toBe(true);
    expect(t.some((s) => s.project === null)).toBe(true);
    expect(t.some((s) => s.project === "webshop")).toBe(false);
    expect((await api.schedules.list({ enabled: false })).map((s) => s.enabled)).toEqual([false]);
    await expect(api.schedules.list({ project: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("creates each action type with 201 shapes, resolves agent keys, computes next_run_at, and emits created with IDs", async () => {
    const api = mock();
    const ev = stream(api);
    await wait();
    const actions = [
      { type: "mail" as const, to: "terrarium/lead", subject: "", body: "b" },
      { type: "turn" as const, agent: "terrarium/lead", text: "go", start_if_stopped: true },
      exec,
      { type: "agent" as const, agent: "lead", verb: "restart" as const },
      { type: "probe" as const, subscription: "anthropic-max" },
    ];
    for (const [i, action] of actions.entries()) {
      const s = await api.schedules.create({ name: `s${i}`, project: "terrarium", spec: { kind: "cron", expr: "*/5 * * * *", tz: "UTC" }, action });
      expect(s).toMatchObject({ project: "terrarium", enabled: true, catch_up: "skip", source: "api", runs_count: 0, last_run: null, max_concurrency: 1 });
      expect(Date.parse(s.next_run_at!)).toBeGreaterThan(Date.now());
      expect(s.action.type).toBe(action.type);
    }
    expect((await api.schedules.list()).find((s) => s.name === "s3")!.action).toEqual({ type: "agent", agent: "terrarium/lead", verb: "restart" });
    const e = ev.frames().find((x) => x.data.change === "created")!;
    expect(e).toMatchObject({ kind: "schedule", project: "terrarium", summary: "schedule terrarium/s0: created" });
    expect(Object.keys(e.data).sort()).toEqual(["change", "name", "project", "schedule"]);
    expect(typeof e.data.schedule).toBe("string");
    const g = await api.schedules.create({ name: "once", project: null, spec: { kind: "at", at: new Date(Date.now() + 3_600_000).toISOString() }, action: { type: "mail", to: "operator", subject: "Hi", body: "" } });
    expect(g.project).toBeNull();
    expect((await api.schedules.create({ name: "iv", project: "webshop", spec: { kind: "interval", every: "1h30m" }, action: exec })).next_run_at).toBeTruthy();
    expect(api.world.audit.at(-1)).toMatchObject({ action: "schedule.create", target: "iv" });
    ev.close();
  });

  it("refuses bad fields with 422 invalid, details.field, and the message `<field>: ...`; a cron error quotes the expression", async () => {
    const api = mock();
    const ok = { name: "x", project: "terrarium", spec: { kind: "interval" as const, every: "15m" }, action: exec };
    const bad = (patch: object) => err(api.schedules.create({ ...ok, ...patch } as never)).then((e) => e!);
    const cron = await bad({ spec: { kind: "cron", expr: "61 * * * *", tz: "UTC" } });
    expect(cron).toMatchObject({ status: 422, code: "invalid", details: { field: "spec.expr" } });
    expect(cron.message).toMatch(/^spec\.expr: bad cron expression `61 \* \* \* \*`/);
    expect(await bad({ spec: { kind: "cron", expr: "0 9 * * *", tz: "Mars/Base" } })).toMatchObject({ status: 422, details: { field: "spec.tz" } });
    expect(await bad({ spec: { kind: "interval", every: "30s" } })).toMatchObject({ details: { field: "spec.every" } });
    expect(await bad({ spec: { kind: "at", at: "2000-01-01T00:00:00Z" } })).toMatchObject({ details: { field: "spec.at" } });
    expect(await bad({ name: "Bad Name" })).toMatchObject({ details: { field: "name" } });
    expect(await bad({ action: { type: "turn", agent: "nobody", text: "x" } })).toMatchObject({ details: { field: "action.agent" } });
    expect(await bad({ action: { type: "exec", target: "nope", cmd: ["ls"] } })).toMatchObject({ details: { field: "action.target" } });
    expect(await bad({ action: { type: "exec", target: "build-box", cmd: [] } })).toMatchObject({ details: { field: "action.cmd" } });
    expect(await bad({ action: { ...exec, timeout_secs: 4000 } })).toMatchObject({ details: { field: "action.timeout_secs" } });
    expect(await bad({ action: { type: "probe", subscription: "nope" } })).toMatchObject({ details: { field: "action.subscription" } });
    expect(await bad({ project: "nope" })).toMatchObject({ details: { field: "project" } });
    await api.schedules.create(ok);
    expect(await bad({})).toMatchObject({ status: 409, code: "conflict" });
  });

  it("answers 403 forbidden: a member writes nothing and sees no global schedule; a project_admin stays in its project; a global schedule needs a sysadmin", async () => {
    const member = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "terrarium" } });
    const list = await member.schedules.list({ project: "terrarium" });
    expect(list.every((s) => s.project === "terrarium")).toBe(true);
    const t = list.find((s) => s.name === "tests")!;
    await expect(member.schedules.disable(t.id)).rejects.toMatchObject({ status: 403, code: "forbidden" });
    await expect(member.schedules.run(t.id)).rejects.toMatchObject({ status: 403 });
    await expect(member.schedules.create({ name: "m", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: exec })).rejects.toMatchObject({ status: 403 });
    await expect(member.schedules.list({ project: "webshop" })).rejects.toMatchObject({ status: 403 });
    const admin = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "project_admin", project: "terrarium" } });
    const probe = { type: "probe" as const, subscription: "anthropic-max" };
    await expect(admin.schedules.create({ name: "g", project: null, spec: { kind: "interval", every: "1h" }, action: probe })).rejects.toMatchObject({ status: 403, message: expect.stringContaining("sysadmin") });
    await expect(admin.schedules.create({ name: "w", project: "webshop", spec: { kind: "interval", every: "1h" }, action: probe })).rejects.toMatchObject({ status: 403 });
    await expect(admin.schedules.create({ name: "x", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "mail", to: "webshop/shop-lead", subject: "", body: "x" } })).rejects.toMatchObject({ status: 403, message: expect.stringContaining("leaves the project") });
    await expect(admin.schedules.create({ name: "y", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "exec", target: "pg-test", cmd: ["ls"] } })).rejects.toMatchObject({ status: 403 });
    expect((await admin.schedules.create({ name: "a", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: exec })).project).toBe("terrarium");
    // The operator may reach across projects.
    const op = mock();
    expect((await op.schedules.create({ name: "z", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "exec", target: "pg-test", cmd: ["ls"] } })).action).toMatchObject({ target: "pg-test" });
  });

  it("patches (no project in the body), enables, disables (next_run_at null), and deletes, with an event for each", async () => {
    const api = mock();
    const ev = stream(api);
    await wait();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const p = await api.schedules.update(s.id, { spec: { kind: "cron", expr: "0 9 * * *", tz: "UTC" } });
    expect(p.spec).toEqual({ kind: "cron", expr: "0 9 * * *", tz: "UTC" });
    expect(p.action).toEqual(s.action);
    expect(new Date(p.next_run_at!).getUTCHours()).toBe(9);
    await expect(api.schedules.update(s.id, { project: "webshop" } as never)).rejects.toMatchObject({ status: 422 });
    expect(await api.schedules.disable(s.id)).toMatchObject({ enabled: false, next_run_at: null });
    expect((await api.schedules.enable(s.id)).next_run_at).toBeTruthy();
    await api.schedules.remove(s.id);
    await expect(api.schedules.get(s.id)).rejects.toMatchObject({ status: 404 });
    expect(ev.frames().map((e) => e.data.change)).toEqual(["updated", "disabled", "enabled", "deleted"]);
    const file = (await api.schedules.list()).find((x) => x.source === "file")!;
    for (const call of [api.schedules.remove(file.id), api.schedules.update(file.id, { enabled: false }), api.schedules.disable(file.id)])
      await expect(call).rejects.toMatchObject({ status: 409, code: "file_managed", message: expect.stringContaining("projects/terrarium.toml") });
    ev.close();
  });

  it("runs now: run_started at once, the answer is the run when it ends, then run_finished with {run, status, trigger}", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleRunMs: 15 });
    const ev = stream(api);
    await wait();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const pending = api.schedules.run(s.id);
    await wait(2);
    expect(ev.frames().map((e) => e.data.change)).toEqual(["run_started"]);
    expect(ev.frames()[0]!.data).toMatchObject({ schedule: s.id, status: "running", trigger: "manual" });
    expect(typeof ev.frames()[0]!.data.run).toBe("string");
    // A second run while one goes on is skipped at once.
    const overlap = await api.schedules.run(s.id);
    expect(overlap).toMatchObject({ status: "skipped", summary: expect.stringContaining("still running") });
    const r = await pending;
    expect(Object.keys(r).sort()).toEqual(RUN_KEYS);
    expect(r).toMatchObject({ schedule: s.id, status: "ok", trigger: "manual", summary: "exec on build-box: exit 0" });
    expect(r.finished_at).toBeTruthy();
    expect(r.output).toMatchObject({ target: "build-box", exit_code: 0, stdout: expect.stringContaining("make test") });
    const finished = ev.frames().filter((e) => e.data.change === "run_finished").at(-1)!;
    expect(finished.data).toMatchObject({ schedule: s.id, run: r.id, status: "ok", trigger: "manual" });
    expect(finished.summary).toBe("schedule terrarium/tests: ok (exec on build-box: exit 0)");
    const after = await api.schedules.get(s.id);
    expect(after.runs_count).toBe(s.runs_count + 2);
    expect(after.last_run).toMatchObject({ run_id: r.id, status: "ok", at: r.finished_at });
    const failing = await api.schedules.create({ name: "f", project: "terrarium", spec: { kind: "interval", every: "1h" }, action: { type: "exec", target: "build-box", cmd: ["false"] } });
    expect(await api.schedules.run(failing.id)).toMatchObject({ status: "failed", error: "exit code 1" });
    expect((await api.schedules.get(failing.id)).last_run).toMatchObject({ status: "failed" });
    ev.close();
  });

  it("pages runs newest first with a cursor", async () => {
    const api = mock();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const p1 = await api.schedules.runs(s.id, { limit: 10 });
    expect(p1.items).toHaveLength(10);
    expect(p1.items[0]!.started_at > p1.items[1]!.started_at).toBe(true);
    expect(p1.next).toBe(p1.items.at(-1)!.id);
    const p2 = await api.schedules.runs(s.id, { limit: 10, cursor: p1.next! });
    const p3 = await api.schedules.runs(s.id, { limit: 10, cursor: p2.next! });
    expect(p3.items).toHaveLength(5);
    expect(p3.next).toBeNull();
    expect(new Set([...p1.items, ...p2.items, ...p3.items].map((r) => r.id)).size).toBe(25);
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}`);
      return new Response(init.method === "DELETE" ? null : "[]", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.schedules.list({ project: "terrarium", enabled: true });
    await api.schedules.runs("S1", { limit: 20, cursor: "R9" });
    await api.schedules.run("S1");
    await api.schedules.enable("S1");
    await api.schedules.disable("S1");
    await api.schedules.update("S1", { enabled: false });
    await api.schedules.remove("S1");
    expect(calls).toEqual([
      "GET /api/schedules?project=terrarium&enabled=true&limit=1000",
      "GET /api/schedules/S1/runs?limit=20&cursor=R9",
      "POST /api/schedules/S1/run",
      "POST /api/schedules/S1/enable",
      "POST /api/schedules/S1/disable",
      "PATCH /api/schedules/S1",
      "DELETE /api/schedules/S1",
    ]);
  });

  it("gives each subscription default_models, as the daemon does", async () => {
    const api = mock();
    const subs = await api.subscriptions.list();
    for (const s of subs) expect(s.default_models).toBeTypeOf("object");
    const zai = subs.find((s) => s.name === "zai-key")!;
    expect(zai.default_models).toEqual({ pi: "glm-5.3-flash", claude: "glm-5.3" });
    // pi refuses a Claude plan: no pi default.
    expect(subs.find((s) => s.name === "anthropic-max")!.default_models).toEqual({});
  });

  it("adds subscription_detail to GET /api/agents/{id} only", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "est", project: "terrarium", harness: "pi", subscription: "zai-key" });
    expect(a.subscription_detail).toBeUndefined();
    const shown = await api.agents.get(a.id);
    expect(Object.keys(shown.subscription_detail!).sort()).toEqual(["default_model", "harnesses", "login_status", "name", "plan", "provider"]);
    expect((await api.agents.list()).every((x) => x.subscription_detail === undefined)).toBe(true);
  });

  it("answers GET /api/subscriptions/{name}/models in the daemon's shape", async () => {
    const api = mock();
    const zai = await api.subscriptions.models("zai-key");
    expect(zai.source).toBe("provider");
    expect(zai.models.map((m) => m.id)).toContain("glm-5.3-flash");
    expect(typeof zai.fetched_at).toBe("string");
    const pi = await api.subscriptions.models("openrouter-key", { refresh: true });
    expect(pi.source).toBe("pi");
    expect(pi.models[0]).toHaveProperty("context");
    const fb = await api.subscriptions.models("copilot-pro");
    expect(fb.source).toBe("fallback");
    expect(fb.notes?.length).toBeGreaterThan(0);
    await expect(api.subscriptions.models("nope")).rejects.toMatchObject({ status: 404 });
  });
});

describe("triggers (docs/api.md §19.4)", () => {
  const on = { kind: "on" as const, kinds: ["ci.run.failed"] };
  const mail = { type: "mail" as const, to: "terrarium/lead", subject: "CI failed: {{event.data.repo}}", body: "Run: {{event.data.run_url}}" };
  const err = (p: Promise<unknown>) => p.then(() => null, (e: unknown) => e as ApiError);

  it("filters ?kind=time|trigger and answers 400 for another kind", async () => {
    const api = mock();
    const t = await api.schedules.list({ kind: "trigger" });
    expect(t.length).toBeGreaterThan(0);
    expect(t.every((s) => s.spec.kind === "on" && s.next_run_at === null)).toBe(true);
    expect((await api.schedules.list({ kind: "time" })).some((s) => s.spec.kind === "on")).toBe(false);
    await expect(api.schedules.list({ kind: "weekly" as never })).rejects.toMatchObject({ status: 400 });
  });

  it("gives each trigger run its event, and every run event_seq", async () => {
    const api = mock();
    const t = (await api.schedules.list({ kind: "trigger" }))[0]!;
    const runs = (await api.schedules.runs(t.id)).items;
    expect(runs.every((r) => r.trigger === "event" && typeof r.event_seq === "number" && r.event?.seq === r.event_seq)).toBe(true);
    expect(Object.keys(runs[0]!.event!).sort()).toEqual(["kind", "seq", "summary"]);
    const time = (await api.schedules.list({ kind: "time" }))[0]!;
    const r = (await api.schedules.runs(time.id)).items[0]!;
    expect(r.event_seq).toBeNull();
    expect(r).not.toHaveProperty("event");
  });

  it("creates a trigger with the default max_per_hour, and refuses turn, agent, and probe on the field action", async () => {
    const api = mock();
    const s = await api.schedules.create({ name: "t1", project: "terrarium", spec: on, action: mail });
    expect(s.spec).toEqual({ kind: "on", kinds: ["ci.run.failed"], max_per_hour: 30 });
    expect(s.next_run_at).toBeNull();
    for (const action of [
      { type: "turn" as const, agent: "terrarium/lead", text: "go" },
      { type: "agent" as const, agent: "terrarium/lead", verb: "restart" as const },
      { type: "probe" as const, subscription: "anthropic-max" },
    ])
      expect(await err(api.schedules.create({ name: "t2", project: "terrarium", spec: on, action }))).toMatchObject({ status: 422, details: { field: "action" } });
  });

  it("names the spec field of a bad trigger, and refuses an unknown placeholder", async () => {
    const api = mock();
    const bad = (spec: object, action: object = mail) => err(api.schedules.create({ name: "t", project: "terrarium", spec: { ...on, ...spec } as never, action: action as never }));
    expect(await bad({ kinds: [] })).toMatchObject({ details: { field: "spec.kinds" } });
    expect(await bad({ match: [{ path: "branch", op: "eq", value: "main" }] })).toMatchObject({ details: { field: "spec.match" } });
    expect(await bad({ match: [{ path: "data.repo", op: "in", value: "x" }] })).toMatchObject({ details: { field: "spec.match" } });
    expect(await bad({ dedupe: { key: "k", window: "8d" } })).toMatchObject({ details: { field: "spec.dedupe.window" } });
    expect(await bad({ escalate: { key: "k", after: 0 } })).toMatchObject({ details: { field: "spec.escalate.after" } });
    expect(await bad({ max_per_hour: 601 })).toMatchObject({ details: { field: "spec.max_per_hour" } });
    expect(await bad({}, { ...mail, subject: "{{event.nope}}" })).toMatchObject({ details: { field: "action.subject" } });
  });

  it("takes a card action on a project, not on a global schedule", async () => {
    const api = mock();
    const card = { type: "card" as const, op: "open" as const, key: "ci:{{event.data.repo}}", title: "CI failed" };
    expect((await api.schedules.create({ name: "c", project: "terrarium", spec: on, action: card })).action).toEqual(card);
    expect(await err(api.schedules.create({ name: "c", project: null, spec: on, action: card }))).toMatchObject({ details: { field: "action" } });
    expect(await err(api.schedules.create({ name: "c2", project: "terrarium", spec: on, action: { ...card, key: " " } }))).toMatchObject({ details: { field: "action.key" } });
    expect(await err(api.schedules.create({ name: "c3", project: "terrarium", spec: on, action: { ...card, column: "done" } }))).toMatchObject({ details: { field: "action.column" } });
    // A time schedule may use the card action too.
    expect((await api.schedules.create({ name: "weekly", project: "terrarium", spec: { kind: "cron", expr: "0 9 * * 1", tz: "UTC" }, action: { type: "card", op: "open", key: "review" } })).action.type).toBe("card");
  });
});

describe("event sources (docs/api.md §20)", () => {
  const SOURCE_KEYS = ["created_at", "created_by", "enabled", "hook_note", "hook_path", "hook_url", "id", "kind", "last_error", "last_seen", "name", "project", "received", "setup", "updated_at"];

  it("lists in the daemon's shape, with no secret", async () => {
    const api = mock();
    const all = await api.eventSources.list();
    expect(all.length).toBeGreaterThan(0);
    for (const s of all) {
      expect(Object.keys(s).sort()).toEqual(SOURCE_KEYS);
      expect(s.hook_path).toBe(`/hooks/${s.project ?? "_"}/${s.name}`);
    }
    expect((await api.eventSources.list({ project: "webshop" })).every((s) => s.project === null || s.project === "webshop")).toBe(true);
  });

  it("answers the secret only at create and rotate, with the setup steps of the kind", async () => {
    const api = mock();
    const s = await api.eventSources.create({ name: "fj", project: "terrarium", kind: "forgejo" });
    expect(s.secret).toMatch(/^[0-9a-f]{64}$/);
    expect(s.setup.join(" ")).toContain("application/json");
    expect(await api.eventSources.get(s.id)).not.toHaveProperty("secret");
    const r = await api.eventSources.rotate(s.id);
    expect(r.secret).not.toBe(s.secret);
    expect((await api.eventSources.list()).every((x) => !("secret" in x))).toBe(true);
    expect((await api.eventSources.update(s.id, { enabled: false })).enabled).toBe(false);
    await api.eventSources.remove(s.id);
    await expect(api.eventSources.get(s.id)).rejects.toMatchObject({ status: 404 });
  });

  it("refuses a bad name, a taken name, a global source from a project_admin, and any write from a member", async () => {
    const api = mock();
    await expect(api.eventSources.create({ name: "Bad Name", project: "terrarium", kind: "github" })).rejects.toMatchObject({ status: 422, details: { field: "name" } });
    await expect(api.eventSources.create({ name: "gh", project: "terrarium", kind: "github" })).rejects.toMatchObject({ status: 409 });
    const admin = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "project_admin", project: "terrarium" } });
    await expect(admin.eventSources.create({ name: "g", project: null, kind: "generic" })).rejects.toMatchObject({ status: 403 });
    expect((await admin.eventSources.create({ name: "g", project: "terrarium", kind: "generic" })).project).toBe("terrarium");
    const member = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "terrarium" } });
    const gh = (await member.eventSources.list({ project: "terrarium" })).find((x) => x.name === "gh")!;
    await expect(member.eventSources.rotate(gh.id)).rejects.toMatchObject({ status: 403 });
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}`);
      return new Response(init.method === "DELETE" ? null : "[]", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.schedules.list({ project: "terrarium", kind: "trigger" });
    await api.eventSources.list({ project: "terrarium" });
    await api.eventSources.create({ name: "gh", kind: "github", project: "terrarium" });
    await api.eventSources.update("S1", { enabled: false });
    await api.eventSources.rotate("terrarium/gh");
    await api.eventSources.remove("S1");
    expect(calls).toEqual([
      "GET /api/schedules?project=terrarium&kind=trigger&limit=1000",
      "GET /api/event-sources?project=terrarium",
      "POST /api/event-sources",
      "PATCH /api/event-sources/S1",
      "POST /api/event-sources/terrarium%2Fgh/rotate",
      "DELETE /api/event-sources/S1",
    ]);
  });
});

describe("agent templates (docs/api.md §21)", () => {
  const TEMPLATE_KEYS = ["created_at", "created_by", "description", "id", "name", "project", "revision", "source", "spec", "updated_at"];

  it("lists in the daemon's shape, global first, a project's with the global ones", async () => {
    const api = mock();
    const all = await api.templates.list();
    expect(all.length).toBeGreaterThan(1);
    for (const t of all) expect(Object.keys(t).sort()).toEqual(TEMPLATE_KEYS);
    expect(all[0]!.project).toBeNull();
    expect((await api.templates.list({ project: "webshop" })).every((t) => t.project === null || t.project === "webshop")).toBe(true);
    expect((await api.templates.list({ scope: "global" })).every((t) => t.project === null)).toBe(true);
  });

  it("normalizes the spec, keeps the name unique in its scope, and counts revisions", async () => {
    const api = mock();
    const t = await api.templates.create({ name: "tester", project: "terrarium", spec: { harness: "pi", roles: [] } });
    expect(t.spec).toEqual({ harness: "pi", policy: { targets: [], mail_to: ["*"] }, restart: "on-failure" });
    expect(t.revision).toBe(1);
    // The same name in another scope is fine; in the same scope it is a conflict.
    expect((await api.templates.create({ name: "tester", project: null, spec: { harness: "claude" } })).project).toBeNull();
    await expect(api.templates.create({ name: "tester", project: "terrarium", spec: { harness: "pi" } })).rejects.toMatchObject({ status: 409 });
    await expect(api.templates.create({ name: "Bad", spec: { harness: "pi" } })).rejects.toMatchObject({ status: 422, details: { field: "name" } });
    await expect(api.templates.create({ name: "x", spec: { harness: "nope" } as never })).rejects.toMatchObject({ status: 422, details: { field: "spec" } });
    await expect(api.templates.create({ name: "x", spec: { harness: "pi", name: "w" } as never })).rejects.toMatchObject({ status: 422, details: { field: "spec" } });
    const u = await api.templates.update("terrarium/tester", { description: "runs the tests" });
    expect(u.revision).toBe(2);
    expect((await api.templates.update(u.id, { description: "runs the tests" })).revision).toBe(2);
    await api.templates.remove(u.id);
    await expect(api.templates.get(u.id)).rejects.toMatchObject({ status: 404 });
  });

  it("resolves the project's template before the global one, and global: forces the global one", async () => {
    const api = mock();
    await api.templates.create({ name: "worker", project: "terrarium", spec: { harness: "pi" } });
    expect((await api.templates.resolve("worker", "terrarium")).agent).toMatchObject({ project: "terrarium", harness: "pi" });
    const g = await api.templates.resolve("global:worker", "terrarium");
    expect(g.template.project).toBeNull();
    expect(g.agent.harness).toBe("claude");
    expect((await api.templates.resolve("worker", "webshop")).template.project).toBeNull();
    await expect(api.templates.resolve("reviewer", "webshop")).rejects.toMatchObject({ status: 404 });
    await expect(api.templates.resolve("terrarium/reviewer", "webshop")).rejects.toMatchObject({ status: 422, code: "invalid", details: { field: "template" } });
  });

  it("makes an agent from a template: the body wins per field, policy merges per key, null clears", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "r1", project: "terrarium", template: "reviewer", model: null, policy: { mail_to: ["*"] } as never, restart: "never" });
    expect(a).toMatchObject({ harness: "codex", prompt: "worker", restart: "never", policy: { targets: ["build-box"], mail_to: ["*"] } });
    expect(a.model).toBeUndefined();
    expect(a.template).toMatchObject({ name: "reviewer", project: "terrarium", revision: 1 });
    await expect(api.agents.create({ name: "r2", project: "terrarium", template: "nope" })).rejects.toMatchObject({ status: 404 });
  });

  it("makes a template from an agent, in the agent's project by default", async () => {
    const api = mock();
    const t = await api.templates.fromAgent("terrarium/lead", { name: "lead-copy" });
    const lead = await api.agents.get("terrarium/lead");
    expect(t.project).toBe("terrarium");
    expect(t.spec.harness).toBe(lead.harness);
    expect((await api.templates.fromAgent("terrarium/lead", { name: "lead-copy", project: null })).project).toBeNull();
  });

  it("follows the rights: everyone reads global; a project_admin manages its project's; a member only reads", async () => {
    const admin = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "project_admin", project: "terrarium" } });
    await expect(admin.templates.create({ name: "g", project: null, spec: { harness: "pi" } })).rejects.toMatchObject({ status: 403 });
    await expect(admin.templates.create({ name: "w", project: "webshop", spec: { harness: "pi" } })).rejects.toMatchObject({ status: 403 });
    expect((await admin.templates.create({ name: "w", spec: { harness: "pi" } })).project).toBe("terrarium");
    const member = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller: { role: "member", project: "webshop" } });
    const seen = await member.templates.list();
    expect(seen.some((t) => t.project === null)).toBe(true);
    expect(seen.some((t) => t.project === "terrarium")).toBe(false);
    await expect(member.templates.update("global:worker", { description: "x" })).rejects.toMatchObject({ status: 403 });
    await expect(member.templates.get("terrarium/reviewer")).rejects.toMatchObject({ status: 403 });
  });

  it("emits template events", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => frames.push(e));
    await wait();
    const t = await api.templates.create({ name: "ev", project: "terrarium", spec: { harness: "pi" } });
    await wait();
    const e = frames.find((x) => x.kind === "template")!;
    expect(e).toMatchObject({ project: "terrarium", data: { change: "created", template: t.id, name: "ev", key: "terrarium/ev", revision: 1 } });
    s.close();
  });

  it("the HTTP client sends the documented routes", async () => {
    const calls: string[] = [];
    const t = createHttpTransport({ baseUrl: "http://d", getToken: () => null, fetch: (async (url: string, init: RequestInit) => {
      calls.push(`${init.method} ${String(url).replace("http://d", "")}`);
      return new Response(init.method === "DELETE" ? null : "[]", { status: init.method === "DELETE" ? 204 : 200, headers: { "content-type": "application/json" } });
    }) as unknown as typeof fetch });
    const { createHttpApi } = await import("../client");
    const api = createHttpApi(t);
    await api.templates.list({ scope: "global" });
    await api.templates.create({ name: "w", spec: { harness: "pi" } });
    await api.templates.get("global:worker");
    await api.templates.update("terrarium/w", { description: "d" });
    await api.templates.resolve("worker", "terrarium");
    await api.templates.fromAgent("terrarium/lead", { name: "l" });
    await api.templates.remove("T1");
    expect(calls).toEqual([
      "GET /api/templates?scope=global",
      "POST /api/templates",
      "GET /api/templates/global%3Aworker",
      "PATCH /api/templates/terrarium%2Fw",
      "GET /api/templates/worker/resolve?project=terrarium",
      "POST /api/templates/from-agent/terrarium%2Flead",
      "DELETE /api/templates/T1",
    ]);
  });
});

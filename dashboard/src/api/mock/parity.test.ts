// The mock must answer in the daemon's shapes (docs/api.md), so mock mode and
// real mode never diverge.
import { describe, expect, it } from "vitest";
import { ApiError, createHttpTransport } from "../http";
import { createMockApi } from ".";
import type { AgentEvent } from "../types";
import { bearerProtocol, type AuthSocket, type MakeRawSocket, type PtySocket } from "../sockets";
import { FakePty } from "./pty";
import { acceptHandshake, MOCK_TOKEN, type HandshakeRules } from "./ws";

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
    await expect(api.targets.create({ name: "x" })).rejects.toMatchObject({ status: 405 });
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
      expect(["logged_in", "expiring", "expired", "missing", "unknown"]).toContain(s.login_status);
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

  it("runs the target lifecycle in the mock", async () => {
    const api = mock();
    await api.targets.action("lab-win11", "up");
    expect((await api.targets.list()).find((x) => x.id === "lab-win11")!.state).toBe("starting");
    await wait(10);
    expect((await api.targets.list()).find((x) => x.id === "lab-win11")!.state).toBe("up");
    await api.targets.action("lab-win11", "snapshot");
    expect((await api.targets.list()).find((x) => x.id === "lab-win11")!.snapshots).toEqual(["clean", "snap-2"]);
    const opened = await api.targets.exec("pg-test", { shell: true, pty: { rows: 24, cols: 80 } });
    expect(opened).toMatchObject({ target: "pg-test", kind: "shell" });
    await api.targets.closeSession("pg-test", opened.session_id);
    await expect(api.targets.closeSession("pg-test", opened.session_id)).rejects.toMatchObject({ status: 404 });
    await expect(api.targets.action("build-box", "up")).rejects.toMatchObject({ code: "invalid" });
    await expect(api.targets.action("pi-runner", "up")).rejects.toMatchObject({ code: "invalid" });
    await expect(api.targets.action("pg-test", "snapshot")).rejects.toMatchObject({ code: "invalid" });
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


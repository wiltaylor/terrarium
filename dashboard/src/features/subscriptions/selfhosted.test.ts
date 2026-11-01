// The Self-hosted model path of the wizard, the subscription row texts,
// and the mock's self-hosted, enable, disable, and stop-all calls.
import { describe, expect, it } from "vitest";
import { ApiError } from "../../api/http";
import { createMockApi } from "../../api/mock";
import { checkHttpUrl, statusUrlOf } from "../../api/mock/selfhosted";
import type { Subscription } from "../../api/types";
import { compatibleSubscriptions, selfHostedServes, subscriptionOptions } from "../agents/subscription";
import { lifecycleBadge } from "../../lib/lifecycle";
import { apiKeyBody, emptySelfHostedForm, parseCount, parseModels, reloginFor, selfHostedBody, selfHostedSummary, stopAllText, streamsText, styleHarnesses, tokensText, urlError, wizardFromQuery } from "./wizard";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const form = (over = {}) => ({ ...emptySelfHostedForm(), name: "gpu", baseUrl: "http://gpu.lan:8000/v1", ...over });

describe("the Self-hosted model form", () => {
  it("builds the create body and the key call", () => {
    const b = selfHostedBody(form({ api: "anthropic", baseUrl: " http://gpu.lan:8000/ ", key: " sk-1 ", models: "m1, m2\nm1", defaultModel: "m2", contextWindow: "128k", maxOutputTokens: "8192", maxConcurrent: "2", harnesses: ["claude", "pi", "codex"] }));
    expect(b).toEqual({
      key: "sk-1",
      def: {
        name: "gpu",
        provider: "selfhosted",
        harnesses: ["claude", "pi"],
        selfhosted: { api: "anthropic", base_url: "http://gpu.lan:8000", models: ["m1", "m2"], default_model: "m2", context_window: 131_072, max_output_tokens: 8192, max_concurrent: 2 },
      },
    });
    // No key: no key call. No limits: no fields.
    expect(selfHostedBody(form())).toEqual({ def: { name: "gpu", provider: "selfhosted", harnesses: ["pi", "codex"], selfhosted: { api: "openai", base_url: "http://gpu.lan:8000/v1" } } });
  });

  it("refuses bad URLs, numbers, and harnesses", () => {
    const err = (over: object) => {
      const r = selfHostedBody(form(over));
      return "error" in r ? r.error : null;
    };
    expect(err({ baseUrl: "ftp://gpu/v1" })).toContain("http://");
    expect(err({ baseUrl: "http://user:pw@gpu/v1" })).toContain("password");
    expect(err({ baseUrl: "http://gpu/v1?x=1" })).toContain("query");
    expect(err({ statusUrl: "javascript:alert(1)" })).toContain("http://");
    expect(err({ statusUrl: "https://s.lan/?p=1" })).toBeNull();
    expect(err({ contextWindow: "100" })).toContain("1024");
    expect(err({ contextWindow: "abc" })).toContain("whole number");
    expect(err({ contextWindow: "4096", maxOutputTokens: "8192" })).toContain("more than the context window");
    expect(err({ maxConcurrent: "0" })).toContain("1 to 1024");
    expect(err({ harnesses: ["claude", "copilot"] })).toBe("pick at least one harness");
    expect(err({ name: " " })).toBe("the name is empty");
  });

  it("parses models, counts, and filters harnesses by style", () => {
    expect(parseModels("a, b\n a ,,c")).toEqual(["a", "b", "c"]);
    expect(parseCount("x", "", 1)).toEqual({});
    expect(parseCount("x", "1M", 1)).toEqual({ value: 1_048_576 });
    expect(parseCount("x", "131_072", 1)).toEqual({ value: 131_072 });
    expect(styleHarnesses("openai", ["claude", "pi", "codex", "copilot"])).toEqual(["pi", "codex"]);
    expect(styleHarnesses("anthropic", ["claude", "pi", "codex", "copilot"])).toEqual(["claude", "pi"]);
    expect(urlError("u", "http://[::1]:8080/v1")).toBeNull();
    expect(urlError("u", "http://:80/v1")).toContain("no host");
    expect(wizardFromQuery("selfhosted")).toEqual({ path: "selfhosted" });
  });

  it("re-login on a self-hosted subscription stores its key", () => {
    expect(reloginFor({ name: "gpu", provider: "selfhosted", harnesses: ["pi"], has_api_key: false })).toEqual({ path: "api_key", setup: { name: "gpu", provider: "selfhosted" } });
    // A key for an existing server sends no empty base URL.
    expect(apiKeyBody({ provider: "selfhosted", key: "k", baseUrl: " " })).toEqual({ provider: "selfhosted", key: "k" });
  });
});

describe("the subscription row", () => {
  const s = (over: Partial<Subscription>) => over as Subscription;
  it("says the style, the streams, and the context", () => {
    expect(selfHostedSummary(s({ selfhosted: { api: "anthropic", base_url: "http://g", max_concurrent: 2, context_window: 131_072 } }))).toBe("self-hosted · anthropic · 2 streams · 128k ctx");
    expect(selfHostedSummary(s({ selfhosted: { api: "openai", base_url: "http://g", max_concurrent: 1, context_window: 200_000 } }))).toBe("self-hosted · openai · 1 stream · 200k ctx");
    expect(selfHostedSummary(s({}))).toBeNull();
    expect(streamsText(s({ streams: { max: 2, in_use: 1, waiting: 0 } }))).toBe("1/2 streams in use");
    expect(streamsText(s({ streams: { max: 2, in_use: 2, waiting: 3 } }))).toBe("2/2 streams in use · 3 waiting");
    expect(streamsText(s({}))).toBeNull();
    expect(tokensText(1_000_000)).toBe("1M");
    expect(tokensText(5000)).toBe("5k");
    expect(tokensText(1234)).toBe("1234");
  });

  it("says who a stop-all stopped", () => {
    expect(stopAllText("gpu", { stopped: [] }, false)).toBe("No agent on gpu was running.");
    expect(stopAllText("gpu", { stopped: [{ project: "p", name: "a" }, { project: "p", name: "b", error: "gone" }] }, true)).toBe("gpu is disabled. Stopping p/a. Not stopped: p/b (gone).");
  });
});

describe("the agent form and the agent badge", () => {
  const base = { harnesses: ["claude", "pi"], login_status: "api_key", has_api_key: false, windows: [], agents: [] } as unknown as Subscription;
  const gpu = { ...base, name: "gpu", provider: "selfhosted", selfhosted: { api: "anthropic", base_url: "http://g" } } as Subscription;
  const off = { ...base, name: "off", provider: "openrouter", harnesses: ["pi"], enabled: false } as Subscription;
  it("offers a self-hosted server to its style's harnesses, and no disabled subscription", () => {
    expect(compatibleSubscriptions([gpu, off], "claude").map((s) => s.name)).toEqual(["gpu"]);
    expect(compatibleSubscriptions([gpu, off], "pi").map((s) => s.name)).toEqual(["gpu"]);
    // A stray harness in the list: the style still decides.
    expect(compatibleSubscriptions([{ ...gpu, harnesses: ["codex", "copilot"] }], "codex")).toEqual([]);
    expect(selfHostedServes({ provider: "openai-compatible" }, "codex")).toBe(true);
    expect(selfHostedServes({ provider: "anthropic" }, "copilot")).toBe(true);
    const opts = subscriptionOptions([gpu, off], "pi", "off");
    expect(opts.at(-1)?.label).toContain("disabled");
  });

  it("shows a disabled subscription and a stream wait on the badge", () => {
    expect(lifecycleBadge({ status: "stopped", paused: false, status_reason: "subscription_disabled" }).label).toBe("stopped (subscription disabled)");
    expect(lifecycleBadge({ status: "idle", paused: false, status_reason: "stream_limit: waiting for a free stream on `gpu`" }).label).toBe("waiting for a free stream");
  });
});

describe("the mock's self-hosted subscriptions", () => {
  it("creates, validates, patches, and fetches models", async () => {
    const api = mock();
    const s = await api.subscriptions.create({ name: "gpu2", provider: "openai-compatible", selfhosted: { api: "anthropic", base_url: "http://gpu:8000/v1/", max_concurrent: 2 } });
    // The old word is the OpenAI style.
    expect(s).toMatchObject({ provider: "selfhosted", harnesses: ["pi", "codex"], enabled: true, no_quota: true, selfhosted: { api: "openai", base_url: "http://gpu:8000/v1" }, streams: { max: 2, in_use: 0, waiting: 0 } });
    await expect(api.subscriptions.create({ name: "gpu3", provider: "selfhosted", harnesses: ["copilot"], selfhosted: { api: "openai", base_url: "http://gpu/v1" } })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.create({ name: "gpu4", provider: "selfhosted" })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.create({ name: "gpu5", provider: "selfhosted", selfhosted: { api: "openai", base_url: "file:///etc" } })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.subscriptions.create({ name: "or2", provider: "openrouter", selfhosted: { api: "openai", base_url: "http://g/v1" } })).rejects.toMatchObject({ code: "invalid" });
    const p = await api.subscriptions.update("gpu2", { selfhosted: { api: "anthropic", base_url: "http://gpu:8000", context_window: 65_536, max_concurrent: null } });
    expect(p.selfhosted).toEqual({ api: "anthropic", base_url: "http://gpu:8000", context_window: 65_536 });
    expect(p.harnesses).toEqual(["pi"]);
    expect(p.streams).toBeUndefined();
    await expect(api.subscriptions.update("anthropic-max", { alert_at: 90 })).rejects.toMatchObject({ code: "file_managed" });
    const m = await api.subscriptions.fetchModels({ api: "openai", base_url: "http://gpu:8000/v1" });
    expect(m.models.map((x) => x.id)).toContain("qwen3-coder-30b");
    const e = await api.subscriptions.fetchModels({ base_url: "http://fail.lan/v1" }).catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 503, code: "unavailable" });
    // A stored key never goes to another host.
    await expect(api.subscriptions.fetchModels({ subscription: "gpu-lan", base_url: "http://evil.lan" })).rejects.toMatchObject({ code: "invalid" });
    expect((await api.subscriptions.models("gpu-lan")).source).toBe("subscription");
    expect(statusUrlOf("zai")).toBeUndefined();
    expect(statusUrlOf("moonshot")).toBe("https://status.moonshot.cn");
    expect(checkHttpUrl("u", "http://gpu:99999", false)).toEqual({ error: "u has a bad port `99999`" });
  });

  it("shows the seeded server, its streams, and the status pages", async () => {
    const api = mock();
    const list = await api.subscriptions.list();
    const gpu = list.find((s) => s.name === "gpu-lan")!;
    expect(gpu).toMatchObject({ streams: { max: 2, in_use: 1, waiting: 0 }, status_url: "http://gpu.lan:3001/status", no_quota: true });
    expect(list.find((s) => s.name === "anthropic-max")?.status_url).toBe("https://status.anthropic.com");
    expect(list.find((s) => s.name === "gemini")?.enabled).toBe(false);
  });

  it("refuses a disabled subscription for new agents and starts, and an unsupported harness", async () => {
    const api = mock();
    await expect(api.agents.create({ name: "g1", project: "terrarium", harness: "pi", subscription: "gemini" })).rejects.toMatchObject({ code: "invalid" });
    await expect(api.agents.create({ name: "g2", project: "terrarium", harness: "codex", subscription: "gpu-lan" })).rejects.toMatchObject({ code: "invalid", message: expect.stringContaining("claude and pi") });
    const a = await api.agents.create({ name: "g3", project: "terrarium", harness: "pi", subscription: "gpu-lan" });
    await api.subscriptions.disable("gpu-lan");
    await expect(api.agents.start(a.id)).rejects.toMatchObject({ code: "conflict" });
    await expect(api.agents.restart(a.id)).rejects.toMatchObject({ code: "conflict" });
    const on = await api.subscriptions.enable("gpu-lan");
    expect(on.enabled).toBe(true);
  });

  it("stops all and only the agents of the subscription", async () => {
    const api = mock();
    const a = await api.agents.create({ name: "s1", project: "terrarium", harness: "pi", subscription: "gpu-lan" });
    await api.agents.start(a.id);
    await new Promise((r) => setTimeout(r, 20));
    const before = (await api.agents.list()).filter((x) => x.subscription !== "gpu-lan" && ["idle", "working", "starting", "waiting", "blocked"].includes(x.status)).length;
    const r = await api.subscriptions.stopAll("gpu-lan", { disable: true });
    expect(r.stopped.map((x) => x.name)).toEqual(["s1"]);
    expect(r.subscription.enabled).toBe(false);
    await new Promise((res) => setTimeout(res, 20));
    expect((await api.agents.get(a.id)).status).toBe("stopped");
    const after = (await api.agents.list()).filter((x) => x.subscription !== "gpu-lan" && ["idle", "working", "starting", "waiting", "blocked"].includes(x.status)).length;
    expect(after).toBe(before);
  });
});

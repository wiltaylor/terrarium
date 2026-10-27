import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import type { ModelList, Subscription } from "../../api/types";
import {
  appliedText,
  changeSubscription,
  compatibleSubscriptions,
  defaultModelFor,
  modelSourceText,
  normalizePool,
  poolCandidates,
  prunePool,
  reconcileSubscription,
  samePool,
  subscriptionLabel,
  subscriptionOptions,
  swapTargets,
} from "./subscription";

const sub = (over: Partial<Subscription>): Subscription => ({
  name: "s",
  provider: "anthropic",
  harnesses: ["claude"],
  alert_at: 80,
  source: "api",
  login_status: "logged_in",
  probe_status: "none",
  created_at: "",
  updated_at: "",
  windows: [],
  local_estimate_only: true,
  has_api_key: false,
  held_agents: 0,
  default_models: {},
  agents: [],
  ...over,
});

const zai = sub({ name: "zai-key", provider: "zai", plan: "coding", harnesses: ["claude", "pi"], login_status: "api_key", has_api_key: true, default_models: { pi: "glm-5.3-flash", claude: "glm-5.3" } });
const max = sub({ name: "anthropic-max", plan: "Max 20x" });
const pro = sub({ name: "openai-pro", provider: "openai", plan: "pro", harnesses: ["codex"], login_status: "expired" });
const all = [max, pro, zai];

describe("subscription choice", () => {
  it("lists only the subscriptions of the harness", () => {
    expect(compatibleSubscriptions(all, "pi").map((s) => s.name)).toEqual(["zai-key"]);
    expect(compatibleSubscriptions(all, "codex").map((s) => s.name)).toEqual(["openai-pro"]);
    expect(compatibleSubscriptions(all, "claude").map((s) => s.name)).toEqual(["anthropic-max", "zai-key"]);
    expect(compatibleSubscriptions(all, "copilot")).toEqual([]);
  });

  it("labels name, provider, plan, and status", () => {
    expect(subscriptionLabel(zai)).toBe("zai-key — zai coding (API key)");
    expect(subscriptionLabel(max)).toBe("anthropic-max — anthropic Max 20x (logged in)");
    expect(subscriptionLabel(pro)).toBe("openai-pro — openai pro (expired)");
    // A plan that says "API key" is not said twice.
    expect(subscriptionLabel(sub({ name: "or", provider: "openrouter", plan: "API key", has_api_key: true }))).toBe("or — openrouter (API key)");
  });

  it("offers none and the compatible subscriptions, and keeps an incompatible current one visible", () => {
    expect(subscriptionOptions(all, "pi", "").map((o) => o.value)).toEqual(["", "zai-key"]);
    expect(subscriptionOptions(all, "codex", "").map((o) => o.value)).toEqual(["", "openai-pro"]);
    const kept = subscriptionOptions(all, "pi", "anthropic-max");
    expect(kept.at(-1)).toEqual({ value: "anthropic-max", label: "anthropic-max — anthropic Max 20x (logged in) — not for pi" });
    expect(subscriptionOptions(all, "pi", "gone").at(-1)).toEqual({ value: "gone", label: "gone — not found" });
  });

  it("clears an incompatible choice on a harness change, with a note", () => {
    expect(reconcileSubscription("zai-key", "pi", all)).toEqual({ subscription: "zai-key", note: null });
    const r = reconcileSubscription("zai-key", "codex", all);
    expect(r.subscription).toBe("");
    expect(r.note).toContain("zai-key is not for codex");
    expect(r.note).toContain("claude, pi");
    // Not loaded yet, or no choice: nothing changes.
    expect(reconcileSubscription("zai-key", "codex", [])).toEqual({ subscription: "zai-key", note: null });
    expect(reconcileSubscription("", "codex", all)).toEqual({ subscription: "", note: null });
  });

  it("reads the default model of the harness", () => {
    expect(defaultModelFor(zai, "pi")).toBe("glm-5.3-flash");
    expect(defaultModelFor(zai, "claude")).toBe("glm-5.3");
    expect(defaultModelFor(max, "claude")).toBeUndefined();
    expect(defaultModelFor(undefined, "pi")).toBeUndefined();
    // An older daemon sends no default_models.
    expect(defaultModelFor({ ...zai, default_models: undefined }, "pi")).toBeNull();
  });
});

describe("model source", () => {
  it("names each source", () => {
    const l = (source: ModelList["source"]): ModelList => ({ source, fetched_at: "", models: [] });
    expect(modelSourceText(l("provider"))).toBe("from the provider's model list");
    expect(modelSourceText(l("plan"))).toBe("from the harness login's model list");
    expect(modelSourceText(l("pi"))).toBe("from pi's model catalogue");
    expect(modelSourceText(l("fallback"))).toMatch(/default table/);
  });
});

describe("the agent page's subscription edit", () => {
  it("patches the subscription, and null clears it", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const a = await api.agents.create({ name: "est", project: "terrarium", harness: "pi" });
    const set = await changeSubscription(api, a.id, "zai-key");
    expect(set.subscription).toBe("zai-key");
    const shown = await api.agents.get(a.id);
    expect(shown.subscription_detail).toEqual({ name: "zai-key", provider: "zai", plan: "coding", login_status: "api_key", harnesses: ["claude", "pi"], default_model: "glm-5.3-flash" });
    const cleared = await changeSubscription(api, a.id, " ");
    expect(cleared.subscription).toBeUndefined();
    expect((await api.agents.get(a.id)).subscription_detail).toBeUndefined();
  });
});

describe("subscription pool", () => {
  const max2 = sub({ name: "anthropic-max-2", plan: "Max 20x", harnesses: ["claude", "pi"] });
  const key = sub({ name: "anthropic-key", harnesses: ["pi"], has_api_key: true });
  const list = [max, max2, key, pro, zai];

  it("offers the subscriptions of the same provider that the harness can spend", () => {
    expect(poolCandidates(list, "claude", "anthropic-max").map((s) => s.name)).toEqual(["anthropic-max", "anthropic-max-2"]);
    expect(poolCandidates(list, "pi", "anthropic-max-2").map((s) => s.name)).toEqual(["anthropic-max-2", "anthropic-key"]);
    expect(poolCandidates(list, "claude", "")).toEqual([]);
    expect(poolCandidates(list, "claude", "gone")).toEqual([]);
  });

  it("puts the current subscription first and drops repeats", () => {
    expect(normalizePool(["anthropic-max-2", "anthropic-max-2", " "], "anthropic-max")).toEqual(["anthropic-max", "anthropic-max-2"]);
    expect(normalizePool(["anthropic-max", "anthropic-max-2"], "anthropic-max-2")).toEqual(["anthropic-max-2", "anthropic-max"]);
    // Only the current subscription: nothing to swap to, so no pool.
    expect(normalizePool(["anthropic-max"], "anthropic-max")).toEqual([]);
    expect(normalizePool([], "anthropic-max")).toEqual([]);
  });

  it("keeps only the candidates after a change of harness or subscription", () => {
    const pool = ["anthropic-max", "anthropic-max-2"];
    expect(prunePool(pool, list, "claude", "anthropic-max-2")).toEqual(["anthropic-max-2", "anthropic-max"]);
    expect(prunePool(pool, list, "codex", "openai-pro")).toEqual([]);
    expect(prunePool(pool, list, "claude", "")).toEqual([]);
    // The list is not loaded yet: no change.
    expect(prunePool(pool, [], "codex", "openai-pro")).toBe(pool);
  });

  it("compares pools in order", () => {
    expect(samePool(["a", "b"], ["a", "b"])).toBe(true);
    expect(samePool(["a", "b"], ["b", "a"])).toBe(false);
    expect(samePool([], ["a"])).toBe(false);
  });

  it("lists the swap targets: the pool without the current subscription", () => {
    expect(swapTargets({ subscription: "anthropic-max", subscription_pool: ["anthropic-max", "anthropic-max-2"] })).toEqual(["anthropic-max-2"]);
    expect(swapTargets({ subscription: "anthropic-max" })).toEqual([]);
  });

  it("says how a swap applied", () => {
    expect(appliedText({ to: "b", applied: "live" })).toBe("Moved to b. The harness uses the new login at its next request. No restart.");
    expect(appliedText({ to: "b", applied: "restart_scheduled" })).toBe("Moved to b. The agent restarts with resume when the turn ends.");
    expect(appliedText({ to: "b", applied: "restarted" })).toBe("Moved to b. The agent restarted with resume.");
    expect(appliedText({ to: "b", applied: "next_start" })).toBe("Moved to b. The agent does not run. Its next start uses the new account.");
  });
});

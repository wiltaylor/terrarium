// Default models and model lists in the mock (docs/api.md §16.1, §16.3).
// The daemon side is `crates/terrariumd/src/models.rs`, with the pi
// adapter's `flags::DEFAULT_MODELS` and `provider::provider_for`.

import type { AgentSubscriptionDetail, Harness, ModelList, Subscription } from "../types";
import { isZai } from "./zai";

/** pi's default model by Terrarium provider, for an API key and for a plan login. A subset of the adapter table. */
const PI_DEFAULTS: Record<string, { key?: string; plan?: string }> = {
  zai: { key: "glm-5.3-flash", plan: "glm-5.3-flash" },
  openai: { key: "gpt-6-luna", plan: "gpt-6-luna" },
  openrouter: { key: "openai/gpt-6-luna" },
  google: { key: "gemini-3.1-flash-lite" },
  github: { plan: "gpt-6-luna" },
  anthropic: { key: "claude-haiku-5-5" },
};

const isKey = (s: Pick<Subscription, "plan" | "login_status">) => s.login_status === "api_key" || (s.plan ?? "").trim().toLowerCase() === "api key";

/** `default_models` of a subscription: pi (no entry when pi refuses a Claude plan) and Claude Code on Z.ai. */
export function defaultModels(s: Pick<Subscription, "provider" | "plan" | "harnesses" | "login_status">): Partial<Record<Harness, string>> {
  const out: Partial<Record<Harness, string>> = {};
  const provider = s.provider.trim().toLowerCase();
  if (s.harnesses.includes("pi")) {
    const row = PI_DEFAULTS[provider];
    const m = isZai(provider) ? row?.key : isKey(s) ? row?.key : row?.plan;
    if (m) out.pi = m;
  }
  if (s.harnesses.includes("claude") && isZai(provider)) out.claude = "glm-5.3";
  return out;
}

/** `subscription_detail` of `GET /api/agents/{id}`. */
export function subscriptionDetail(s: Subscription, harness: Harness): AgentSubscriptionDetail {
  const m = defaultModels(s)[harness];
  return { name: s.name, provider: s.provider, ...(s.plan ? { plan: s.plan } : {}), login_status: s.login_status, harnesses: s.harnesses, ...(m ? { default_model: m } : {}) };
}

/** What Z.ai's `/models` answered on 2026-10-26 (docs/compat.md, "Z.ai models"). */
export const ZAI_PROVIDER_MODELS = ["glm-4.5", "glm-4.5-air", "glm-4.6", "glm-4.7", "glm-5", "glm-5-turbo", "glm-5.1", "glm-5.2", "glm-5.3", "glm-5.3-flash", "glm-5.3-flashx"];

/** A slice of pi's catalogue by Terrarium provider (`pi --offline --list-models`). */
const PI_CATALOGUE: Record<string, { id: string; context: number }[]> = {
  openrouter: [
    { id: "openai/gpt-6-luna", context: 400_000 },
    { id: "~anthropic/claude-haiku-latest", context: 1_000_000 },
    { id: "~anthropic/claude-sonnet-latest", context: 1_000_000 },
  ],
  openai: [
    { id: "gpt-6-luna", context: 400_000 },
    { id: "gpt-6-sol", context: 400_000 },
  ],
};

/** `GET /api/subscriptions/{name}/models`: the provider for a Z.ai key, else pi's catalogue, else the default table. */
export function modelList(s: Subscription, hasKey: boolean, at: string): ModelList {
  const provider = s.provider.trim().toLowerCase();
  if (isZai(provider) && hasKey) return { models: ZAI_PROVIDER_MODELS.map((id) => ({ id })), source: "provider", fetched_at: at };
  const pi = PI_CATALOGUE[provider];
  if (pi) return { models: pi.map((m) => ({ ...m })), source: "pi", fetched_at: at };
  const ids = [...new Set(Object.values(defaultModels(s)))];
  return { models: ids.map((id) => ({ id })), source: "fallback", fetched_at: at, notes: [`pi lists no model for provider \`${s.provider}\``] };
}

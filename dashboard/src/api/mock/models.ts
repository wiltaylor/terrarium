// Default models and model lists in the mock (docs/api.md §16.1, §16.3).
// The daemon side is `crates/terrariumd/src/models.rs`, with the pi
// adapter's `flags::DEFAULT_MODELS` and `provider::provider_for`.

import type { AgentSubscriptionDetail, Harness, ModelInfo, ModelList, Subscription } from "../types";
import { isZai } from "./zai";
import { isMoonshot, moonshotDefaultModel, moonshotModels } from "./moonshot";

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
    const m = isMoonshot(provider) ? moonshotDefaultModel(s.plan) : isZai(provider) ? row?.key : isKey(s) ? row?.key : row?.plan;
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

/** What the plan logins listed on 2026-10-26 (docs/compat.md, "Model lists (plan logins)"), a slice. */
export const PLAN_MODELS: Record<"claude" | "codex" | "copilot", ModelInfo[]> = {
  claude: [
    { id: "claude-haiku-5-5", name: "Claude Haiku 5.5", context: 1_000_000 },
    { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5", context: 1_000_000 },
    { id: "claude-opus-5-5", name: "Claude Opus 5.5", context: 1_000_000 },
    { id: "claude-fable-5-1", name: "Claude Fable 5.1", context: 1_000_000 },
  ],
  codex: [
    { id: "gpt-6.1-sol", name: "GPT-6.1-Sol", context: 500_000 },
    { id: "gpt-6-astra", name: "GPT-6-Astra", context: 272_000 },
    { id: "gpt-6-sol", name: "GPT-6-Sol", context: 272_000 },
    { id: "gpt-6-luna", name: "GPT-6-Luna", context: 272_000 },
  ],
  copilot: [
    { id: "claude-sonnet-4.6", name: "Claude Sonnet 4.6", context: 264_000 },
    { id: "gpt-5.3-codex", name: "GPT-5.3-Codex", context: 400_000 },
    { id: "mai-code-1.1-flash", name: "MAI-Code-1.1-Flash", context: 256_000 },
    { id: "claude-haiku-4.5", name: "Claude Haiku 4.5", context: 144_000 },
  ],
};

/** The plan list of a subscription, as `crate::models::plan::kind` picks it. */
function planKind(s: Subscription, provider: string, hasKey: boolean): keyof typeof PLAN_MODELS | null {
  if (provider === "anthropic" && (s.harnesses.includes("claude") || hasKey)) return "claude";
  if ((provider === "openai" || provider === "openai-codex") && s.harnesses.includes("codex") && !hasKey) return "codex";
  if (["github", "github-copilot", "copilot"].includes(provider) && s.harnesses.includes("copilot")) return "copilot";
  return null;
}

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

/**
 * `GET /api/subscriptions/{name}/models`: the provider for a Z.ai key, else
 * the plan login (the mock takes a missing or expired login as a failure),
 * else pi's catalogue, else the default table.
 */
export function modelList(s: Subscription, hasKey: boolean, at: string): ModelList {
  const provider = s.provider.trim().toLowerCase();
  if (isZai(provider) && hasKey) return { models: ZAI_PROVIDER_MODELS.map((id) => ({ id })), source: "provider", fetched_at: at };
  const plan = planKind(s, provider, hasKey);
  if (plan && s.login_status !== "missing" && s.login_status !== "expired") return { models: PLAN_MODELS[plan].map((m) => ({ ...m })), source: "plan", fetched_at: at };
  const pi = PI_CATALOGUE[provider];
  if (pi) return { models: pi.map((m) => ({ ...m })), source: "pi", fetched_at: at };
  const ids = [...new Set([...Object.values(defaultModels(s)), ...(isMoonshot(provider) ? moonshotModels(s.plan) : [])])];
  return { models: ids.map((id) => ({ id })), source: "fallback", fetched_at: at, notes: [`pi lists no model for provider \`${s.provider}\``] };
}

// The mock's copy of the daemon's display name table
// (`terrarium_core::providers::display_name`, docs/compat.md "Display
// names"): the vendor's own product and plan name. Keep the two in step.

import type { Subscription } from "../types";

const claude = (raw: string): string | undefined => {
  const k = raw.toLowerCase().replace(/[^a-z0-9]/g, "").replace(/^defaultclaude/, "").replace(/^claude/, "");
  if (k.includes("ultra") || k.includes("max20x")) return "Ultra";
  if (k.startsWith("max")) return "Max";
  return ({ pro: "Pro", free: "Free", team: "Team", enterprise: "Enterprise" } as Record<string, string>)[k];
};
const chatgpt: Record<string, string> = { free: "Free", go: "Go", plus: "Plus", pro: "Pro", team: "Business", business: "Business", edu: "Edu", enterprise: "Enterprise" };
const copilot: Record<string, string> = { free: "Free", pro: "Pro", "pro+": "Pro+", max: "Max", business: "Business", enterprise: "Enterprise" };
const glm: Record<string, string> = { lite: "Lite", pro: "Pro", max: "Max" };

const isKey = (plan?: string) => ["api key", "api", "api-cn"].includes((plan ?? "").trim().toLowerCase());
const join = (product: string, tier?: string) => (tier ? `${product} ${tier}` : product);

/** `Claude Ultra`, `ChatGPT Pro`, `GitHub Copilot`, `GLM Coding Plan Max`, `Kimi Code`, `OpenRouter`, `Self-hosted <host>`. */
export function mockDisplayName(s: Pick<Subscription, "name" | "provider" | "plan" | "windows"> & { selfhosted?: { base_url?: string } }): string {
  const p = s.provider.trim().toLowerCase();
  const plan = s.plan?.trim() || undefined;
  if (p === "selfhosted" || p === "openai-compatible") {
    const host = s.selfhosted?.base_url?.replace(/^[a-z]+:\/\//i, "").split(/[/?#]/)[0];
    return `Self-hosted ${host || s.name}`;
  }
  switch (p) {
    case "anthropic":
      return isKey(plan) ? "Claude API" : join("Claude", plan && claude(plan));
    case "openai":
      return isKey(plan) ? "OpenAI API" : join("ChatGPT", plan && chatgpt[plan.toLowerCase()]);
    case "github":
      return join("GitHub Copilot", plan && copilot[plan.toLowerCase()]);
    case "zai": {
      if (isKey(plan)) return "Z.ai";
      const level = s.windows?.map((w) => (w as { raw?: { level?: string } }).raw?.level).find(Boolean);
      return join("GLM Coding Plan", level && glm[level.toLowerCase()]);
    }
    case "moonshot":
      return isKey(plan) ? "Kimi API Platform" : "Kimi Code";
    case "openrouter":
      return "OpenRouter";
    default:
      return plan ? `${s.provider} ${plan}` : s.provider || s.name;
  }
}

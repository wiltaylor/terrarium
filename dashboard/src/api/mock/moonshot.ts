// Moonshot AI (Kimi) API-key subscriptions in the mock (docs/api.md §16.5).
// The daemon side is `terrarium_core::moonshot` and `api/subscriptions.rs`.
// A Moonshot subscription is for pi only.

import type { ApiKeyInput, Harness, LoginStatus } from "../types";

export const MOONSHOT_KEY_ENV = "MOONSHOT_API_KEY";
export const MOONSHOT_PLANS = ["coding", "coding-cn", "api", "api-cn"] as const;
export type MoonshotPlan = (typeof MOONSHOT_PLANS)[number];

export const isMoonshot = (provider: string | undefined): boolean =>
  (provider ?? "").trim().toLowerCase() === "moonshot";

/** The plan of a Moonshot subscription, or an error text as the daemon gives it. No plan is `coding`. */
export function moonshotPlan(
  plan: string | undefined,
): { plan: MoonshotPlan } | { error: string } {
  const p = (plan ?? "").trim().toLowerCase();
  if (!p) return { plan: "coding" };
  if ((MOONSHOT_PLANS as readonly string[]).includes(p)) return { plan: p as MoonshotPlan };
  return {
    error: `unknown moonshot plan \`${(plan ?? "").trim()}\`; use \`coding\` (Kimi Code), \`coding-cn\` (Kimi Code, China), \`api\` (Open Platform, pay as you go), or \`api-cn\` (Open Platform, China)`,
  };
}

/** Why a Moonshot subscription cannot list `harnesses`, or null. Only pi can spend it. */
export function moonshotHarnessError(name: string, harnesses: Harness[]): string | null {
  const other = harnesses.filter((h) => h !== "pi");
  return other.length
    ? `subscription \`${name}\`: a moonshot subscription is for the pi harness only, not ${other.join(", ")}; set harnesses to ["pi"]`
    : null;
}

/** The balance probe of an Open Platform plan; Kimi Code has none. */
export function moonshotProbe(plan: string | undefined): string | undefined {
  const r = moonshotPlan(plan);
  if (!("plan" in r)) return undefined;
  if (r.plan === "api") return "moonshot-balance";
  if (r.plan === "api-cn") return "moonshot-cn-balance";
  return undefined;
}

/** pi's model when the agent sets none. */
export function moonshotDefaultModel(plan: string | undefined): string {
  const r = moonshotPlan(plan);
  return "plan" in r && r.plan.startsWith("api") ? "kimi-k3" : "kimi-for-coding";
}

/** The documented model ids of a plan, the default first (the daemon's fallback table). */
export function moonshotModels(plan: string | undefined): string[] {
  const coding = ["k3.1-pro", "k3.1-pro-highspeed", "k3", "k3-256k", "kimi-for-coding", "kimi-for-coding-highspeed"];
  const api = ["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"];
  const def = moonshotDefaultModel(plan);
  const all = def.startsWith("kimi-k") ? api : coding;
  return [def, ...all.filter((m) => m !== def)];
}

/** What a new moonshot key subscription gets. */
export function moonshotKeyDefaults(input: ApiKeyInput): {
  plan: string;
  harnesses: Harness[];
  login_status: LoginStatus;
  probe?: string;
} {
  const r = moonshotPlan(input.plan);
  const plan = "plan" in r ? r.plan : "coding";
  const probe = moonshotProbe(plan);
  return { plan, harnesses: ["pi"], login_status: "api_key", ...(probe ? { probe } : {}) };
}

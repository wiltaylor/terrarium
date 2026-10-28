// Z.ai (GLM) API-key subscriptions in the mock (docs/api.md §16.5). The
// daemon side is `terrarium_core::zai` and `api/subscriptions.rs`.

import type { ApiKeyInput, Harness, LoginStatus } from "../types";
import { isMoonshot, moonshotPlan } from "./moonshot";

export const ZAI_KEY_ENV = "ZAI_API_KEY";
export const ZAI_PLANS = ["coding", "api"] as const;

export const isZai = (provider: string | undefined): boolean =>
  (provider ?? "").trim().toLowerCase() === "zai";

/** The plan of a Z.ai subscription, or an error text as the daemon gives it. No plan is `coding`. */
export function zaiPlan(
  plan: string | undefined,
): { plan: string } | { error: string } {
  const p = (plan ?? "").trim().toLowerCase();
  if (!p) return { plan: "coding" };
  if ((ZAI_PLANS as readonly string[]).includes(p)) return { plan: p };
  return {
    error: `unknown zai plan \`${plan}\`; use \`coding\` (GLM Coding Plan) or \`api\` (pay as you go)`,
  };
}

/** Check the plan of a key body: zai takes `coding` or `api`; moonshot takes `coding`, `coding-cn`, `api` or `api-cn`; other providers take none. Returns an error text. */
export function checkKeyPlan(input: ApiKeyInput): string | null {
  if (isZai(input.provider)) {
    const r = zaiPlan(input.plan);
    return "error" in r ? r.error : null;
  }
  if (isMoonshot(input.provider)) {
    const r = moonshotPlan(input.plan);
    return "error" in r ? r.error : null;
  }
  return input.plan !== undefined
    ? `\`plan\` is only for providers zai and moonshot, not \`${input.provider}\``
    : null;
}

/** What a new zai key subscription gets. */
export function zaiKeyDefaults(input: ApiKeyInput): {
  plan: string;
  harnesses: Harness[];
  login_status: LoginStatus;
} {
  const r = zaiPlan(input.plan);
  return {
    plan: "plan" in r ? r.plan : "coding",
    harnesses: ["claude", "pi"],
    login_status: "api_key",
  };
}

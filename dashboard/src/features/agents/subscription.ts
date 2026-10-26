// Which subscriptions an agent can use, and how the agent form shows them.
// Pure: AgentForm.tsx and the agent page call these.

import type { Api } from "../../api/client";
import type { Agent, Harness, LoginStatus, ModelList, Subscription } from "../../api/types";

/** The subscriptions whose `harnesses` include `harness`. The daemon refuses a start on any other. */
export function compatibleSubscriptions(list: Subscription[], harness: Harness): Subscription[] {
  return list.filter((s) => s.harnesses.includes(harness));
}

/** The login status in short words. */
export function loginText(status: LoginStatus, hasKey = false): string {
  if (status === "api_key" || hasKey) return "API key";
  switch (status) {
    case "logged_in":
      return "logged in";
    case "expiring":
      return "logged in, expiring";
    case "expired":
      return "expired";
    case "missing":
      return "not logged in";
    default:
      return "not checked";
  }
}

/** `name — provider plan (API key|logged in|expired)`. A plan that only says "API key" is not said twice. */
export function subscriptionLabel(s: Pick<Subscription, "name" | "provider" | "plan" | "login_status"> & { has_api_key?: boolean }): string {
  const status = loginText(s.login_status, s.has_api_key);
  const plan = s.plan && s.plan.toLowerCase() !== status.toLowerCase() ? ` ${s.plan}` : "";
  return `${s.name} — ${s.provider}${plan} (${status})`;
}

/**
 * The model the daemon gives `harness` on `sub` when the agent sets none.
 * `undefined`: no subscription, or the daemon picks no model (the harness
 * keeps its own default). `null`: the daemon is too old to say.
 */
export function defaultModelFor(sub: Subscription | undefined, harness: Harness): string | null | undefined {
  if (!sub) return undefined;
  if (!sub.default_models) return null;
  return sub.default_models[harness];
}

export interface SubscriptionChoice {
  /** The subscription to keep in the form: the same name, or "" when cleared. */
  subscription: string;
  /** Why the choice was cleared, to show under the field. */
  note: string | null;
}

/**
 * After a harness change: keep the chosen subscription when the new harness
 * can spend it, else clear it with a note. A name that is not in the list
 * stays (the list may not be loaded yet, or the daemon will say why).
 */
export function reconcileSubscription(chosen: string, harness: Harness, list: Subscription[]): SubscriptionChoice {
  const name = chosen.trim();
  const sub = name ? list.find((s) => s.name === name) : undefined;
  if (!sub || sub.harnesses.includes(harness)) return { subscription: chosen, note: null };
  return {
    subscription: "",
    note: `Subscription ${name} is not for ${harness} (it is for ${sub.harnesses.join(", ") || "no harness"}). The form cleared it.`,
  };
}

/**
 * The options of the subscription select: "none", the compatible
 * subscriptions, and the current value when it is not one of them (so an
 * edit does not change it without a word).
 */
export function subscriptionOptions(list: Subscription[], harness: Harness, current: string): { value: string; label: string }[] {
  const fit = compatibleSubscriptions(list, harness);
  const out = [{ value: "", label: "none" }, ...fit.map((s) => ({ value: s.name, label: subscriptionLabel(s) }))];
  const name = current.trim();
  if (name && !fit.some((s) => s.name === name)) {
    const known = list.find((s) => s.name === name);
    out.push({ value: name, label: known ? `${subscriptionLabel(known)} — not for ${harness}` : `${name} — not found` });
  }
  return out;
}

/** Where a model list came from, for the hint under the field. */
export function modelSourceText(list: ModelList): string {
  if (list.source === "provider") return "from the provider's model list";
  if (list.source === "plan") return "from the harness login's model list";
  if (list.source === "pi") return "from pi's model catalogue";
  return "from Terrarium's default table (the provider, the login, and pi listed none)";
}

/**
 * The Edit control of the agent page: `PATCH /api/agents/{id}` with the new
 * subscription, or `null` for none. The daemon uses it at the next start.
 */
export function changeSubscription(api: Pick<Api, "agents">, id: string, name: string): Promise<Agent> {
  const next = name.trim();
  return api.agents.update(id, { subscription: next || null });
}

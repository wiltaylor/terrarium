// Which subscriptions an agent can use, and how the agent form shows them.
// Pure: AgentForm.tsx and the agent page call these.

import type { Api } from "../../api/client";
import type { Agent, Harness, LoginStatus, ModelList, Subscription, SwapResult } from "../../api/types";

/**
 * The subscriptions whose `harnesses` include `harness`. The daemon refuses a start on any other.
 * pi uses API keys only (PLAN §16.3): the daemon never lists pi on a plan, and a subscription with
 * a plan account is never one for pi.
 */
export function compatibleSubscriptions(list: Subscription[], harness: Harness): Subscription[] {
  return list.filter((s) => s.harnesses.includes(harness) && (harness !== "pi" || !s.account));
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
    const why = harness === "pi" ? "not for pi, which uses API keys only" : `not for ${harness}`;
    out.push({ value: name, label: known ? `${subscriptionLabel(known)} — ${why}` : `${name} — not found` });
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

// ----- The subscription pool (PLAN §16.3) -----

/**
 * The subscriptions the pool of an agent may hold: the same provider as
 * the current subscription, and `harness` can spend each. Empty when the
 * current subscription is not in the list.
 */
export function poolCandidates(list: Subscription[], harness: Harness, current: string): Subscription[] {
  const cur = list.find((s) => s.name === current.trim());
  if (!cur) return [];
  return list.filter((s) => s.provider === cur.provider && s.harnesses.includes(harness));
}

/**
 * The pool to save: no repeats, the current subscription first. A pool
 * with only the current subscription is empty: there is nothing to swap to.
 */
export function normalizePool(pool: string[], current: string): string[] {
  const cur = current.trim();
  const names = [...new Set(pool.map((n) => n.trim()).filter(Boolean))];
  if (!names.some((n) => n !== cur)) return [];
  return cur ? [cur, ...names.filter((n) => n !== cur)] : names;
}

/**
 * The pool after a change of harness or subscription: keep only the
 * candidates. An empty list (not loaded yet) keeps the pool as it is.
 */
export function prunePool(pool: string[], list: Subscription[], harness: Harness, current: string): string[] {
  if (!list.length || !pool.length) return pool;
  const fit = new Set(poolCandidates(list, harness, current).map((s) => s.name));
  return normalizePool(pool.filter((n) => fit.has(n)), current);
}

/** The same pool: the same names in the same order. */
export const samePool = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/** The pool members that a swap can go to: all but the current subscription. */
export function swapTargets(agent: Pick<Agent, "subscription" | "subscription_pool">): string[] {
  return (agent.subscription_pool ?? []).filter((n) => n !== agent.subscription);
}

/** What `POST /api/agents/{id}/subscription` did, in one short text. */
export function appliedText(r: Pick<SwapResult, "to" | "applied">): string {
  switch (r.applied) {
    case "live":
      return `Moved to ${r.to}. The harness uses the new login at its next request. No restart.`;
    case "restart_scheduled":
      return `Moved to ${r.to}. The agent restarts with resume when the turn ends.`;
    case "restarted":
      return `Moved to ${r.to}. The agent restarted with resume.`;
    case "next_start":
      return `Moved to ${r.to}. The agent does not run. Its next start uses the new account.`;
  }
}

// ----- The account of an agent with no subscription (PLAN §16.3) -----

export type NoSubscriptionAccount = { name: string; why: "default" | "only" } | { error: string } | null;

/**
 * The plan account that a `harness` agent with no subscription runs on, as
 * the daemon picks it: the default account of the harness, else the one
 * account. Two or more and no default: the start fails. `null`: no account
 * (pi, or no plan account of the harness).
 */
export function noSubscriptionAccount(list: Subscription[], harness: Harness): NoSubscriptionAccount {
  if (harness === "pi") return null;
  const kin = list.filter((s) => s.account?.harness === harness && s.account.has_login && !s.has_api_key);
  const def = kin.find((s) => s.default);
  if (def) return { name: def.name, why: "default" };
  if (kin.length === 1) return { name: kin[0]!.name, why: "only" };
  if (kin.length > 1) return { error: `the ${harness} accounts (${kin.map((s) => s.name).join(", ")}) have no default: the agent cannot start` };
  return null;
}

/** `account: anthropic-max-20x (default)`. */
export function noSubscriptionAccountText(a: NonNullable<NoSubscriptionAccount>): string {
  if ("error" in a) return a.error;
  return `account: ${a.name} (${a.why === "default" ? "default" : "the only account"})`;
}

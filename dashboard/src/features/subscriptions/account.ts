// The plan account of a subscription (PLAN §16.3) in short words, for the
// cards of the Subscriptions page. Pure.

import type { PlanAccount, Subscription } from "../../api/types";

/** Where the account came from. */
export function accountSourceText(source: PlanAccount["source"]): string {
  return source === "migration" ? "moved from the shared home" : "logged in with the wizard";
}

/** `claude account · wil@example.com · moved from the shared home`. */
export function accountText(a: PlanAccount): string {
  const login = a.has_login ? (a.login ?? "login name not known") : "no login";
  return `${a.harness} account · ${login} · ${accountSourceText(a.source)}`;
}

/** PLAN §16.3: the harnesses with a default plan account. pi names its provider with a subscription. */
export const DEFAULT_HARNESSES: readonly string[] = ["claude", "codex", "copilot"];

/** A plan account that can be the default of its harness: Claude Code, Codex, or Copilot, and no API key. */
export function canBeDefault(s: Subscription): boolean {
  return !!s.account && DEFAULT_HARNESSES.includes(s.account.harness) && !s.has_api_key && !s.env_var;
}

export interface DefaultInfo {
  /** The card gets the "default" badge. */
  isDefault: boolean;
  /** The card gets the "Make default" button. */
  canMakeDefault: boolean;
  /** Two or more accounts of the harness and none is the default: agents with no subscription cannot start. */
  noDefault: boolean;
}

/**
 * The default state of the card of `s` among `list`. An older daemon sends
 * no `default`: then the page shows nothing.
 */
export function defaultInfo(list: Subscription[], s: Subscription): DefaultInfo {
  const none: DefaultInfo = { isDefault: false, canMakeDefault: false, noDefault: false };
  if (s.default === undefined || !canBeDefault(s)) return none;
  const kin = list.filter((x) => canBeDefault(x) && x.account!.harness === s.account!.harness);
  return {
    isDefault: s.default,
    canMakeDefault: !s.default && s.account!.has_login,
    noDefault: kin.length >= 2 && !kin.some((x) => x.default),
  };
}

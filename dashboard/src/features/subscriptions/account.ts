// The plan account of a subscription (PLAN §16.3) in short words, for the
// cards of the Subscriptions page. Pure.

import type { PlanAccount } from "../../api/types";

/** Where the account came from. */
export function accountSourceText(source: PlanAccount["source"]): string {
  return source === "migration" ? "moved from the shared home" : "logged in with the wizard";
}

/** `claude account · wil@example.com · moved from the shared home`. */
export function accountText(a: PlanAccount): string {
  const login = a.has_login ? (a.login ?? "login name not known") : "no login";
  return `${a.harness} account · ${login} · ${accountSourceText(a.source)}`;
}

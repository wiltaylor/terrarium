import { describe, expect, it } from "vitest";
import type { PlanAccount, Subscription } from "../../api/types";
import { accountSourceText, accountText, canBeDefault, defaultInfo } from "./account";

const acct = (over: Partial<PlanAccount>): PlanAccount => ({ harness: "claude", has_login: true, source: "login", created_at: "", updated_at: "", ...over });

describe("plan account text", () => {
  it("says where the account came from", () => {
    expect(accountSourceText("migration")).toBe("moved from the shared home");
    expect(accountSourceText("login")).toBe("logged in with the wizard");
  });

  it("names the harness, the login, and the source", () => {
    expect(accountText(acct({ login: "wil@example.com", source: "migration" }))).toBe("claude account · wil@example.com · moved from the shared home");
    expect(accountText(acct({ harness: "codex" }))).toBe("codex account · login name not known · logged in with the wizard");
    expect(accountText(acct({ has_login: false, login: "old@example.com" }))).toBe("claude account · no login · logged in with the wizard");
  });
});

const sub = (name: string, harness: PlanAccount["harness"] | null, over: Partial<Subscription> = {}): Subscription => ({
  name,
  provider: "anthropic",
  harnesses: harness ? [harness] : ["pi"],
  alert_at: 80,
  source: "api",
  login_status: "logged_in",
  probe_status: "ok",
  created_at: "",
  updated_at: "",
  windows: [],
  local_estimate_only: false,
  has_api_key: false,
  held_agents: 0,
  agents: [],
  default: false,
  ...(harness ? { account: acct({ harness }) } : {}),
  ...over,
});

describe("the default account (PLAN §16.3)", () => {
  it("badges the default and offers Make default on the other accounts of the harness", () => {
    const a = sub("max-a", "claude", { default: true });
    const b = sub("max-b", "claude");
    const pro = sub("pro", "codex");
    const list = [a, b, pro];
    expect(defaultInfo(list, a)).toEqual({ isDefault: true, canMakeDefault: false, noDefault: false });
    expect(defaultInfo(list, b)).toEqual({ isDefault: false, canMakeDefault: true, noDefault: false });
    // One codex account and no default: the start uses it, so no warning.
    expect(defaultInfo(list, pro)).toEqual({ isDefault: false, canMakeDefault: true, noDefault: false });
  });

  it("warns on each account of a harness with two or more accounts and no default", () => {
    const b = sub("max-b", "claude");
    const c = sub("max-c", "claude");
    expect(defaultInfo([b, c], b).noDefault).toBe(true);
    expect(defaultInfo([b, c], c).noDefault).toBe(true);
  });

  it("offers nothing for pi, an API key, a logged-out account, or an older daemon", () => {
    const pi = sub("pi-a", "pi");
    const key = sub("glm", null, { provider: "zai", has_api_key: true, env_var: "ZAI_API_KEY" });
    const out = sub("max-x", "claude", { account: acct({ has_login: false }) });
    const old = sub("max-o", "claude", { default: undefined });
    for (const s of [pi, key, old]) expect(defaultInfo([s], s)).toEqual({ isDefault: false, canMakeDefault: false, noDefault: false });
    expect(defaultInfo([out], out).canMakeDefault).toBe(false);
    expect(canBeDefault(pi)).toBe(false);
    expect(canBeDefault(key)).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import type { PlanAccount } from "../../api/types";
import { accountSourceText, accountText } from "./account";

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

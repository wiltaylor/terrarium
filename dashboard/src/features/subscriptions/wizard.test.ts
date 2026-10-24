import { describe, expect, it } from "vitest";
import type { LoginSession } from "../../api/types";
import { countdown, endMessage, initialWizard, reloginFor, sessionToCancel, shouldPoll, teardownLeft, TEARDOWN_MS, wizardReduce, type WizardAction, type WizardState } from "./wizard";

const session = (over: Partial<LoginSession> = {}): LoginSession => ({
  id: "L1",
  kind: "codex",
  state: "starting",
  started_at: "2026-10-24T10:00:00Z",
  done: false,
  instructions: "",
  ...over,
});
const run = (s: WizardState, ...actions: WizardAction[]) => actions.reduce(wizardReduce, s);

describe("wizard state machine", () => {
  it("goes pick -> setup -> starting -> live -> done", () => {
    let s = run(initialWizard(), { type: "pick", path: "codex" });
    expect(s).toEqual({ step: "setup", path: "codex", setup: {} });
    s = run(s, { type: "setup", setup: { name: "chatgpt-home" } }, { type: "start" });
    expect(s).toMatchObject({ step: "starting", path: "codex", setup: { name: "chatgpt-home" } });
    s = run(s, { type: "started", session: session() });
    expect(s.step).toBe("live");
    expect(shouldPoll(s)).toBe(true);
    expect(sessionToCancel(s)).toBe("L1");
    s = run(s, { type: "polled", session: session({ state: "waiting", url: "https://x", device_code: "AB-CD" }) });
    expect(s.step === "live" && s.session.device_code).toBe("AB-CD");
    s = run(s, { type: "polled", session: session({ state: "done", done: true, subscription: "chatgpt-home" }) });
    expect(s).toMatchObject({ step: "done", subscription: "chatgpt-home" });
    expect(shouldPoll(s)).toBe(false);
    expect(sessionToCancel(s)).toBeNull();
  });

  it("ends on failed, cancelled, or expired, and can retry with the same setup", () => {
    for (const state of ["failed", "cancelled", "expired"] as const) {
      const s = run(initialWizard("claude", { name: "c" }), { type: "start" }, { type: "started", session: session({ kind: "claude" }) }, { type: "polled", session: session({ kind: "claude", state, done: true, error: "exit 1" }) });
      expect(s.step).toBe("ended");
      if (s.step !== "ended") return;
      expect(s.message).toBe(endMessage(s.session));
      expect(sessionToCancel(s)).toBeNull();
      expect(run(s, { type: "retry" })).toEqual({ step: "setup", path: "claude", setup: { name: "c" } });
      expect(run(s, { type: "back" })).toEqual({ step: "pick" });
    }
    expect(endMessage(session({ state: "failed", error: "exit 1" }))).toBe("The login failed: exit 1");
  });

  it("goes back to setup when the start call fails, and drops late or foreign polls", () => {
    const starting = run(initialWizard("copilot"), { type: "start" });
    expect(run(starting, { type: "failed" })).toEqual({ step: "setup", path: "copilot", setup: {} });
    const live = run(starting, { type: "started", session: session({ kind: "copilot" }) });
    expect(run(live, { type: "polled", session: session({ id: "OTHER", state: "done", done: true, subscription: "x" }) })).toBe(live);
    const done = run(live, { type: "polled", session: session({ kind: "copilot", state: "done", done: true, subscription: "copilot" }) });
    expect(run(done, { type: "polled", session: session({ state: "failed", done: true }) })).toBe(done);
  });

  it("treats a session that is already done at start as the end", () => {
    const s = run(initialWizard("codex"), { type: "start" }, { type: "started", session: session({ state: "failed", done: true }) });
    expect(s.step).toBe("ended");
  });

  it("saves the form paths straight to done, and does not start a login for them", () => {
    const k = initialWizard("api_key", { provider: "github" });
    expect(run(k, { type: "start" })).toBe(k);
    expect(run(k, { type: "saved", subscription: "gh" })).toEqual({ step: "done", subscription: "gh" });
    expect(run(initialWizard("define"), { type: "saved", subscription: "d" })).toEqual({ step: "done", subscription: "d" });
    // A login path does not take `saved`.
    const l = initialWizard("codex");
    expect(run(l, { type: "saved", subscription: "x" })).toBe(l);
  });

  it("counts the ten-minute teardown down from started_at", () => {
    const start = Date.parse("2026-10-24T10:00:00Z");
    expect(teardownLeft(session(), start)).toBe(TEARDOWN_MS);
    expect(teardownLeft(session(), start + 9 * 60_000 + 30_000)).toBe(30_000);
    expect(teardownLeft(session(), start + 11 * 60_000)).toBe(0);
    expect(countdown(TEARDOWN_MS)).toBe("10:00");
    expect(countdown(61_500)).toBe("1:02");
    expect(countdown(0)).toBe("0:00");
  });

  it("picks the re-login path from the subscription", () => {
    const base = { name: "n", provider: "anthropic", env_var: undefined, has_api_key: false };
    expect(reloginFor({ ...base, harnesses: ["claude", "pi"] })).toEqual({ path: "claude", setup: { name: "n" } });
    expect(reloginFor({ ...base, harnesses: ["pi"], provider: "openai-codex" })).toEqual({ path: "pi", setup: { name: "n", provider: "openai-codex" } });
    expect(reloginFor({ ...base, harnesses: ["pi"], provider: "groq", has_api_key: true, env_var: "GROQ_API_KEY" })).toEqual({ path: "api_key", setup: { name: "n", provider: "groq" } });
  });
});

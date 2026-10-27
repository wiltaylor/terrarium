import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import {
  changePassword,
  createAuthStore,
  LEGACY_KEYS,
  loginError,
  passwordFormError,
  restoreSession,
  signIn,
  signInWithPassword,
  signOut,
  signOutEverywhere,
  TOKEN_KEY,
  upgradeToSession,
  type KeyValueStorage,
} from "./auth";
import { createMockApi, type MockOptions } from "../api/mock";
import { MOCK_PASSWORD, MOCK_TOKEN } from "../api/mock/ws";
import { ApiError, AuthError } from "../api/http";

class Mem implements KeyValueStorage {
  m = new Map<string, string>();
  getItem(k: string) {
    return this.m.get(k) ?? null;
  }
  setItem(k: string, v: string) {
    this.m.set(k, v);
  }
  removeItem(k: string) {
    this.m.delete(k);
  }
  /** Every stored value, to check that no secret is in them. */
  dump() {
    return [...this.m.values()].join("\n");
  }
}

const make = (session = new Mem(), local = new Mem()) => createRoot(() => createAuthStore({ session, local }));

/**
 * The browser mock: it checks what the transport would send, from the
 * store's own signals, and keeps its fake cookie in `jar` (a reload of the
 * page keeps the cookie, as a browser does).
 */
function browser(auth: ReturnType<typeof make>, extra: MockOptions = {}, jar = new Mem()) {
  return createMockApi({
    latencyMs: 0,
    eventsIntervalMs: 0,
    stepMs: 0,
    client: { token: auth.token, csrf: auth.csrf, onUnauthorized: auth.requireToken },
    cookieJar: jar,
    ...extra,
  });
}

describe("auth store: load", () => {
  it("starts in `checking`, with no login screen yet", () => {
    const a = make();
    expect(a.mode()).toBe("checking");
    expect(a.needsToken()).toBe(false);
    expect(a.csrf()).toBeNull();
  });

  it("no cookie: GET /api/session is 401, so the login screen shows, with no 'session ended' note", async () => {
    const a = make();
    expect(await restoreSession(browser(a), a)).toBe(false);
    expect(a.mode()).toBe("none");
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(false);
  });

  it("the cookie is the cache: a reload gets the CSRF value from GET /api/session, and stores nothing", async () => {
    const jar = new Mem();
    const session = new Mem();
    const local = new Mem();
    const first = make(session, local);
    const api1 = browser(first, {}, jar);
    await restoreSession(api1, first);
    await signInWithPassword(api1, first, MOCK_PASSWORD);
    const csrf = first.csrf();
    expect(csrf).toMatch(/^csrf_/);
    // Nothing of the session is in this page's storage.
    expect(session.m.size + local.m.size).toBe(0);
    // A reload: a new store, the same cookie jar.
    const again = make(session, local);
    const api2 = browser(again, {}, jar);
    expect(await restoreSession(api2, again)).toBe(true);
    expect(again.mode()).toBe("session");
    expect(again.csrf()).toBe(csrf);
    expect(again.passwordSet()).toBe(true);
    // The CSRF value works for a write.
    expect((await api2.cards.create("terrarium", { title: "after reload" })).title).toBe("after reload");
  });

  it("deletes the copies of older builds; an old token moves to this tab and becomes a session", async () => {
    const session = new Mem();
    const local = new Mem();
    local.setItem(TOKEN_KEY, MOCK_TOKEN);
    for (const k of LEGACY_KEYS) {
      local.setItem(k, '{"csrf":"old","expires_at":"2099-01-01T00:00:00Z"}');
      session.setItem(k, '{"csrf":"old","expires_at":"2099-01-01T00:00:00Z"}');
    }
    const a = make(session, local);
    expect(a.mode()).toBe("bearer");
    expect(local.m.size).toBe(0);
    expect([...session.m.keys()]).toEqual([TOKEN_KEY]);
    expect(await restoreSession(browser(a), a)).toBe(true);
    expect(a.mode()).toBe("session");
    expect(session.dump() + local.dump()).not.toContain(MOCK_TOKEN);
  });
});

describe("auth store: password login", () => {
  it("keeps only the CSRF value, in memory, never the password", async () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    await signInWithPassword(browser(a), a, MOCK_PASSWORD);
    expect(a.mode()).toBe("session");
    expect(a.token()).toBeNull();
    expect(a.csrf()).toMatch(/^csrf_/);
    expect(a.expiresAt()).toMatch(/Z$/);
    expect(session.m.size + local.m.size).toBe(0);
  });

  it("a wrong password is an AuthError and changes nothing", async () => {
    const a = make();
    await restoreSession(browser(a), a);
    const err = await signInWithPassword(browser(a), a, "wrong").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AuthError);
    expect(a.mode()).toBe("none");
  });

  it("no password set: 409, and the screen says to run terrariumd password set", async () => {
    const a = make();
    const err = await signInWithPassword(browser(a, { password: null }), a, "anything").catch((e: unknown) => e);
    expect(err).toMatchObject({ status: 409 });
    const p = loginError(err);
    expect(p.noPassword).toBe(true);
    expect(p.message).toContain("terrariumd password set");
  });

  it("five failures, then 429 with the wait in seconds", async () => {
    const a = make();
    const api = browser(a);
    for (let i = 0; i < 5; i++) await signInWithPassword(api, a, `wrong ${i}`).catch(() => undefined);
    const err = await signInWithPassword(api, a, MOCK_PASSWORD).catch((e: unknown) => e);
    expect(err).toMatchObject({ status: 429 });
    const p = loginError(err);
    expect(p.retryAfter).toBe(30);
    expect(p.message).toContain("30 s");
  });
});

describe("loginError: the text of the login screen", () => {
  it("the same text for every wrong password, and for a token", () => {
    expect(loginError(new AuthError({ error: { code: "unauthorized", message: "login failed" } })).message).toBe("The password is not correct.");
    expect(loginError(new AuthError(undefined)).message).toBe("The password is not correct.");
    expect(loginError(new AuthError(undefined), "token").message).toBe("The daemon refused the token.");
  });

  it("429 with and without retry_after, 404 (no password login), and other errors", () => {
    expect(loginError(new ApiError(429, "x", undefined, "rate_limited", { retry_after: 12.2 }))).toEqual({
      message: "Too many failed sign-ins. Try again in 13 s.",
      retryAfter: 13,
    });
    expect(loginError(new ApiError(429, "x", undefined, "rate_limited")).retryAfter).toBeUndefined();
    expect(loginError(new ApiError(404, "no such API route")).message).toContain("Use the token instead");
    expect(loginError(new Error("down")).message).toBe("Sign-in failed: down");
  });
});

describe("auth store: the session ends", () => {
  it("a 401 in session mode marks it expired, keeps the mode (the page stays), and drops the CSRF value", () => {
    const a = make();
    a.startSession({ csrf: "c1", expires_at: "2099-01-01T00:00:00Z" });
    a.requireToken();
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(true);
    expect(a.mode()).toBe("session");
    expect(a.csrf()).toBeNull();
    a.startSession({ csrf: "c2", expires_at: "2099-01-01T00:00:00Z" });
    expect(a.expired()).toBe(false);
    expect(a.needsToken()).toBe(false);
  });

  it("a 401 while checking is a plain sign-out", () => {
    const a = make();
    a.requireToken();
    expect(a.mode()).toBe("none");
    expect(a.expired()).toBe(false);
  });

  it("sign-out calls DELETE /api/session and forgets everything", async () => {
    const a = make();
    const api = browser(a);
    await signInWithPassword(api, a, MOCK_PASSWORD);
    await signOut(api, a);
    expect(a.mode()).toBe("none");
    expect(a.needsToken()).toBe(true);
    // The cookie is gone on the daemon side too.
    await expect(api.projects.list()).rejects.toMatchObject({ status: 401 });
  });

  it("log out everywhere sends the CSRF value and ends the session", async () => {
    const a = make();
    const api = browser(a);
    await signInWithPassword(api, a, MOCK_PASSWORD);
    await signOutEverywhere(api, a);
    expect(a.mode()).toBe("none");
    const b = make();
    expect(await restoreSession(browser(b), b)).toBe(false);
  });
});

describe("auth store: change the password", () => {
  it("a cookie session sends the current password, then signs in again with the new one", async () => {
    const a = make();
    const api = browser(a);
    await signInWithPassword(api, a, MOCK_PASSWORD);
    const old = a.csrf();
    await expect(changePassword(api, a, "wrong", "a new long password")).rejects.toMatchObject({ status: 403 });
    expect(a.csrf()).toBe(old);
    await changePassword(api, a, MOCK_PASSWORD, "a new long password");
    expect(a.mode()).toBe("session");
    expect(a.csrf()).not.toBe(old);
    expect((await api.cards.create("terrarium", { title: "new session" })).title).toBe("new session");
    await expect(signInWithPassword(api, a, MOCK_PASSWORD)).rejects.toBeInstanceOf(AuthError);
  });

  it("checks the form before it calls the daemon", () => {
    const f = { current: "x", next: "a long password", again: "a long password", needCurrent: true };
    expect(passwordFormError(f)).toBeNull();
    expect(passwordFormError({ ...f, current: "" })).toContain("current");
    expect(passwordFormError({ ...f, current: "", needCurrent: false })).toBeNull();
    expect(passwordFormError({ ...f, next: "short", again: "short" })).toContain("12");
    expect(passwordFormError({ ...f, again: "other" })).toContain("not the same");
  });
});

describe("auth store: the token instead (and a bearer-only daemon)", () => {
  it("the token buys a cookie session; the token is not kept", async () => {
    const session = new Mem();
    const a = make(session);
    expect(await signIn(browser(a), a, ` ${MOCK_TOKEN} `)).toBe("session");
    expect(a.token()).toBeNull();
    expect(session.dump()).not.toContain(MOCK_TOKEN);
  });

  it("falls back to the token, in this tab only, when POST /api/session is 404", async () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    expect(await signIn(createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, sessions: false }), a, " t1 ")).toBe("bearer");
    expect(a.bearerFallback()).toBe(true);
    expect(a.token()).toBe("t1");
    expect(session.getItem(TOKEN_KEY)).toBe("t1");
    expect(local.m.size).toBe(0);
  });

  it("stays in bearer mode when the upgrade gets 404", async () => {
    const a = make();
    a.useBearer("t");
    expect(await upgradeToSession(createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, sessions: false }), a)).toBe(false);
    expect(a.mode()).toBe("bearer");
  });

  it("a 401 in bearer mode asks again but is not an expiry", () => {
    const a = make();
    a.useBearer("t");
    a.requireToken();
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(false);
  });

  it("works when storage throws", () => {
    const bad: KeyValueStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
      removeItem: () => {
        throw new Error("blocked");
      },
    };
    const a = createRoot(() => createAuthStore({ session: bad, local: bad }));
    a.startSession({ csrf: "c1", expires_at: null });
    expect(a.csrf()).toBe("c1");
    a.useBearer("m");
    expect(a.token()).toBe("m");
  });
});

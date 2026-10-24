import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import {
  createAuthStore,
  REMEMBER_KEY,
  SESSION_KEY,
  SESSION_REMEMBER_KEY,
  signIn,
  signOut,
  TOKEN_KEY,
  upgradeToSession,
  type AuthOptions,
  type KeyValueStorage,
} from "./auth";
import { createMockApi } from "../api/mock";
import { MOCK_TOKEN } from "../api/mock/ws";
import { AuthError } from "../api/http";

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
  /** Every stored value, to check that the token is in none of them. */
  dump() {
    return [...this.m.values()].join("\n");
  }
}

const T0 = Date.parse("2026-10-24T10:00:00Z");
const HOUR = 3600_000;
const grant = (csrf = "c1", end = T0 + HOUR) => ({ csrf, expires_at: new Date(end).toISOString() });

/** A clock and a timer that the test moves by hand. */
function clock() {
  let t = T0;
  let timers: { at: number; fn: () => void }[] = [];
  const opts: AuthOptions = {
    now: () => t,
    setTimer: (fn, ms) => {
      const x = { at: t + ms, fn };
      timers.push(x);
      return x;
    },
    clearTimer: (x) => {
      timers = timers.filter((y) => y !== x);
    },
  };
  const advance = (ms: number) => {
    t += ms;
    const due = timers.filter((x) => x.at <= t);
    timers = timers.filter((x) => x.at > t);
    due.forEach((x) => x.fn());
  };
  return { opts, advance };
}

const make = (session: Mem, local: Mem, opts: AuthOptions = clock().opts) => createRoot(() => createAuthStore({ session, local }, opts));
const mockApi = (sessions = true) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, sessions });

describe("auth store: signed out", () => {
  it("starts with no credentials and the token screen", () => {
    const a = make(new Mem(), new Mem());
    expect(a.mode()).toBe("none");
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(false);
    expect(a.token()).toBeNull();
    expect(a.csrf()).toBeNull();
  });
});

describe("auth store: cookie session", () => {
  it("keeps only the CSRF value, in memory and sessionStorage, never the token", async () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    expect(await signIn(mockApi(), a, `  ${MOCK_TOKEN} `, false)).toBe("session");
    expect(a.mode()).toBe("session");
    expect(a.needsToken()).toBe(false);
    expect(a.token()).toBeNull();
    expect(a.csrf()).toMatch(/^csrf_/);
    expect(a.expiresAt()).toMatch(/Z$/);
    expect(JSON.parse(session.getItem(SESSION_KEY)!)).toEqual({ csrf: a.csrf(), expires_at: a.expiresAt() });
    expect(local.m.size).toBe(0);
    expect(session.dump() + local.dump()).not.toContain(MOCK_TOKEN);
    expect(a.remembered()).toBe(false);
  });

  it("survives a reload in the same tab, and a new tab asks again", () => {
    const session = new Mem();
    const local = new Mem();
    make(session, local).startSession(grant());
    const reload = make(session, local);
    expect(reload.mode()).toBe("session");
    expect(reload.csrf()).toBe("c1");
    expect(make(new Mem(), local).mode()).toBe("none");
  });

  it("drops a stored session whose expires_at has passed, and says it ended", () => {
    const session = new Mem();
    session.setItem(SESSION_KEY, JSON.stringify(grant("old", T0 - 1)));
    const a = make(session, new Mem());
    expect(a.mode()).toBe("none");
    expect(a.expired()).toBe(true);
    expect(session.getItem(SESSION_KEY)).toBeNull();
  });

  it("drops a malformed stored session", () => {
    const session = new Mem();
    session.setItem(SESSION_KEY, "{not json");
    expect(make(session, new Mem()).mode()).toBe("none");
  });
});

describe("auth store: remember on this device", () => {
  it("puts the CSRF value (not the token) in localStorage, and a new tab is signed in", async () => {
    const local = new Mem();
    const a = make(new Mem(), local);
    await signIn(mockApi(), a, MOCK_TOKEN, true);
    expect(JSON.parse(local.getItem(SESSION_REMEMBER_KEY)!).csrf).toBe(a.csrf());
    expect(local.dump()).not.toContain(MOCK_TOKEN);
    const tab2Session = new Mem();
    const tab2 = make(tab2Session, local);
    expect(tab2.mode()).toBe("session");
    expect(tab2.remembered()).toBe(true);
    expect(tab2Session.getItem(SESSION_KEY)).not.toBeNull();
  });

  it("a sign-in without the box removes the device copy", () => {
    const local = new Mem();
    const a = make(new Mem(), local);
    a.startSession(grant(), true);
    a.startSession(grant("c2"), false);
    expect(local.getItem(SESSION_REMEMBER_KEY)).toBeNull();
  });
});

describe("auth store: the session ends", () => {
  it("a 401 in session mode marks it expired, keeps the mode (the page stays), and drops the CSRF value", () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    a.startSession(grant(), true);
    a.requireToken();
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(true);
    expect(a.mode()).toBe("session");
    expect(a.csrf()).toBeNull();
    expect(session.m.size + local.m.size).toBe(0);
    // The box stays ticked for the next sign-in.
    expect(a.remembered()).toBe(true);
    a.startSession(grant("c2"));
    expect(a.expired()).toBe(false);
    expect(a.needsToken()).toBe(false);
  });

  it("ends by itself at expires_at", () => {
    const c = clock();
    const a = make(new Mem(), new Mem(), c.opts);
    a.startSession(grant("c1", T0 + 10_000));
    c.advance(9_999);
    expect(a.needsToken()).toBe(false);
    c.advance(1);
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(true);
  });

  it("a new session cancels the timer of the old one", () => {
    const c = clock();
    const a = make(new Mem(), new Mem(), c.opts);
    a.startSession(grant("c1", T0 + 10_000));
    a.startSession(grant("c2", T0 + HOUR));
    c.advance(20_000);
    expect(a.needsToken()).toBe(false);
  });
});

describe("auth store: bearer-only daemon (404 fallback)", () => {
  it("falls back to the token when POST /api/session is 404", async () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    expect(await signIn(mockApi(false), a, " t1 ", false)).toBe("bearer");
    expect(a.mode()).toBe("bearer");
    expect(a.bearerFallback()).toBe(true);
    expect(a.token()).toBe("t1");
    expect(a.csrf()).toBeNull();
    expect(session.getItem(TOKEN_KEY)).toBe("t1");
    expect(local.m.size).toBe(0);
  });

  it("keeps the token in localStorage only on 'Remember on this device'", () => {
    const local = new Mem();
    const a = make(new Mem(), local);
    a.useBearer("t1", true);
    expect(local.getItem(REMEMBER_KEY)).toBe("t1");
    const tab2 = make(new Mem(), local);
    expect(tab2.mode()).toBe("bearer");
    expect(tab2.token()).toBe("t1");
    tab2.useBearer("t2", false);
    expect(local.getItem(REMEMBER_KEY)).toBeNull();
  });

  it("a 401 in bearer mode asks again but is not an expiry", () => {
    const a = make(new Mem(), new Mem());
    a.useBearer("t");
    a.requireToken();
    expect(a.needsToken()).toBe(true);
    expect(a.expired()).toBe(false);
  });

  it("moves an old localStorage token to this tab only, then trades it for a session", async () => {
    const session = new Mem();
    const local = new Mem();
    local.setItem(TOKEN_KEY, MOCK_TOKEN);
    const a = make(session, local);
    expect(a.mode()).toBe("bearer");
    expect(local.getItem(TOKEN_KEY)).toBeNull();
    expect(session.getItem(TOKEN_KEY)).toBe(MOCK_TOKEN);
    expect(await upgradeToSession(mockApi(), a)).toBe(true);
    expect(a.mode()).toBe("session");
    expect(session.dump() + local.dump()).not.toContain(MOCK_TOKEN);
  });

  it("stays in bearer mode when the upgrade gets 404", async () => {
    const a = make(new Mem(), new Mem());
    a.useBearer("t");
    expect(await upgradeToSession(mockApi(false), a)).toBe(false);
    expect(a.mode()).toBe("bearer");
  });

  it("a session from a newer daemon replaces every copy of the token", () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    a.useBearer("t", true);
    a.startSession(grant(), true);
    expect(session.dump() + local.dump()).not.toContain('"t"');
    expect(session.getItem(TOKEN_KEY)).toBeNull();
    expect(local.getItem(REMEMBER_KEY)).toBeNull();
  });
});

describe("sign-in errors and sign-out", () => {
  it("a wrong token is an AuthError and changes nothing", async () => {
    const a = make(new Mem(), new Mem());
    await expect(signIn(mockApi(), a, "wrong", false)).rejects.toBeInstanceOf(AuthError);
    expect(a.mode()).toBe("none");
  });

  it("sign-out calls DELETE /api/session and forgets everything", async () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    let removed = 0;
    await signIn(mockApi(), a, MOCK_TOKEN, true);
    await signOut({ session: { create: async () => null, remove: async () => void removed++ } }, a);
    expect(removed).toBe(1);
    expect(a.mode()).toBe("none");
    expect(a.needsToken()).toBe(true);
    expect(a.remembered()).toBe(false);
    expect(session.m.size + local.m.size).toBe(0);
  });

  it("sign-out in bearer mode does not call the daemon, and a failed DELETE still signs out", async () => {
    let removed = 0;
    const a = make(new Mem(), new Mem());
    a.useBearer("t");
    await signOut({ session: { create: async () => null, remove: async () => void removed++ } }, a);
    expect(removed).toBe(0);
    a.startSession(grant());
    await signOut({ session: { create: async () => null, remove: async () => Promise.reject(new Error("down")) } }, a);
    expect(a.mode()).toBe("none");
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
    const a = createRoot(() => createAuthStore({ session: bad, local: bad }, clock().opts));
    a.startSession(grant());
    expect(a.csrf()).toBe("c1");
    a.useBearer("m");
    expect(a.token()).toBe("m");
  });
});

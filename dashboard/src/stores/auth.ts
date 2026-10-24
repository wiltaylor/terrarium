// Dashboard sign-in (docs/security-review.md F3, docs/api.md §1.1).
//
// The token screen posts the bearer token once to `POST /api/session`. The
// daemon answers with an `HttpOnly` session cookie, which script cannot
// read, and a CSRF value. The store keeps only the CSRF value and the end
// of the session: in memory, in sessionStorage, and (on "Remember on this
// device") in localStorage. The bearer token is not kept.
//
// A daemon without sessions answers `POST /api/session` with 404. Then the
// store falls back to bearer mode: it keeps the token, as before, in memory
// and sessionStorage, and in localStorage on opt-in. The app shows a notice.
//
// Modes:
//   none     no credentials. The token screen shows.
//   session  the cookie authenticates. `csrf` is set; `token` is null.
//   bearer   the token authenticates. `token` is set; `csrf` is null.
// A 401 in `session` mode means that the cookie ended: `expired` is true and
// the token screen shows over the current page, which stays mounted.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { SessionGrant } from "../api/types";

/** sessionStorage key of the bearer token (bearer mode). Also the key of the old localStorage copy, which the store moves out. */
export const TOKEN_KEY = "terrarium.token";
/** localStorage key of the bearer token, set only on "Remember on this device" in bearer mode. */
export const REMEMBER_KEY = "terrarium.token.remembered";
/** sessionStorage key of the session: `{"csrf", "expires_at"}`. Never the token. */
export const SESSION_KEY = "terrarium.session";
/** localStorage key of the session, set only on "Remember on this device". */
export const SESSION_REMEMBER_KEY = "terrarium.session.remembered";

export type AuthMode = "none" | "session" | "bearer";

export interface KeyValueStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface AuthStorages {
  /** Per tab. Usually `sessionStorage`. */
  session?: KeyValueStorage | null;
  /** Per device. Usually `localStorage`. Used only on opt-in. */
  local?: KeyValueStorage | null;
}

export interface AuthOptions {
  now?: () => number;
  /** Timer for the end of the session. Tests pass their own. */
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (t: unknown) => void;
}

const get = (s: KeyValueStorage | null | undefined, k: string): string | null => {
  try {
    return s?.getItem(k) ?? null;
  } catch {
    return null;
  }
};
const set = (s: KeyValueStorage | null | undefined, k: string, v: string): void => {
  try {
    s?.setItem(k, v);
  } catch {
    // storage blocked; the memory copy still works for this page
  }
};
const del = (s: KeyValueStorage | null | undefined, k: string): void => {
  try {
    s?.removeItem(k);
  } catch {
    // ignore
  }
};

/** Read a stored session. Null when it is absent, malformed, or ended at `now`. */
export function readSession(raw: string | null, now: number): SessionGrant | null {
  if (raw === null) return null;
  try {
    const o = JSON.parse(raw) as Partial<SessionGrant>;
    if (typeof o.csrf !== "string" || !o.csrf || typeof o.expires_at !== "string") return null;
    const end = Date.parse(o.expires_at);
    if (Number.isFinite(end) && end <= now) return null;
    return { csrf: o.csrf, expires_at: o.expires_at };
  } catch {
    return null;
  }
}

/** The longest delay `setTimeout` takes. */
const MAX_TIMER = 2 ** 31 - 1;

export function createAuthStore(storages: AuthStorages = {}, opts: AuthOptions = {}) {
  const { session, local } = storages;
  const now = opts.now ?? Date.now;
  const setTimer = opts.setTimer ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));
  const clearTimer = opts.clearTimer ?? ((t: unknown) => clearTimeout(t as ReturnType<typeof setTimeout>));

  // Before the opt-in, the token lived in localStorage. Move it to this tab only.
  const legacy = get(local, TOKEN_KEY);
  if (legacy !== null) {
    if (get(session, TOKEN_KEY) === null) set(session, TOKEN_KEY, legacy);
    del(local, TOKEN_KEY);
  }

  // A stored session wins over a stored token. A session that ended is dropped.
  const tabSession = readSession(get(session, SESSION_KEY), now());
  const deviceSession = readSession(get(local, SESSION_REMEMBER_KEY), now());
  const hadSession = get(session, SESSION_KEY) !== null || get(local, SESSION_REMEMBER_KEY) !== null;
  const storedSession = tabSession ?? deviceSession;
  if (!tabSession) del(session, SESSION_KEY);
  if (!deviceSession) del(local, SESSION_REMEMBER_KEY);
  if (storedSession && !tabSession) set(session, SESSION_KEY, JSON.stringify(storedSession));

  const tokenRemembered = get(local, REMEMBER_KEY);
  const storedToken = storedSession ? null : (get(session, TOKEN_KEY) ?? tokenRemembered);
  if (storedToken !== null && get(session, TOKEN_KEY) === null) set(session, TOKEN_KEY, storedToken);

  const initialMode: AuthMode = storedSession ? "session" : storedToken !== null ? "bearer" : "none";
  const [mode, setMode] = createSignal<AuthMode>(initialMode);
  const [csrf, setCsrf] = createSignal<string | null>(storedSession?.csrf ?? null);
  const [expiresAt, setExpiresAt] = createSignal<string | null>(storedSession?.expires_at ?? null);
  const [token, setTokenSignal] = createSignal<string | null>(storedToken);
  const [remembered, setRemembered] = createSignal(storedSession ? deviceSession !== null : tokenRemembered !== null);
  const [needsToken, setNeedsToken] = createSignal(initialMode === "none");
  /** The session ended (a 401, or `expires_at` passed). The token screen says so. */
  const [expired, setExpired] = createSignal(initialMode === "none" && hadSession);

  let timer: unknown;
  const stopTimer = () => {
    if (timer !== undefined) clearTimer(timer);
    timer = undefined;
  };

  const forgetSession = () => {
    del(session, SESSION_KEY);
    del(local, SESSION_REMEMBER_KEY);
    setCsrf(null);
    setExpiresAt(null);
    stopTimer();
  };
  const forgetToken = () => {
    del(session, TOKEN_KEY);
    del(local, REMEMBER_KEY);
    del(local, TOKEN_KEY);
    setTokenSignal(null);
  };

  /** The cookie ended: ask for the token again over the current page. */
  const endSession = () => {
    if (mode() !== "session") return;
    forgetSession();
    setExpired(true);
    setNeedsToken(true);
  };

  const armTimer = (expires: string) => {
    stopTimer();
    const end = Date.parse(expires);
    if (!Number.isFinite(end)) return;
    const wait = end - now();
    if (wait <= 0) return endSession();
    timer = setTimer(() => (wait > MAX_TIMER ? armTimer(expires) : endSession()), Math.min(wait, MAX_TIMER));
  };
  if (storedSession) armTimer(storedSession.expires_at);

  return {
    mode,
    /** The bearer token, in bearer mode only. */
    token,
    /** The CSRF value of the cookie session, in session mode only. */
    csrf,
    /** When the cookie session ends (RFC 3339), in session mode only. */
    expiresAt,
    needsToken,
    expired,
    /** True when the credential (the CSRF value, or in bearer mode the token) also sits in localStorage. */
    remembered,
    /** True when the daemon has no sessions, so the token stays in the browser. */
    bearerFallback: () => mode() === "bearer",

    /** Called by the transport on HTTP 401. */
    requireToken() {
      if (mode() === "session") return endSession();
      setNeedsToken(true);
    },

    /** Keep a session from `POST /api/session`. Every copy of the token goes. */
    startSession(grant: SessionGrant, remember = false) {
      const stored = JSON.stringify({ csrf: grant.csrf, expires_at: grant.expires_at });
      forgetToken();
      set(session, SESSION_KEY, stored);
      if (remember) set(local, SESSION_REMEMBER_KEY, stored);
      else del(local, SESSION_REMEMBER_KEY);
      setCsrf(grant.csrf);
      setExpiresAt(grant.expires_at);
      setRemembered(remember);
      setMode("session");
      setExpired(false);
      setNeedsToken(false);
      armTimer(grant.expires_at);
    },

    /** Bearer mode, for a daemon without sessions: keep the token, as before sessions. */
    useBearer(value: string, remember = false) {
      const v = value.trim();
      forgetSession();
      set(session, TOKEN_KEY, v);
      if (remember) set(local, REMEMBER_KEY, v);
      else del(local, REMEMBER_KEY);
      setTokenSignal(v);
      setRemembered(remember);
      setMode("bearer");
      setExpired(false);
      setNeedsToken(false);
    },

    /** Forget everything: memory, this tab, and this device. */
    clear() {
      forgetSession();
      forgetToken();
      setRemembered(false);
      setMode("none");
      setExpired(false);
      setNeedsToken(true);
    },
  };
}

export type AuthStore = ReturnType<typeof createAuthStore>;

/** How a sign-in ended: a cookie session, or the bearer fallback. */
export type SignInResult = "session" | "bearer";

/**
 * Post the token once to `POST /api/session` and keep the session. A daemon
 * without sessions (404) gives bearer mode. A wrong token throws `AuthError`.
 */
export async function signIn(api: Pick<Api, "session">, auth: AuthStore, token: string, remember: boolean): Promise<SignInResult> {
  const t = token.trim();
  const grant = await api.session.create(t);
  if (grant) {
    auth.startSession(grant, remember);
    return "session";
  }
  auth.useBearer(t, remember);
  return "bearer";
}

/** `DELETE /api/session` (in session mode), then forget everything here. */
export async function signOut(api: Pick<Api, "session">, auth: AuthStore): Promise<void> {
  if (auth.mode() === "session") {
    try {
      await api.session.remove();
    } catch {
      // The local state goes anyway. The cookie ends at `expires_at`.
    }
  }
  auth.clear();
}

/**
 * Bearer mode on load (a token from an older build, or a daemon that had no
 * sessions): try to trade the token for a session. On success the token
 * leaves the browser. On 404 nothing changes.
 */
export async function upgradeToSession(api: Pick<Api, "session">, auth: AuthStore): Promise<boolean> {
  const t = auth.token();
  if (auth.mode() !== "bearer" || !t) return false;
  try {
    const grant = await api.session.create(t);
    if (!grant) return false;
    auth.startSession(grant, auth.remembered());
    return true;
  } catch {
    return false;
  }
}

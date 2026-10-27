// Dashboard sign-in (docs/security-review.md F3 and F15, docs/api.md §1.1).
//
// The login screen posts the operator password (or, on "Use the token
// instead", the bearer token) once to `POST /api/session`. The daemon
// answers with an `HttpOnly` session cookie, which script cannot read, and a
// CSRF value. The cookie is the cache: it lives `TERRARIUM_SESSION_TTL`
// (30 days by default) and the daemon renews it past half its life. The
// store keeps the CSRF value in memory only. At load it calls
// `GET /api/session`: a 200 gives the CSRF value again, so there is no login
// screen; a 401 shows it. A renewed cookie keeps the same CSRF value, so
// nothing changes here on a renewal.
//
// The password, the session token, and the CSRF value never go into
// localStorage or sessionStorage.
//
// A daemon without sessions answers `POST /api/session` with 404. Then the
// store falls back to bearer mode: it keeps the token in memory and in this
// tab's sessionStorage, as older builds did. The app shows a notice.
//
// Modes:
//   checking  the load-time `GET /api/session` has not answered yet.
//   none      no credentials. The login screen shows.
//   session   the cookie authenticates. `csrf` is set; `token` is null.
//   bearer    the token authenticates. `token` is set; `csrf` is null.
// A 401 in `session` mode means that the cookie ended: `expired` is true and
// the login screen shows over the current page, which stays mounted.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import { ApiError, AuthError } from "../api/http";
import type { SessionGrant } from "../api/types";

/** sessionStorage key of the bearer token (bearer mode only). */
export const TOKEN_KEY = "terrarium.token";
/** Keys of older builds. The store deletes them at load: it keeps no CSRF value and no token on the device. */
export const LEGACY_KEYS = ["terrarium.token.remembered", "terrarium.session", "terrarium.session.remembered"];

export type AuthMode = "checking" | "none" | "session" | "bearer";

export interface KeyValueStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface AuthStorages {
  /** Per tab. Usually `sessionStorage`. Bearer mode only. */
  session?: KeyValueStorage | null;
  /** Per device. Usually `localStorage`. Only read to delete the copies of older builds. */
  local?: KeyValueStorage | null;
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

export function createAuthStore(storages: AuthStorages = {}) {
  const { session, local } = storages;

  // Older builds kept the token or the CSRF value on the device. A token
  // moves to this tab only (bearer mode, then the upgrade to a session);
  // every other copy goes.
  const legacyToken = get(local, TOKEN_KEY) ?? get(local, LEGACY_KEYS[0]!);
  for (const k of [TOKEN_KEY, ...LEGACY_KEYS]) del(local, k);
  for (const k of LEGACY_KEYS) del(session, k);
  if (legacyToken !== null && get(session, TOKEN_KEY) === null) set(session, TOKEN_KEY, legacyToken);
  const storedToken = get(session, TOKEN_KEY);

  const [mode, setMode] = createSignal<AuthMode>(storedToken !== null ? "bearer" : "checking");
  const [csrf, setCsrf] = createSignal<string | null>(null);
  const [expiresAt, setExpiresAt] = createSignal<string | null>(null);
  const [token, setTokenSignal] = createSignal<string | null>(storedToken);
  const [passwordSet, setPasswordSet] = createSignal<boolean | null>(null);
  const [needsToken, setNeedsToken] = createSignal(false);
  /** The session ended (a 401 after a sign-in). The login screen says so. */
  const [expired, setExpired] = createSignal(false);

  const forgetSession = () => {
    setCsrf(null);
    setExpiresAt(null);
  };
  const forgetToken = () => {
    del(session, TOKEN_KEY);
    setTokenSignal(null);
  };

  /** The cookie ended: ask again over the current page. */
  const endSession = () => {
    if (mode() !== "session") return;
    forgetSession();
    setExpired(true);
    setNeedsToken(true);
  };

  return {
    mode,
    /** The bearer token, in bearer mode only. */
    token,
    /** The CSRF value of the cookie session, in session mode only. In memory only. */
    csrf,
    /** When the cookie session ends (RFC 3339), as the daemon last said. A renewal moves it later. */
    expiresAt,
    /** Is an operator password set? Null until `GET /api/session` answers. */
    passwordSet,
    needsToken,
    expired,
    /** True when the daemon has no sessions, so the token stays in the browser. */
    bearerFallback: () => mode() === "bearer",

    /** Called by the transport on HTTP 401. */
    requireToken() {
      if (mode() === "session") return endSession();
      if (mode() === "checking") return setMode("none");
      setNeedsToken(true);
    },

    /** Keep a session from `POST /api/session` or `GET /api/session`. Every copy of the token goes. */
    startSession(grant: Pick<SessionGrant, "csrf" | "expires_at"> | { csrf: string | null; expires_at?: string | null }) {
      forgetToken();
      setCsrf(grant.csrf ?? null);
      setExpiresAt(grant.expires_at ?? null);
      setMode("session");
      setExpired(false);
      setNeedsToken(false);
    },

    /** Bearer mode, for a daemon without sessions: keep the token in this tab. */
    useBearer(value: string) {
      const v = value.trim();
      forgetSession();
      set(session, TOKEN_KEY, v);
      setTokenSignal(v);
      setMode("bearer");
      setExpired(false);
      setNeedsToken(false);
    },

    setPasswordSet,

    /** No session at load: show the login screen, with no "session ended" note. */
    signedOut() {
      forgetSession();
      setMode("none");
      setNeedsToken(true);
    },

    /** Forget everything: memory and this tab. */
    clear() {
      forgetSession();
      forgetToken();
      setMode("none");
      setExpired(false);
      setNeedsToken(true);
    },
  };
}

export type AuthStore = ReturnType<typeof createAuthStore>;

/**
 * At load: ask the daemon whether the cookie is a session
 * (`GET /api/session`). A 200 gives the CSRF value, so there is no login
 * screen. A 401 (or no daemon) shows the login screen. Bearer mode keeps
 * its token and tries to trade it for a session.
 */
export async function restoreSession(api: Pick<Api, "session">, auth: AuthStore): Promise<boolean> {
  if (auth.mode() === "bearer") return upgradeToSession(api, auth);
  if (auth.mode() !== "checking") return false;
  try {
    const info = await api.session.current();
    if (!info) {
      auth.signedOut();
      return false;
    }
    auth.setPasswordSet(info.password_set);
    auth.startSession({ csrf: info.csrf ?? null, expires_at: info.expires_at ?? null });
    return true;
  } catch {
    auth.signedOut();
    return false;
  }
}

/** How a sign-in ended: a cookie session, or the bearer fallback. */
export type SignInResult = "session" | "bearer";

/**
 * Post the token once to `POST /api/session` and keep the session. A daemon
 * without sessions (404) gives bearer mode. A wrong token throws `AuthError`.
 */
export async function signIn(api: Pick<Api, "session">, auth: AuthStore, token: string): Promise<SignInResult> {
  const t = token.trim();
  const grant = await api.session.create(t);
  if (grant) {
    auth.startSession(grant);
    return "session";
  }
  auth.useBearer(t);
  return "bearer";
}

/** Post the operator password once to `POST /api/session` and keep the session. Errors: see `loginError`. */
export async function signInWithPassword(api: Pick<Api, "session">, auth: AuthStore, password: string): Promise<void> {
  const grant = await api.session.login(password);
  auth.setPasswordSet(true);
  auth.startSession(grant);
}

/** What the login screen says about a failed sign-in. */
export interface LoginProblem {
  message: string;
  /** `429`: wait this many seconds. */
  retryAfter?: number;
  /** `409`: the daemon has no password; use the token, or set one. */
  noPassword?: boolean;
}

/** Map a sign-in error to the text of the login screen. Every wrong password gets the same text. */
export function loginError(e: unknown, via: "password" | "token" = "password"): LoginProblem {
  if (e instanceof AuthError || (e instanceof ApiError && e.status === 401)) {
    return { message: via === "password" ? "The password is not correct." : "The daemon refused the token." };
  }
  if (e instanceof ApiError && e.status === 409) {
    return {
      message: "The daemon has no operator password. Run `terrariumd password set` on the daemon host, or use the token instead.",
      noPassword: true,
    };
  }
  if (e instanceof ApiError && e.status === 429) {
    const d = e.details as { retry_after?: unknown } | undefined;
    const secs = typeof d?.retry_after === "number" && d.retry_after > 0 ? Math.ceil(d.retry_after) : undefined;
    return {
      message: secs ? `Too many failed sign-ins. Try again in ${secs} s.` : "Too many failed sign-ins. Try again later.",
      retryAfter: secs,
    };
  }
  if (e instanceof ApiError && e.status === 404 && via === "password") {
    return { message: "This daemon has no password login. Use the token instead." };
  }
  return { message: `Sign-in failed: ${e instanceof Error ? e.message : String(e)}` };
}

/** The shortest operator password (the daemon checks it too). */
export const MIN_PASSWORD = 12;

/** Check the change-password form. Null: it may go to the daemon. */
export function passwordFormError(f: { current: string; next: string; again: string; needCurrent: boolean }): string | null {
  if (f.needCurrent && !f.current) return "Enter the current password.";
  if ([...f.next].length < MIN_PASSWORD) return `The new password needs at least ${MIN_PASSWORD} characters.`;
  if (f.next !== f.again) return "The two new passwords are not the same.";
  return null;
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

/** `DELETE /api/sessions`: end every session (this one too), then forget everything here. */
export async function signOutEverywhere(api: Pick<Api, "account">, auth: AuthStore): Promise<void> {
  await api.account.logoutEverywhere();
  auth.clear();
}

/**
 * `PUT /api/auth/password`. The change ends every session, also this one, so
 * a cookie session signs in again at once with the new password. In bearer
 * mode the token stays good.
 */
export async function changePassword(api: Pick<Api, "account" | "session">, auth: AuthStore, current: string, next: string): Promise<void> {
  const viaSession = auth.mode() === "session";
  await api.account.setPassword(current ? { current, new: next } : { new: next });
  auth.setPasswordSet(true);
  if (viaSession) await signInWithPassword(api, auth, next);
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
    auth.startSession(grant);
    return true;
  } catch {
    return false;
  }
}

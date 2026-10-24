// Bearer token for the daemon API (docs/security-review.md F3).
//
// The primary copy is in memory. A copy goes to sessionStorage, so a reload
// in the same tab keeps it and a new tab asks again. "Remember on this
// device" opts in to a copy in localStorage, which outlives the tab. Script
// on the dashboard origin can read every one of these; an HttpOnly cookie
// session is the real fix and needs the daemon (see src/README.md).

import { createSignal } from "solid-js";

/** sessionStorage key. Also the key of the old localStorage copy, which the store moves out. */
export const TOKEN_KEY = "terrarium.token";
/** localStorage key, set only when the operator ticks "Remember on this device". */
export const REMEMBER_KEY = "terrarium.token.remembered";

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

  // Before this change the token lived in localStorage without an opt-in.
  // Move it to this tab only; the operator ticks "Remember" to keep it.
  const legacy = get(local, TOKEN_KEY);
  if (legacy !== null) {
    if (get(session, TOKEN_KEY) === null) set(session, TOKEN_KEY, legacy);
    del(local, TOKEN_KEY);
  }

  const remembered0 = get(local, REMEMBER_KEY);
  const initial = get(session, TOKEN_KEY) ?? remembered0;
  if (initial !== null && get(session, TOKEN_KEY) === null) set(session, TOKEN_KEY, initial);

  const [token, setTokenSignal] = createSignal<string | null>(initial);
  const [remembered, setRemembered] = createSignal(remembered0 !== null);
  const [needsToken, setNeedsToken] = createSignal(false);

  return {
    token,
    needsToken,
    /** True when the token also sits in localStorage. */
    remembered,
    /** Called by the transport on HTTP 401. */
    requireToken: () => setNeedsToken(true),
    setToken(value: string, remember = false) {
      const v = value.trim();
      set(session, TOKEN_KEY, v);
      if (remember) set(local, REMEMBER_KEY, v);
      else del(local, REMEMBER_KEY);
      setTokenSignal(v);
      setRemembered(remember);
      setNeedsToken(false);
    },
    /** Forget the token everywhere: memory, this tab, and this device. */
    clear() {
      del(session, TOKEN_KEY);
      del(local, REMEMBER_KEY);
      del(local, TOKEN_KEY);
      setTokenSignal(null);
      setRemembered(false);
      setNeedsToken(true);
    },
  };
}

export type AuthStore = ReturnType<typeof createAuthStore>;

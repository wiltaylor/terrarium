// Bearer token for the daemon API. Kept in localStorage.

import { createSignal } from "solid-js";

export const TOKEN_KEY = "terrarium.token";

export interface KeyValueStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

function safeStorage(): KeyValueStorage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

export function createAuthStore(storage: KeyValueStorage | null = safeStorage()) {
  const read = () => {
    try {
      return storage?.getItem(TOKEN_KEY) ?? null;
    } catch {
      return null;
    }
  };
  const [token, setTokenSignal] = createSignal<string | null>(read());
  const [needsToken, setNeedsToken] = createSignal(false);

  return {
    token,
    needsToken,
    /** Called by the transport on HTTP 401. */
    requireToken: () => setNeedsToken(true),
    setToken(value: string) {
      const v = value.trim();
      try {
        storage?.setItem(TOKEN_KEY, v);
      } catch {
        // storage blocked; keep it in memory
      }
      setTokenSignal(v);
      setNeedsToken(false);
    },
    clear() {
      try {
        storage?.removeItem(TOKEN_KEY);
      } catch {
        // ignore
      }
      setTokenSignal(null);
      setNeedsToken(true);
    },
  };
}

export type AuthStore = ReturnType<typeof createAuthStore>;

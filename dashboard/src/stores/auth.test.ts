import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { createAuthStore, REMEMBER_KEY, TOKEN_KEY, type KeyValueStorage } from "./auth";

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
}

const make = (session: Mem, local: Mem) => createRoot(() => createAuthStore({ session, local }));

describe("createAuthStore", () => {
  it("keeps the token in memory and sessionStorage, not localStorage, by default", () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    expect(a.token()).toBeNull();
    a.setToken("  s3cret ");
    expect(a.token()).toBe("s3cret");
    expect(session.getItem(TOKEN_KEY)).toBe("s3cret");
    expect(local.m.size).toBe(0);
    expect(a.remembered()).toBe(false);
    // A reload in the same tab: sessionStorage survives.
    expect(make(session, local).token()).toBe("s3cret");
    // A new tab: a fresh sessionStorage, so it asks again.
    expect(make(new Mem(), local).token()).toBeNull();
  });

  it("puts the token in localStorage only on 'Remember on this device'", () => {
    const local = new Mem();
    const a = make(new Mem(), local);
    a.setToken("t1", true);
    expect(local.getItem(REMEMBER_KEY)).toBe("t1");
    const tab2 = make(new Mem(), local);
    expect(tab2.token()).toBe("t1");
    expect(tab2.remembered()).toBe(true);
    tab2.setToken("t2", false);
    expect(local.getItem(REMEMBER_KEY)).toBeNull();
  });

  it("moves an old localStorage token to this tab only", () => {
    const session = new Mem();
    const local = new Mem();
    local.setItem(TOKEN_KEY, "old");
    const a = make(session, local);
    expect(a.token()).toBe("old");
    expect(local.getItem(TOKEN_KEY)).toBeNull();
    expect(session.getItem(TOKEN_KEY)).toBe("old");
    expect(a.remembered()).toBe(false);
  });

  it("clear() forgets the token everywhere and asks again", () => {
    const session = new Mem();
    const local = new Mem();
    const a = make(session, local);
    a.setToken("t", true);
    a.clear();
    expect(a.token()).toBeNull();
    expect(a.needsToken()).toBe(true);
    expect(session.m.size + local.m.size).toBe(0);
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
    a.setToken("m");
    expect(a.token()).toBe("m");
  });
});

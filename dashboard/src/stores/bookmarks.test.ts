// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { bookmarkHref, createBookmarksStore } from "./bookmarks";

const origin = "https://terrarium.test";
describe("bookmarks", () => {
  it("normalizes local wiki URLs but keeps other sites external", () => {
    expect(bookmarkHref(`${origin}/p/demo/knowledge/wiki/start#intro`, origin)).toBe("/p/demo/knowledge/wiki/start#intro");
    expect(bookmarkHref("https://other.test/p/demo/knowledge/wiki/start", origin)).toBe("https://other.test/p/demo/knowledge/wiki/start");
    expect(bookmarkHref("https://example.com", origin)).toBe("https://example.com/");
  });
  it("rejects executable, credential-bearing and non-wiki relative URLs", () => {
    for (const value of ["javascript:alert(1)", "data:text/html,hi", "file:///tmp/a", "https://user:pass@example.com", "//example.com", "/settings", "not a url"]) {
      expect(bookmarkHref(value, origin)).toBeNull();
    }
  });
  it("persists, deduplicates, renames and removes bookmarks", () => {
    const values = new Map<string, string>();
    const storage = { getItem: (k: string) => values.get(k) ?? null, setItem: (k: string, v: string) => { values.set(k, v); } };
    const store = createBookmarksStore(storage, origin);
    expect(store.add(" Start ", "/p/demo/knowledge/wiki/start")).toBe(true);
    store.add("Renamed", `${origin}/p/demo/knowledge/wiki/start`);
    expect(store.items()).toEqual([{ title: "Renamed", href: "/p/demo/knowledge/wiki/start" }]);
    const restored = createBookmarksStore(storage, origin);
    expect(restored.has("/p/demo/knowledge/wiki/start")).toBe(true);
    restored.remove(`${origin}/p/demo/knowledge/wiki/start`);
    expect(createBookmarksStore(storage, origin).items()).toEqual([]);
    expect(store.add("", "https://example.com")).toBe(false);
  });
  it("validates stored data and tolerates unavailable storage", () => {
    const stored = [{ title: "Bad", href: "javascript:alert(1)" }, null, { title: "Good", href: "https://example.com" }];
    expect(createBookmarksStore({ getItem: () => JSON.stringify(stored), setItem: () => {} }, origin).items()).toEqual([{ title: "Good", href: "https://example.com/" }]);
    const store = createBookmarksStore({ getItem: () => { throw Error("blocked"); }, setItem: () => { throw Error("blocked"); } }, origin);
    expect(store.add("Good", "https://example.com")).toBe(true);
    expect(store.items()).toHaveLength(1);
  });
  it("migrates once, deduplicates and refreshes across browsers", async () => {
    let rows = [{ title: "Server title", href: "https://existing.test/" }];
    const remote = {
      list: async () => [...rows],
      add: async (title: string, href: string) => (rows = [...rows.filter((b) => b.href !== href), { title, href }]),
      remove: async (href: string) => (rows = rows.filter((b) => b.href !== href)),
    };
    let legacy = JSON.stringify([{ title: "Old title", href: "https://existing.test" }, { title: "Local", href: "https://local.test" }]);
    const first = createBookmarksStore({ getItem: () => legacy, setItem: (_k, v) => { legacy = v; } }, origin, remote);
    const second = createBookmarksStore(null, origin, remote);
    await first.refresh();
    expect(JSON.parse(legacy)).toEqual([]);
    expect(first.items()[0]?.title).toBe("Server title");
    await second.refresh();
    expect(second.items()).toEqual(first.items());
    second.add("New", "https://new.test");
    await second.refresh();
    await first.refresh();
    expect(first.has("https://new.test")).toBe(true);
    first.remove("https://local.test");
    await first.refresh();
    await second.refresh();
    expect(second.has("https://local.test")).toBe(false);
    expect(second.items()).toHaveLength(2);
  });
  it("keeps the legacy list after a failed import and retries", async () => {
    let legacy = JSON.stringify([{ title: "Local", href: "https://local.test" }]);
    let fail = true;
    let rows: { title: string; href: string }[] = [];
    const store = createBookmarksStore({ getItem: () => legacy, setItem: (_k, v) => { legacy = v; } }, origin, {
      list: async () => rows,
      add: async (title, href) => { if (fail) throw Error("offline"); return rows = [{ title, href }]; },
      remove: async () => [],
    });
    await store.refresh();
    expect(JSON.parse(legacy)).toHaveLength(1);
    expect(store.error()).toBeInstanceOf(Error);
    fail = false;
    await store.refresh();
    expect(JSON.parse(legacy)).toEqual([]);
    expect(store.items()).toHaveLength(1);
  });
});

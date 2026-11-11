// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { ApiError } from "../api/http";
import { bookmarkAddError, bookmarkHref, createBookmarksStore, skippedReport } from "./bookmarks";

const origin = "https://terrarium.test";
describe("bookmarks", () => {
  it("edits in place, keeps order and rejects a duplicate destination", () => {
    const store = createBookmarksStore(null, origin);
    store.add("First", "https://first.test");
    store.add("Second", "https://second.test");
    expect(store.add("Renamed", "https://new.test", "https://first.test/")).toBe(true);
    expect(store.items().map((b) => b.title)).toEqual(["Renamed", "Second"]);
    expect(store.add("Duplicate", "https://second.test", "https://new.test/")).toBe(false);
    expect(store.items()).toHaveLength(2);
  });
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
  it("skips and reports a legacy bookmark that the daemon refuses, and later changes go on (TER-257)", async () => {
    const long = "x".repeat(501);
    let legacy = JSON.stringify([{ title: long, href: "https://long.test" }, { title: "Good", href: "https://good.test" }]);
    let rows: { title: string; href: string }[] = [];
    const store = createBookmarksStore({ getItem: () => legacy, setItem: (_k, v) => { legacy = v; } }, origin, {
      list: async () => rows,
      add: async (title, href) => {
        if (title.length > 500) throw new ApiError(422, "A bookmark needs a title (up to 500 bytes)");
        return rows = [...rows, { title, href }];
      },
      remove: async (href) => rows = rows.filter((b) => b.href !== href),
    });
    await store.refresh();
    expect(store.error()).toBeNull();
    expect(JSON.parse(legacy)).toEqual([]);
    expect(store.items().map((b) => b.title)).toEqual(["Good"]);
    expect(store.skipped().map((b) => b.href)).toEqual(["https://long.test/"]);
    expect(skippedReport(store.skipped())?.message).toContain("1 old bookmark of this browser was not imported");
    store.dismissSkipped();
    expect(skippedReport(store.skipped())).toBeNull();
    store.add("New", "https://new.test");
    await store.refresh();
    expect(store.error()).toBeNull();
    expect(store.items().map((b) => b.title)).toEqual(["Good", "New"]);
  });
  it("keeps the bookmarks in this browser when the daemon forbids them, with no banner (TER-257)", async () => {
    const values = new Map<string, string>([["terrarium.bookmarks", JSON.stringify([{ title: "Local", href: "https://local.test" }])]]);
    const storage = { getItem: (k: string) => values.get(k) ?? null, setItem: (k: string, v: string) => { values.set(k, v); } };
    let calls = 0;
    const forbidden = async (): Promise<never> => { calls++; throw new ApiError(403, "bookmarks needs the operator"); };
    const store = createBookmarksStore(storage, origin, { list: forbidden, add: forbidden, remove: forbidden });
    await store.refresh();
    expect(store.error()).toBeNull();
    await store.refresh();
    expect(calls).toBe(1);
    expect(store.add("Second", "https://second.test")).toBe(true);
    await store.refresh();
    expect(calls).toBe(1);
    expect(store.error()).toBeNull();
    expect(createBookmarksStore(storage, origin).items().map((b) => b.title)).toEqual(["Local", "Second"]);
  });
  it("names the reason why a page cannot be pinned (TER-257)", () => {
    expect(bookmarkAddError(" ", "/p/demo/knowledge/wiki/a", origin)).toContain("needs a title");
    expect(bookmarkAddError("é".repeat(251), "/p/demo/knowledge/wiki/a", origin)).toContain("longer than 500 bytes");
    expect(bookmarkAddError("Ok", "/settings", origin)).toContain("cannot be a bookmark");
  });
});

import { describe, expect, it } from "vitest";
import type { KeyValueStorage } from "../stores/auth";
import { collapse, createExpandedStore, expand, EXPANDED_KEY, filterProjects, parseExpanded, projectFromPath, prune, toggle } from "./treeModel";

const memory = (init: Record<string, string> = {}): KeyValueStorage & { data: Record<string, string> } => {
  const data = { ...init };
  return {
    data,
    getItem: (k) => data[k] ?? null,
    setItem: (k, v) => {
      data[k] = v;
    },
    removeItem: (k) => {
      delete data[k];
    },
  };
};

describe("projectFromPath", () => {
  it("reads the project of a /p/ path and decodes it", () => {
    expect(projectFromPath("/p/alpha/board")).toBe("alpha");
    expect(projectFromPath("/p/my%20proj")).toBe("my proj");
    expect(projectFromPath("/p/bad%E0/x")).toBe("bad%E0");
  });
  it("gives null off a project page", () => {
    expect(projectFromPath("/")).toBeNull();
    expect(projectFromPath("/agents")).toBeNull();
    expect(projectFromPath("/projects/alpha")).toBeNull();
  });
});

describe("the open set", () => {
  it("expands, collapses, and toggles with no duplicates", () => {
    expect(expand(["a"], "a")).toEqual(["a"]);
    expect(expand(["a"], "b")).toEqual(["a", "b"]);
    expect(collapse(["a", "b"], "a")).toEqual(["b"]);
    expect(toggle(["a"], "a")).toEqual([]);
    expect(toggle([], "a")).toEqual(["a"]);
  });
  it("prunes gone projects, but keeps all while the list is not loaded", () => {
    expect(prune(["a", "gone"], ["a", "b"])).toEqual(["a"]);
    expect(prune(["a", "gone"], [])).toEqual(["a", "gone"]);
  });
  it("reads only a JSON array of strings from storage", () => {
    expect(parseExpanded(null)).toEqual([]);
    expect(parseExpanded("not json")).toEqual([]);
    expect(parseExpanded('{"a":1}')).toEqual([]);
    expect(parseExpanded('["a", 2, "b"]')).toEqual(["a", "b"]);
  });
});

describe("filterProjects", () => {
  const ps = [{ name: "Alpha" }, { name: "beta" }, { name: "alphabet" }];
  it("matches part of the name, case-insensitive", () => {
    expect(filterProjects(ps, "ALPHA").map((p) => p.name)).toEqual(["Alpha", "alphabet"]);
    expect(filterProjects(ps, " bet ").map((p) => p.name)).toEqual(["beta", "alphabet"]);
  });
  it("keeps all for an empty query and none for no match", () => {
    expect(filterProjects(ps, "  ")).toHaveLength(3);
    expect(filterProjects(ps, "zzz")).toEqual([]);
  });
});

describe("createExpandedStore", () => {
  it("persists a toggle and reads it back in a new store", () => {
    const s = memory();
    const a = createExpandedStore(s);
    a.toggle("alpha");
    a.toggle("beta");
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual(["alpha", "beta"]);
    a.toggle("alpha");
    const b = createExpandedStore(s);
    expect(b.list()).toEqual(["beta"]);
    expect(b.isOpen("beta")).toBe(true);
    expect(b.isOpen("alpha")).toBe(false);
  });

  it("opens the URL project on navigation, and lets you close it until the URL moves to another project", () => {
    const st = createExpandedStore(memory());
    st.follow("/p/alpha/board");
    expect(st.isOpen("alpha")).toBe(true);
    st.close("alpha");
    st.follow("/p/alpha/mail"); // same project: your close stays
    expect(st.isOpen("alpha")).toBe(false);
    st.follow("/p/beta/board");
    expect(st.list()).toEqual(["beta"]);
    st.follow("/agents");
    st.follow("/p/alpha/wiki"); // back to alpha: open again
    expect(st.list()).toEqual(["beta", "alpha"]);
  });

  it("keeps several projects open at once and prunes gone ones", () => {
    const s = memory({ [EXPANDED_KEY]: '["a","b","gone"]' });
    const st = createExpandedStore(s);
    st.prune(["a", "b", "c"]);
    expect(st.list()).toEqual(["a", "b"]);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual(["a", "b"]);
  });

  it("works when storage throws", () => {
    const bad: KeyValueStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
      removeItem: () => undefined,
    };
    const st = createExpandedStore(bad);
    expect(st.list()).toEqual([]);
    st.toggle("a");
    expect(st.isOpen("a")).toBe(true);
  });
});

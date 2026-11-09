import { describe, expect, it } from "vitest";
import type { KeyValueStorage } from "../stores/auth";
import {
  agentFromPath,
  BOOKMARKS_KEY,
  collapse,
  createExpandedStore,
  expand,
  EXPANDED_KEY,
  filterProjects,
  inProjects,
  OLD_AGENTS_ROOT,
  parseExpanded,
  projectFromPath,
  projectKey,
  PROJECTS_ROOT,
  projectTreeKeys,
  prune,
  ROOT_CLOSED,
  toggle,
} from "./treeModel";

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

  it("drops old project, Agents, and Repos open names, also before projects load", () => {
    const old = ["alpha", "alpha/agents", "alpha/repos", "gone"];
    expect(prune(old, ["alpha", "beta"])).toEqual([]);
    expect(prune(old, [])).toEqual([]);
    expect(prune([ROOT_CLOSED, ...old, BOOKMARKS_KEY], ["alpha"])).toEqual([ROOT_CLOSED, BOOKMARKS_KEY]);
  });

  it("keeps open folders while the folders are not loaded, and drops gone ones after", () => {
    expect(prune(["alpha", "folder:F1", "folder:F2"], ["alpha"])).toEqual(["folder:F1", "folder:F2"]);
    expect(prune(["alpha", "folder:F1", "folder:F2"], ["alpha"], ["F1"])).toEqual(["folder:F1"]);
    expect(prune(["folder:F1"], [], [])).toEqual([]);
  });

  it("drops the old Agents root from a stored list", () => {
    expect(prune(["alpha", OLD_AGENTS_ROOT], [])).toEqual([]);
    expect(prune([OLD_AGENTS_ROOT, "alpha", ROOT_CLOSED], ["alpha"])).toEqual([ROOT_CLOSED]);
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

describe("agent paths", () => {
  it("reads the agent of an /agents/ path", () => {
    expect(agentFromPath("/agents/01JB0LEAD")).toBe("01JB0LEAD");
    expect(agentFromPath("/agents/terrarium%2Flead")).toBe("terrarium/lead");
    expect(agentFromPath("/agents")).toBeNull();
    expect(agentFromPath("/p/a/agents")).toBeNull();
  });
});

describe("createExpandedStore", () => {
  it("persists a toggle and reads it back in a new store", () => {
    const s = memory();
    const a = createExpandedStore(s);
    a.toggle("folder:a");
    a.toggle("folder:b");
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual(["folder:a", "folder:b"]);
    a.toggle("folder:a");
    const b = createExpandedStore(s);
    expect(b.list()).toEqual(["folder:b"]);
    expect(b.isOpen("folder:b")).toBe(true);
    expect(b.isOpen("folder:a")).toBe(false);
  });

  it("opens only the Projects root on project navigation", () => {
    const st = createExpandedStore(memory());
    st.follow("/p/alpha/board");
    expect(st.list()).toEqual([]);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    st.close(PROJECTS_ROOT);
    st.follow("/p/alpha/logs/mail");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/p/beta/board");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    expect(st.list()).toEqual([]);
  });

  it("prunes old project state out of storage", () => {
    const s = memory({ [EXPANDED_KEY]: '["a","a/agents","a/repos","gone","folder:F1"]' });
    const st = createExpandedStore(s);
    st.prune(["a", "b", "c"], ["F1"]);
    expect(st.list()).toEqual(["folder:F1"]);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual(["folder:F1"]);
  });

  it("has the Projects root open by default, also for a list stored before the root", () => {
    expect(createExpandedStore(memory()).isOpen(PROJECTS_ROOT)).toBe(true);
    expect(createExpandedStore(memory({ [EXPANDED_KEY]: '["a"]' })).isOpen(PROJECTS_ROOT)).toBe(true);
  });

  it("persists a closed root, keeps it through a prune, and opens it again on project pages", () => {
    const s = memory();
    const st = createExpandedStore(s);
    st.toggle(PROJECTS_ROOT);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual([ROOT_CLOSED]);
    st.prune(["a"]);
    expect(createExpandedStore(s).isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/inbox");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/projects");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    st.close(PROJECTS_ROOT);
    st.follow("/projects/");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/p/alpha/board");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
  });

  it("cleans the old Agents root and project names out of storage on the first prune", () => {
    const s = memory({ [EXPANDED_KEY]: JSON.stringify(["a", OLD_AGENTS_ROOT]) });
    const st = createExpandedStore(s);
    st.prune([]);
    expect(st.list()).toEqual([]);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual([]);
  });

  it("does not open anything on the agent list, which is not in the tree", () => {
    const st = createExpandedStore(memory({ [EXPANDED_KEY]: JSON.stringify([ROOT_CLOSED]) }));
    st.follow("/agents");
    expect(st.list()).toEqual([ROOT_CLOSED]);
  });

  it("opens the root on an agent page after its project loads", () => {
    const projectOf = (id: string) => ({ a1: "alpha", "beta/w": "beta" })[id];
    let loaded = false;
    const later = (id: string) => (loaded ? projectOf(id) : undefined);
    const st = createExpandedStore(memory({ [EXPANDED_KEY]: JSON.stringify([ROOT_CLOSED]) }));
    st.follow("/agents/a1", later);
    expect(st.list()).toEqual([ROOT_CLOSED]);
    loaded = true;
    st.follow("/agents/a1", later);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    st.close(PROJECTS_ROOT);
    st.follow("/agents/a1", later);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/agents/beta%2Fw", later);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    expect(st.list()).toEqual([]);
  });

  it("knows the pages under the root", () => {
    expect(inProjects("/projects")).toBe(true);
    expect(inProjects("/projects/")).toBe(true);
    expect(inProjects("/p/a/board")).toBe(true);
    expect(inProjects("/")).toBe(false);
    expect(inProjects("/projectsx")).toBe(false);
    expect(inProjects("/settings/hosts")).toBe(false);
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
    st.toggle("folder:a");
    expect(st.isOpen("folder:a")).toBe(true);
  });
});

describe("projectTreeKeys", () => {
  it("has one row per project and no page, agent, or repo rows", () => {
    expect(projectTreeKeys({ rootKey: "root", rootOpen: true, projects: ["alpha", "beta"] })).toEqual([
      "root",
      projectKey("alpha"),
      projectKey("beta"),
    ]);
    expect(projectTreeKeys({ rootKey: "root", rootOpen: false, projects: ["alpha"] })).toEqual(["root"]);
  });
});

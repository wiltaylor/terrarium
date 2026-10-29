import { describe, expect, it } from "vitest";
import type { KeyValueStorage } from "../stores/auth";
import {
  agentFromPath,
  AGENTS_ROOT,
  collapse,
  createExpandedStore,
  expand,
  EXPANDED_KEY,
  filterAgents,
  filterProjects,
  inAgents,
  inProjects,
  parseExpanded,
  projectFromPath,
  PROJECTS_ROOT,
  prune,
  ROOT_CLOSED,
  sortAgents,
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
  it("prunes gone projects, but keeps all while the list is not loaded", () => {
    expect(prune(["a", "gone"], ["a", "b"])).toEqual(["a"]);
    expect(prune([ROOT_CLOSED, AGENTS_ROOT, "gone"], ["a"])).toEqual([ROOT_CLOSED, AGENTS_ROOT]);
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

describe("the agent tree", () => {
  const as = [
    { name: "lead", project: "webshop" },
    { name: "runner", project: "terrarium" },
    { name: "lead", project: "terrarium" },
    { name: "css-worker", project: "webshop" },
  ];
  it("sorts by project, then by name, and does not change the input", () => {
    expect(sortAgents(as).map((a) => `${a.project}/${a.name}`)).toEqual(["terrarium/lead", "terrarium/runner", "webshop/css-worker", "webshop/lead"]);
    expect(as[0]!.project).toBe("webshop");
  });
  it("filters on the project and the name, case-insensitive", () => {
    expect(filterAgents(as, "LEAD").map((a) => `${a.project}/${a.name}`)).toEqual(["webshop/lead", "terrarium/lead"]);
    expect(filterAgents(as, " shop ")).toHaveLength(2);
    expect(filterAgents(as, "terrarium/r").map((a) => a.name)).toEqual(["runner"]);
    expect(filterAgents(as, "")).toHaveLength(4);
    expect(filterAgents(as, "zzz")).toEqual([]);
  });
  it("reads the agent of an /agents/ path, and knows the pages under the root", () => {
    expect(agentFromPath("/agents/01JB0LEAD")).toBe("01JB0LEAD");
    expect(agentFromPath("/agents/terrarium%2Flead")).toBe("terrarium/lead");
    expect(agentFromPath("/agents")).toBeNull();
    expect(agentFromPath("/p/a/agents")).toBeNull();
    expect(inAgents("/agents")).toBe(true);
    expect(inAgents("/agents/")).toBe(true);
    expect(inAgents("/agents/x")).toBe(true);
    expect(inAgents("/agentsx")).toBe(false);
    expect(inAgents("/p/a/agents")).toBe(false);
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
    st.follow("/inbox");
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

  it("has the Projects root open by default, also for a list stored before the root", () => {
    expect(createExpandedStore(memory()).isOpen(PROJECTS_ROOT)).toBe(true);
    expect(createExpandedStore(memory({ [EXPANDED_KEY]: '["a"]' })).isOpen(PROJECTS_ROOT)).toBe(true);
  });

  it("persists a closed root, keeps it through a prune, and opens it again on the project pages", () => {
    const s = memory();
    const st = createExpandedStore(s);
    st.toggle(PROJECTS_ROOT);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual([ROOT_CLOSED]);
    st.prune(["a"]);
    expect(createExpandedStore(s).isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/agents"); // not a project page: the root stays closed
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/projects");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    st.close(PROJECTS_ROOT);
    st.follow("/projects/"); // the same place: your close stays
    expect(st.isOpen(PROJECTS_ROOT)).toBe(false);
    st.follow("/p/alpha/board");
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    expect(st.isOpen("alpha")).toBe(true);
  });

  it("has the Agents root closed by default, persists it open, and keeps the Projects root apart", () => {
    const s = memory();
    const st = createExpandedStore(s);
    expect(st.isOpen(AGENTS_ROOT)).toBe(false);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    st.toggle(AGENTS_ROOT);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual([AGENTS_ROOT]);
    st.close(PROJECTS_ROOT);
    st.prune(["a"]);
    const back = createExpandedStore(s);
    expect(back.isOpen(AGENTS_ROOT)).toBe(true);
    expect(back.isOpen(PROJECTS_ROOT)).toBe(false);
    back.toggle(AGENTS_ROOT);
    expect(createExpandedStore(s).isOpen(AGENTS_ROOT)).toBe(false);
  });

  it("opens the Agents root on the agent pages, and lets you close it until the URL moves to another agent", () => {
    const st = createExpandedStore(memory());
    st.follow("/p/alpha/agents"); // the project page: not the Agents root
    expect(st.isOpen(AGENTS_ROOT)).toBe(false);
    st.follow("/agents");
    expect(st.isOpen(AGENTS_ROOT)).toBe(true);
    expect(st.isOpen("alpha")).toBe(true);
    st.close(AGENTS_ROOT);
    st.follow("/agents/"); // the same place: your close stays
    expect(st.isOpen(AGENTS_ROOT)).toBe(false);
    st.follow("/agents/a1");
    expect(st.isOpen(AGENTS_ROOT)).toBe(true);
    st.close(AGENTS_ROOT);
    st.follow("/agents/a1"); // the same agent: your close stays
    expect(st.isOpen(AGENTS_ROOT)).toBe(false);
    st.follow("/agents/a2");
    expect(st.isOpen(AGENTS_ROOT)).toBe(true);
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
    st.toggle("a");
    expect(st.isOpen("a")).toBe(true);
  });
});

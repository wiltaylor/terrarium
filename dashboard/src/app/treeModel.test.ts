import { describe, expect, it } from "vitest";
import type { KeyValueStorage } from "../stores/auth";
import {
  agentFromPath,
  collapse,
  createExpandedStore,
  expand,
  EXPANDED_KEY,
  filterAgents,
  filterProjects,
  inProjects,
  itemKey,
  OLD_AGENTS_ROOT,
  parseExpanded,
  projectAgentKey,
  projectAgentsFromPath,
  projectAgentsNode,
  projectFromPath,
  projectKey,
  projectOfAgentsNode,
  PROJECTS_ROOT,
  projectOfNode,
  projectRepoKey,
  projectReposFromPath,
  projectReposNode,
  projectTreeKeys,
  prune,
  filterRepos,
  repoFromPath,
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
    expect(prune([ROOT_CLOSED, "gone"], ["a"])).toEqual([ROOT_CLOSED]);
    expect(prune(["a", "gone"], [])).toEqual(["a", "gone"]);
  });
  it("drops the old Agents root from a stored list, also before the projects are loaded", () => {
    expect(prune(["a", OLD_AGENTS_ROOT], [])).toEqual(["a"]);
    expect(prune([OLD_AGENTS_ROOT, "a", ROOT_CLOSED], ["a"])).toEqual(["a", ROOT_CLOSED]);
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
    st.follow("/p/alpha/logs/mail"); // same project: your close stays
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
    st.follow("/inbox"); // not a project page: the root stays closed
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

  it("cleans the old Agents root out of storage on the first prune", () => {
    const s = memory({ [EXPANDED_KEY]: JSON.stringify(["a", OLD_AGENTS_ROOT]) });
    const st = createExpandedStore(s);
    st.prune([]);
    expect(st.list()).toEqual(["a"]);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual(["a"]);
  });

  it("does not open anything on the agent list, which is not in the tree", () => {
    const st = createExpandedStore(memory({ [EXPANDED_KEY]: JSON.stringify([ROOT_CLOSED]) }));
    st.follow("/agents");
    expect(st.list()).toEqual([ROOT_CLOSED]);
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

describe("the Agents node of a project", () => {
  const projectOf = (id: string) => ({ a1: "alpha", "beta/w": "beta" })[id];

  it("names the node apart from the projects and the roots", () => {
    expect(projectAgentsNode("alpha")).toBe("alpha/agents");
    expect(projectOfAgentsNode("alpha/agents")).toBe("alpha");
    expect(projectOfAgentsNode(OLD_AGENTS_ROOT)).toBeNull();
    expect(projectOfAgentsNode("alpha")).toBeNull();
    expect(projectOfAgentsNode(PROJECTS_ROOT)).toBeNull();
  });

  it("reads the project from its agent list or from the page of one of its agents", () => {
    expect(projectAgentsFromPath("/p/alpha/agents", projectOf)).toBe("alpha");
    expect(projectAgentsFromPath("/p/alpha/agents/", projectOf)).toBe("alpha");
    expect(projectAgentsFromPath("/p/alpha/board", projectOf)).toBeNull();
    expect(projectAgentsFromPath("/agents/a1", projectOf)).toBe("alpha");
    expect(projectAgentsFromPath("/agents/beta%2Fw", projectOf)).toBe("beta");
    // Not loaded yet, or not an agent page.
    expect(projectAgentsFromPath("/agents/zz", projectOf)).toBeNull();
    expect(projectAgentsFromPath("/agents", projectOf)).toBeNull();
  });

  it("is closed by default, opens on the agent list of the project once, and persists", () => {
    const s = memory();
    const st = createExpandedStore(s);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(false);
    st.follow("/p/alpha/agents", projectOf);
    expect(st.isOpen("alpha")).toBe(true);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(true);
    st.close(projectAgentsNode("alpha"));
    st.follow("/p/alpha/agents/", projectOf); // the same place: your close stays
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(false);
    st.follow("/p/alpha/board", projectOf); // another page: the node stays closed
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(false);
    st.follow("/p/alpha/agents", projectOf);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(true);
    expect(createExpandedStore(s).isOpen(projectAgentsNode("alpha"))).toBe(true);
  });

  it("reveals an agent page: the Projects root, the project of the agent, and its Agents node", () => {
    const s = memory({ [EXPANDED_KEY]: JSON.stringify([ROOT_CLOSED]) });
    const st = createExpandedStore(s);
    st.follow("/agents/a1", projectOf);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    expect(st.isOpen("alpha")).toBe(true);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(true);
    expect(JSON.parse(s.data[EXPANDED_KEY]!)).toEqual(["alpha", projectAgentsNode("alpha")]);
    // The `project/name` form works too.
    st.follow("/agents/beta%2Fw", projectOf);
    expect(st.isOpen("beta")).toBe(true);
    expect(st.isOpen(projectAgentsNode("beta"))).toBe(true);
  });

  it("waits for the agents before it reveals an agent page", () => {
    let loaded = false;
    const later = (id: string) => (loaded ? projectOf(id) : undefined);
    const st = createExpandedStore(memory({ [EXPANDED_KEY]: JSON.stringify([ROOT_CLOSED]) }));
    st.follow("/agents/a1", later);
    expect(st.list()).toEqual([ROOT_CLOSED]);
    loaded = true;
    st.follow("/agents/a1", later);
    expect(st.isOpen(PROJECTS_ROOT)).toBe(true);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(true);
  });

  it("lets you close the nodes of an agent page until the URL moves to another agent", () => {
    const st = createExpandedStore(memory());
    st.follow("/agents/a1", projectOf);
    st.close(projectAgentsNode("alpha"));
    st.close("alpha");
    st.follow("/agents/a1", projectOf); // the same agent: your close stays
    expect(st.isOpen("alpha")).toBe(false);
    st.follow("/agents/beta%2Fw", projectOf);
    st.follow("/agents/a1", projectOf); // back to a1: open again
    expect(st.isOpen("alpha")).toBe(true);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(true);
  });

  it("is pruned with its project only", () => {
    const list = ["alpha", projectAgentsNode("alpha"), projectAgentsNode("gone")];
    expect(prune(list, ["alpha"])).toEqual(["alpha", projectAgentsNode("alpha")]);
  });

  it("puts the agents of an open node after its row, in screen order", () => {
    const open = new Set(["alpha", projectAgentsNode("alpha"), projectAgentsNode("beta")]);
    const keys = projectTreeKeys({
      rootKey: "root",
      rootOpen: true,
      projects: ["alpha", "beta"],
      items: ["board", "schedules", "agents"],
      isOpen: (n) => open.has(n),
      agentsOf: (p) => (p === "alpha" ? ["a1", "a2"] : ["b1"]),
    });
    // The Agents node is last, so its agents come before the next project. beta is closed, so its open Agents node shows nothing.
    expect(keys).toEqual([
      "root",
      projectKey("alpha"),
      itemKey("alpha", "board"),
      itemKey("alpha", "schedules"),
      itemKey("alpha", "agents"),
      projectAgentKey("a1"),
      projectAgentKey("a2"),
      projectKey("beta"),
    ]);
    expect(projectTreeKeys({ rootKey: "root", rootOpen: false, projects: ["alpha"], items: ["agents"], isOpen: () => true, agentsOf: () => ["a1"] })).toEqual(["root"]);
  });
});

describe("the Repos node of a project", () => {
  it("names the node apart from the projects, the roots, and the Agents node", () => {
    expect(projectReposNode("alpha")).toBe("alpha/repos");
    expect(projectOfNode("alpha/repos")).toBe("alpha");
    expect(projectOfNode("alpha/agents")).toBe("alpha");
    expect(projectOfNode("alpha")).toBeNull();
    expect(projectOfNode(PROJECTS_ROOT)).toBeNull();
    expect(projectOfNode(OLD_AGENTS_ROOT)).toBeNull();
  });

  it("reads the project from its repo list or from a repo page, and the repo from a repo page", () => {
    expect(projectReposFromPath("/p/alpha/repos")).toBe("alpha");
    expect(projectReposFromPath("/p/alpha/repos/infra")).toBe("alpha");
    expect(projectReposFromPath("/p/alpha/reposx")).toBeNull();
    expect(projectReposFromPath("/p/alpha/board")).toBeNull();
    expect(projectReposFromPath("/projects")).toBeNull();
    expect(repoFromPath("/p/alpha/repos/my%20repo")).toBe("my repo");
    expect(repoFromPath("/p/alpha/repos")).toBeNull();
  });

  it("filters the repos on the name, case-insensitive", () => {
    const repos = [{ name: "infra" }, { name: "Web" }];
    expect(filterRepos(repos, "WE")).toEqual([{ name: "Web" }]);
    expect(filterRepos(repos, " ")).toEqual(repos);
  });

  it("is closed by default, opens on a repo page of the project once, and persists", () => {
    const s = memory();
    const st = createExpandedStore(s);
    expect(st.isOpen(projectReposNode("alpha"))).toBe(false);
    st.follow("/p/alpha/repos/infra");
    expect(st.isOpen("alpha")).toBe(true);
    expect(st.isOpen(projectReposNode("alpha"))).toBe(true);
    expect(st.isOpen(projectAgentsNode("alpha"))).toBe(false);
    st.close(projectReposNode("alpha"));
    st.follow("/p/alpha/repos/web"); // the same project: your close stays
    expect(st.isOpen(projectReposNode("alpha"))).toBe(false);
    st.follow("/p/alpha/board");
    st.follow("/p/alpha/repos");
    expect(st.isOpen(projectReposNode("alpha"))).toBe(true);
    expect(createExpandedStore(s).isOpen(projectReposNode("alpha"))).toBe(true);
  });

  it("is pruned with its project only", () => {
    expect(prune(["alpha", "alpha/repos", "beta/repos"], ["alpha"])).toEqual(["alpha", "alpha/repos"]);
  });

  it("puts the repos of an open node after its row, before the next page", () => {
    const open = new Set(["alpha", "alpha/repos", "alpha/agents"]);
    const keys = projectTreeKeys({
      rootKey: "root",
      rootOpen: true,
      projects: ["alpha"],
      items: ["board", "repos", "logs", "agents"],
      isOpen: (n) => open.has(n),
      agentsOf: () => ["a1"],
      reposOf: () => ["infra", "web"],
    });
    expect(keys).toEqual([
      "root",
      projectKey("alpha"),
      itemKey("alpha", "board"),
      itemKey("alpha", "repos"),
      projectRepoKey("alpha", "infra"),
      projectRepoKey("alpha", "web"),
      itemKey("alpha", "logs"),
      itemKey("alpha", "agents"),
      projectAgentKey("a1"),
    ]);
  });
});

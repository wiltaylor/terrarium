// @vitest-environment jsdom
// The project tree with two projects: each project row has a health dot and
// a cog that links the project settings screen and is not a Tab stop of the
// tree. The Agents and Repos nodes are not links, and each has a "+".
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";

// The agents, in a signal: a change is a live status update.
const [agentList, setAgentList] = createSignal<{ id: string; project: string; name: string; status: string }[]>([]);

vi.mock("../stores", () => ({
  // No folders: the caller is not the operator or a sysadmin (TER-48).
  projectFolders: {
    available: () => false,
    folders: () => [],
    placements: () => [],
    error: () => null,
    clearError: () => {},
    refresh: async () => {},
  },
  projects: {
    projects: () => [{ name: "alpha" }, { name: "my app" }],
    selected: () => null,
    select: () => {},
  },
  agents: {
    byId: () => undefined,
    inProject: (p: string) => agentList().filter((a) => a.project === p),
  },
  repos: {
    load: () => {},
    loaded: () => true,
    inProject: (p: string) => (p === "alpha" ? [{ id: "r1", project: "alpha", name: "infra", url: "https://x/infra.git" }] : []),
    added: () => {},
  },
}));

const { ProjectTree } = await import("./ProjectTree");
const { expanded } = await import("./TreeRoot");

let dispose: (() => void) | undefined;
afterEach(() => {
  setAgentList([]);
  // The open set is global: each test starts with its nodes closed.
  for (const n of expanded.list()) if (/\/(agents|repos)$/.test(n)) expanded.close(n);
  dispose?.();
  document.body.innerHTML = "";
});

const mount = (path: string) => {
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  history.set({ value: path });
  // No JSX in a .test.ts file: the same tree as <MemoryRouter><Route path="*" component={ProjectTree} /></MemoryRouter>.
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "*", component: ProjectTree });
        },
      }),
    el,
  );
  return el;
};

describe("ProjectTree", () => {
  it("has a cog on each project row that links the project settings screen", () => {
    const el = mount("/projects");
    const cogs = Array.from(el.querySelectorAll<HTMLAnchorElement>("a.tree-cog"));
    expect(cogs.map((a) => a.getAttribute("href"))).toEqual(["/p/alpha/settings", "/p/my%20app/settings"]);
    expect(cogs.map((a) => a.getAttribute("aria-label"))).toEqual(["Settings of alpha", "Settings of my app"]);
    expect(cogs.map((a) => a.title)).toEqual(["Settings of alpha", "Settings of my app"]);
    // In the project row, but not a tree row: the roving tabindex keeps one Tab stop.
    for (const a of cogs) {
      expect(a.closest('[role="treeitem"]')?.getAttribute("data-key")).toMatch(/^p:/);
      expect(a.tabIndex).toBe(-1);
      expect(a.querySelector("svg")?.getAttribute("aria-hidden")).toBe("true");
    }
  });

  it("marks the cog of the project whose settings screen is open, on each tab", () => {
    for (const tab of ["general", "secrets", "templates"]) {
      const el = mount(`/p/alpha/settings/${tab}`);
      const active = Array.from(el.querySelectorAll("a.tree-cog.active")).map((a) => a.getAttribute("aria-label"));
      expect(active).toEqual(["Settings of alpha"]);
      dispose?.();
      document.body.innerHTML = "";
    }
  });

  it("has no Templates, Secrets, or Settings row under an open project", () => {
    const el = mount("/p/alpha/board");
    const rows = Array.from(el.querySelectorAll<HTMLElement>('[role="group"] a[role="treeitem"]')).map((a) => a.textContent);
    expect(rows).toContain("Board");
    for (const gone of ["Templates", "Secrets", "Settings & repos"]) expect(rows).not.toContain(gone);
  });

  it("shows the pages of an open project in order, with the Agents node last", () => {
    const el = mount("/p/alpha/board");
    const group = el.querySelector<HTMLElement>('[data-project="alpha"] > [role="group"]')!;
    const keys = Array.from(group.querySelectorAll<HTMLElement>('[role="treeitem"][data-key^="i:"]')).map((r) => r.dataset.key);
    expect(keys).toEqual(["dashboard", "pipelines", "calendar", "board", "repos", "logs", "knowledge", "agents"].map((p) => `i:alpha/${p}`));
  });

  it("keeps the Board row active on the Schedules tab of the board", () => {
    const el = mount("/p/alpha/board/schedules");
    const active = Array.from(el.querySelectorAll<HTMLElement>('[role="treeitem"][data-key^="i:"].active')).map((r) => r.dataset.key);
    expect(active).toEqual(["i:alpha/board"]);
  });

  it("has a health dot on each project row that follows the agents, with the counts in the label", () => {
    setAgentList([
      { id: "1", project: "alpha", name: "a", status: "idle" },
      { id: "2", project: "alpha", name: "b", status: "working" },
    ]);
    const el = mount("/projects");
    const row = (p: string) => el.querySelector<HTMLElement>(`[role="treeitem"][data-project="${p}"][data-key^="p:"]`)!;
    const dot = (p: string) => row(p).querySelector<HTMLElement>(":scope > .tree-row > .project-dot")!;
    expect(dot("alpha").className).toBe("project-dot health-yellow");
    expect(dot("alpha").getAttribute("aria-hidden")).toBe("true");
    expect(row("alpha").getAttribute("aria-label")).toBe("alpha: 1 working, 1 idle");
    expect(row("alpha").querySelector(".tree-name")?.getAttribute("title")).toBe("alpha: 1 working, 1 idle");
    expect(dot("my app").className).toBe("project-dot health-none");
    expect(row("my app").getAttribute("aria-label")).toBe("my app: no agents");

    // A live change: one agent goes blocked, and the other ends its turn.
    setAgentList([
      { id: "1", project: "alpha", name: "a", status: "blocked" },
      { id: "2", project: "alpha", name: "b", status: "idle" },
      { id: "3", project: "my app", name: "c", status: "idle" },
    ]);
    expect(dot("alpha").className).toBe("project-dot health-red");
    expect(row("alpha").getAttribute("aria-label")).toBe("alpha: 1 blocked, 1 idle");
    expect(dot("my app").className).toBe("project-dot health-green");
    // The blocked count chip stays.
    expect(row("alpha").querySelector(".nav-count.warn")?.textContent).toBe("1");
  });

  it("makes the Agents and Repos nodes rows that only toggle, not links", () => {
    const el = mount("/p/alpha/board");
    for (const path of ["agents", "repos"]) {
      const row = el.querySelector<HTMLElement>(`[data-key="i:alpha/${path}"]`)!;
      expect(row.tagName).toBe("DIV");
      expect(row.getAttribute("role")).toBe("treeitem");
      expect(row.hasAttribute("href")).toBe(false);
      expect(row.closest("a")).toBeNull();
      expect(row.getAttribute("aria-expanded")).toBe("false");
      row.click();
      expect(row.getAttribute("aria-expanded")).toBe("true");
      row.click();
      expect(row.getAttribute("aria-expanded")).toBe("false");
    }
    expect(el.querySelector('a[href="/p/alpha/agents"]')).toBeNull();
    expect(el.querySelector('a[href="/p/alpha/repos"]')).toBeNull();
  });

  it("shows the repos of an open Repos node as links, and marks the repo of the URL", () => {
    const el = mount("/p/alpha/repos/infra");
    const node = el.querySelector<HTMLElement>('[data-key="i:alpha/repos"]')!;
    expect(node.getAttribute("aria-expanded")).toBe("true");
    const repo = el.querySelector<HTMLAnchorElement>('[data-key="pr:alpha/infra"]')!;
    expect(repo.getAttribute("href")).toBe("/p/alpha/repos/infra");
    expect(repo.getAttribute("role")).toBe("treeitem");
    expect(repo.getAttribute("aria-current")).toBe("page");
    expect(repo.textContent).toBe("infra");
  });

  it("has a + on the Agents and Repos nodes that is not a Tab stop and does not toggle the node", () => {
    const el = mount("/p/alpha/board");
    const add = Array.from(el.querySelectorAll<HTMLButtonElement>('[data-project="alpha"] button.tree-add'));
    expect(add.map((b) => b.getAttribute("aria-label"))).toEqual(["Add a repo to alpha", "Add an agent to alpha"]);
    for (const b of add) {
      expect(b.tabIndex).toBe(-1);
      expect(b.title).toBe(`${b.getAttribute("aria-label")} (+)`);
      expect(b.closest('[role="treeitem"]')?.getAttribute("aria-keyshortcuts")).toBe("+");
    }
    const repos = el.querySelector<HTMLElement>('[data-key="i:alpha/repos"]')!;
    add[0]!.click();
    expect(repos.getAttribute("aria-expanded")).toBe("false");
    const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
    expect(dialog.getAttribute("aria-label")).toBe("Add a repo to alpha");
    expect(dialog.querySelector('form[aria-label="Add a repo"]')).not.toBeNull();
  });

  it("opens the add dialog of a node with the + key", () => {
    const el = mount("/p/alpha/board");
    const repos = el.querySelector<HTMLElement>('[data-key="i:alpha/repos"]')!;
    repos.dispatchEvent(new KeyboardEvent("keydown", { key: "+", bubbles: true }));
    expect(document.querySelector('[role="dialog"]')?.getAttribute("aria-label")).toBe("Add a repo to alpha");
    // Enter toggles the node; it does not go to a page.
    repos.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(repos.getAttribute("aria-expanded")).toBe("true");
  });
});

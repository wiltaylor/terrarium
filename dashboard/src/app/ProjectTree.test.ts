// @vitest-environment jsdom
// The project tree with two projects: one linked row per project, with a
// health dot, a blocked count, and a settings cog that is not a Tab stop.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createBookmarksStore } from "../stores/bookmarks";
const bookmarks = createBookmarksStore(null, window.location.origin);

// The agents, in a signal: a change is a live status update.
const [agentList, setAgentList] = createSignal<{ id: string; project: string; name: string; status: string }[]>([]);

vi.mock("../stores", () => ({
  bookmarks,
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
    byId: (id: string) => agentList().find((a) => a.id === id || `${a.project}/${a.name}` === id),
    inProject: (p: string) => agentList().filter((a) => a.project === p),
  },
}));

const { ProjectTree } = await import("./ProjectTree");
const { expanded } = await import("./TreeRoot");

let dispose: (() => void) | undefined;
let lastHistory: ReturnType<typeof createMemoryHistory>;
afterEach(() => {
  setAgentList([]);
  expanded.close("/bookmarks");
  for (const b of bookmarks.items()) bookmarks.remove(b.href);
  dispose?.();
  document.body.innerHTML = "";
});

const mount = (path: string) => {
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  lastHistory = history;
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

const projectRow = (el: HTMLElement, name: string) => el.querySelector<HTMLElement>(`[role="treeitem"][data-key="p:${name}"]`)!;

describe("ProjectTree", () => {
  it.each(["Enter", "name link", "row"])("opens an encoded project URL with %s", async (activation) => {
    const el = mount("/projects");
    const row = el.querySelector<HTMLElement>('[role="treeitem"][data-project="my app"][data-key^="p:"]')!;
    if (activation === "Enter") row.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    else if (activation === "name link") row.querySelector<HTMLAnchorElement>("a.tree-project-link")!.click();
    else row.querySelector<HTMLElement>(":scope > .tree-row")!.click();
    await vi.waitFor(() => expect(lastHistory.get()).toBe("/p/my%20app"));
    expect(row.getAttribute("aria-selected")).toBe("true");
  });

  it("highlights the decoded project row of the URL", () => {
    const el = mount("/p/my%20app/board");
    const row = el.querySelector<HTMLElement>('[role="treeitem"][data-project="my app"][data-key^="p:"]')!;
    expect(row.getAttribute("aria-selected")).toBe("true");
    expect(row.querySelector(":scope > .tree-row")!.classList.contains("current")).toBe(true);
    expect(row.querySelector("a.tree-project-link")?.getAttribute("aria-current")).toBe("page");
  });

  it("navigates the Bookmarks node and wiki links with arrow keys", () => {
    bookmarks.add("Wiki", "/p/alpha/knowledge/wiki/a%20b");
    const el = mount("/projects");
    const row = el.querySelector<HTMLElement>('[data-key="/bookmarks"]')!;
    expect(row.closest('[role="tree"]')?.getAttribute("aria-label")).toBe("Projects");
    const key = (target: HTMLElement, value: string) => target.dispatchEvent(new KeyboardEvent("keydown", { key: value, bubbles: true, cancelable: true }));
    row.focus();
    key(row, "ArrowRight");
    expect(row.getAttribute("aria-expanded")).toBe("true");
    key(row, "ArrowDown");
    const link = el.querySelector<HTMLElement>('[data-key^="bookmark:"]')!;
    expect(document.activeElement).toBe(link);
    key(link, "ArrowLeft");
    expect(document.activeElement).toBe(row);
    key(row, "ArrowLeft");
    expect(row.getAttribute("aria-expanded")).toBe("false");
    key(row, "ArrowLeft");
    expect(document.activeElement?.getAttribute("data-key")).toBe("root");
  });

  it("shows one linked row per project and no page, agent, or repo sub rows", () => {
    const el = mount("/p/alpha/board");
    const rows = Array.from(el.querySelectorAll<HTMLElement>('[role="treeitem"][data-key^="p:"]'));
    expect(rows.map((r) => r.dataset.key)).toEqual(["p:alpha", "p:my app"]);
    expect(rows.map((r) => r.querySelector("a.tree-project-link")?.getAttribute("href"))).toEqual(["/p/alpha", "/p/my%20app"]);
    for (const row of rows) {
      expect(row.hasAttribute("aria-expanded")).toBe(false);
      expect(row.querySelector(".tree-chevron")).toBeNull();
      expect(row.querySelector<HTMLAnchorElement>("a.tree-project-link")?.tabIndex).toBe(-1);
    }
    expect(el.querySelector('[data-key^="i:"]')).toBeNull();
    expect(el.querySelector('[data-key^="pa:"]')).toBeNull();
    expect(el.querySelector('[data-key^="pr:"]')).toBeNull();
    expect(el.querySelector(".tree-agent, .tree-repo")).toBeNull();
  });

  it("marks the project row on the project index and every deep project path", () => {
    for (const path of ["/p/alpha", "/p/alpha/schedules", "/p/alpha/knowledge/wiki/a/b", "/p/alpha/repos/infra", "/p/alpha/settings/secrets"]) {
      const el = mount(path);
      expect(projectRow(el, "alpha").getAttribute("aria-selected")).toBe("true");
      expect(projectRow(el, "alpha").querySelector(".tree-row")?.classList.contains("current")).toBe(true);
      expect(projectRow(el, "alpha").querySelector("a.tree-project-link")?.getAttribute("aria-current")).toBe("page");
      expect(projectRow(el, "my app").getAttribute("aria-selected")).toBe("false");
      expect(projectRow(el, "my app").querySelector("a.tree-project-link")?.hasAttribute("aria-current")).toBe(false);
      dispose?.();
      document.body.innerHTML = "";
    }
  });

  it("marks the project row on a page of one of its agents", () => {
    setAgentList([{ id: "a1", project: "alpha", name: "worker", status: "idle" }]);
    const el = mount("/agents/a1");
    expect(projectRow(el, "alpha").getAttribute("aria-selected")).toBe("true");
    expect(projectRow(el, "alpha").querySelector("a.tree-project-link")?.getAttribute("aria-current")).toBe("page");
    expect(projectRow(el, "my app").getAttribute("aria-selected")).toBe("false");
    expect(el.querySelector('[data-key^="pa:"]')).toBeNull();
  });

  it("has a cog on each project row that links the project settings screen", () => {
    const el = mount("/projects");
    const cogs = Array.from(el.querySelectorAll<HTMLAnchorElement>('a.tree-cog[aria-label^="Settings of "]'));
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

  it("has a health dot on each project row that follows the agents, with the counts in the label", () => {
    setAgentList([
      { id: "1", project: "alpha", name: "a", status: "idle" },
      { id: "2", project: "alpha", name: "b", status: "working" },
    ]);
    const el = mount("/projects");
    const dot = (p: string) => projectRow(el, p).querySelector<HTMLElement>(":scope > .tree-row > .project-dot")!;
    expect(dot("alpha").className).toBe("project-dot health-yellow");
    expect(dot("alpha").getAttribute("aria-hidden")).toBe("true");
    expect(projectRow(el, "alpha").getAttribute("aria-label")).toBe("alpha: 1 working, 1 idle");
    expect(projectRow(el, "alpha").querySelector(".tree-name")?.getAttribute("title")).toBe("alpha: 1 working, 1 idle");
    expect(dot("my app").className).toBe("project-dot health-none");
    expect(projectRow(el, "my app").getAttribute("aria-label")).toBe("my app: no agents");

    // A live change: one agent goes blocked, and the other ends its turn.
    setAgentList([
      { id: "1", project: "alpha", name: "a", status: "blocked" },
      { id: "2", project: "alpha", name: "b", status: "idle" },
      { id: "3", project: "my app", name: "c", status: "idle" },
    ]);
    expect(dot("alpha").className).toBe("project-dot health-red");
    expect(projectRow(el, "alpha").getAttribute("aria-label")).toBe("alpha: 1 blocked, 1 idle");
    expect(dot("my app").className).toBe("project-dot health-green");
    // The blocked count chip stays.
    expect(projectRow(el, "alpha").querySelector(".nav-count.warn")?.textContent).toBe("1");
  });
});

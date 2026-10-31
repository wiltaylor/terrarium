// @vitest-environment jsdom
// The project tree with two projects: each project row has a cog that links
// the project settings screen and is not a Tab stop of the tree.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("../stores", () => ({
  projects: {
    projects: () => [{ name: "alpha" }, { name: "my app" }],
    selected: () => null,
    select: () => {},
  },
  agents: { byId: () => undefined, inProject: () => [] },
}));

const { ProjectTree } = await import("./ProjectTree");

let dispose: (() => void) | undefined;
afterEach(() => {
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
    expect(keys).toEqual(["board", "repos", "logs", "knowledge", "agents"].map((p) => `i:alpha/${p}`));
  });

  it("keeps the Board row active on the Schedules tab of the board", () => {
    const el = mount("/p/alpha/board/schedules");
    const active = Array.from(el.querySelectorAll<HTMLElement>('[role="treeitem"][data-key^="i:"].active')).map((r) => r.dataset.key);
    expect(active).toEqual(["i:alpha/board"]);
  });
});

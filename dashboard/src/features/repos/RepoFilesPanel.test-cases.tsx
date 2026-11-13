import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";

const repos = vi.fn(async (project: string) => [{ id: project, project, name: "infra", url: "https://example.com/repo" }]);
const trees = vi.fn(async (id: string) => ({ repo_id: id, trees: [{ id: "main", kind: "main", label: "main" }] }));
const files = vi.fn(async (_id: string, _tree: string, path: string) => ({ path, entries: path ? [] : [{ path: "folder", name: "folder", kind: "dir" }], truncated: false }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { projects: { repos }, repos: { trees, files } } });
});

it("keeps expansion independent for duplicate panels on the same repo and tree", async () => {
  const history = createMemoryHistory(); history.set({ value: "/host" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><RepoFilesPanel project="alpha" /></section><section><RepoFilesPanel project="alpha" /></section>
  </>} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelectorAll('option[value="infra"]')).toHaveLength(2));
  for (const select of root.querySelectorAll<HTMLSelectElement>('[aria-label="Repository"]')) {
    select.value = "infra"; select.dispatchEvent(new Event("change", { bubbles: true }));
  }
  await waitFor(() => expect(root.querySelectorAll('[role="treeitem"][data-key="folder"]')).toHaveLength(2));
  const folders = root.querySelectorAll<HTMLElement>('[role="treeitem"][data-key="folder"]');
  folders[0]!.click();
  await waitFor(() => expect(folders[0]!.getAttribute("aria-expanded")).toBe("true"));
  expect(folders[1]!.getAttribute("aria-expanded")).toBe("false");
  folders[1]!.click();
  await waitFor(() => expect(folders[1]!.getAttribute("aria-expanded")).toBe("true"));
  folders[0]!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true }));
  await waitFor(() => expect(folders[0]!.getAttribute("aria-expanded")).toBe("false"));
  expect(folders[1]!.getAttribute("aria-expanded")).toBe("true");
});
const { RepoFilesPanel } = await import("./RepoFilesPanel");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.clearAllMocks(); });

it("requires an explicit local repo and associates each duplicate tree with its own controls", async () => {
  const history = createMemoryHistory(); history.set({ value: "/host?path=foreign&tree=foreign" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><RepoFilesPanel project="alpha" /></section><section><RepoFilesPanel project="beta" /></section>
  </>} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelectorAll('option[value="infra"]')).toHaveLength(2));
  expect(trees).not.toHaveBeenCalled();
  const selects = root.querySelectorAll<HTMLSelectElement>('[aria-label="Repository"]');
  selects[0]!.value = "infra"; selects[0]!.dispatchEvent(new Event("change", { bubbles: true }));
  await waitFor(() => expect(root.querySelectorAll(".repo-tree-pane")).toHaveLength(1));
  expect(selects[1]!.value).toBe("");
  selects[1]!.value = "infra"; selects[1]!.dispatchEvent(new Event("change", { bubbles: true }));
  await waitFor(() => expect(root.querySelectorAll(".repo-tree-pane")).toHaveLength(2));
  const ids = [...root.querySelectorAll(".repo-tree-pane")].map((el) => el.id);
  expect(new Set(ids).size).toBe(2);
  for (const section of root.querySelectorAll("section")) {
    const id = section.querySelector(".repo-tree-pane")!.id;
    expect(section.querySelector(".repo-tree-toggle")?.getAttribute("aria-controls")).toBe(id);
    expect(section.querySelector('[role="separator"]')?.getAttribute("aria-controls")).toBe(id);
  }
  expect(trees).toHaveBeenCalledWith("alpha", false);
  expect(trees).toHaveBeenCalledWith("beta", false);
  expect(history.get()).toBe("/host?path=foreign&tree=foreign");
});

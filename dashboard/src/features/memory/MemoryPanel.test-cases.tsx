// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { Memory } from "../../api/types";
import { waitFor } from "../../test/wait";

const memory = (project: string, id: string): Memory => ({
  id, key: `${project}/${id}`, project, title: `${project} ${id}`, body: "body", tags: [], author: "operator", sources: [], supersedes: [], links: [], created_at: "2026-11-13T00:00:00Z", updated_at: "2026-11-13T00:00:00Z",
});
const list = vi.fn(async (p: string) => [memory(p, "MEM-1"), memory(p, "MEM-2")]);
const graph = vi.fn(async (key: string) => ({ root: key, depth: 1, nodes: [memory(key.split("/")[0]!, "MEM-1"), memory(key.split("/")[0]!, "MEM-2")], edges: [] }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { memories: { list, graph } }, projects: { prefixes: () => ({}), projects: () => [] } });
});
const { MemoryPanel } = await import("./MemoryPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.clearAllMocks(); });

it("keeps memory selections local, qualifies project IDs and gives each graph its own marker", async () => {
  const [project, setProject] = createSignal("alpha");
  const history = createMemoryHistory(); history.set({ value: "/host?id=MEM-999" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><MemoryPanel project={project()} view="graph" /></section><section><MemoryPanel project="beta" view="graph" /></section>
  </>} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelectorAll("marker")).toHaveLength(2));
  const ids = [...root.querySelectorAll("marker")].map((m) => m.id);
  expect(new Set(ids).size).toBe(2);
  const nodes = root.querySelectorAll("section:first-child .node");
  nodes[1]!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  await waitFor(() => expect(graph).toHaveBeenCalledWith("alpha/MEM-2", 1));
  expect(root.querySelector("section:last-child .node.center")?.textContent).toContain("MEM-1");
  expect(history.get()).toBe("/host?id=MEM-999");
  setProject("gamma");
  await waitFor(() => expect(graph).toHaveBeenCalledWith("gamma/MEM-1", 1));
  expect(root.querySelector("section:first-child")!.textContent).not.toContain("alpha MEM");
});

it("renders the list part without a graph and ignores the host selection", async () => {
  const history = createMemoryHistory(); history.set({ value: "/host?id=MEM-999" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <MemoryPanel project="alpha" view="list" />} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelectorAll(".thread-list button")).toHaveLength(2));
  expect(root.querySelector("svg.graph")).toBeNull();
  expect(graph).toHaveBeenCalledWith("alpha/MEM-1", 1);
});

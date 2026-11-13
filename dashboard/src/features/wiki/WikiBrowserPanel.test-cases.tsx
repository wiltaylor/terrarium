import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createBookmarksStore } from "../../stores/bookmarks";
import { waitFor } from "../../test/wait";

const get = vi.fn(async (key: string) => ({ id: key, path: key.split("/").slice(1).join("/"), title: key, body: "Hello", rev: 1, updated_at: "2026-11-13T00:00:00Z", author_name: "operator", links: [] }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({
    bookmarks: createBookmarksStore(null, window.location.origin), projects: { prefixes: () => ({}) },
    api: { wiki: {
      tree: async () => [{ path: "index", title: "Index" }, { path: "guide", title: "Guide" }],
      get, backlinks: async () => [], revisions: async () => [],
    }, artifacts: { list: async () => [] } },
  });
});
const { WikiBrowserPanel } = await import("./WikiPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.clearAllMocks(); });

it("browses locally without replacing the host URL or another panel's path and draft", async () => {
  const [project, setProject] = createSignal("alpha");
  const history = createMemoryHistory(); history.set({ value: "/host" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><WikiBrowserPanel project={project()} /></section><section><WikiBrowserPanel project="beta" /></section>
  </>} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelectorAll("article")).toHaveLength(2));
  const sections = root.querySelectorAll("section");
  [...sections[1]!.querySelectorAll("button")].find((b) => b.textContent === "Edit")!.click();
  const draft = sections[1]!.querySelector<HTMLTextAreaElement>('[aria-label="Page body"]')!;
  draft.value = "unsaved"; draft.dispatchEvent(new Event("input", { bubbles: true }));
  sections[0]!.querySelector<HTMLAnchorElement>('a[href="/p/alpha/knowledge/wiki/guide"]')!.click();
  await waitFor(() => expect(get).toHaveBeenCalledWith("alpha/guide"));
  expect(get).not.toHaveBeenCalledWith("beta/guide");
  expect(draft.value).toBe("unsaved");
  expect(sections[1]!.querySelector('[aria-label="Page body"]')).toBe(draft);
  expect(history.get()).toBe("/host");
  setProject("gamma");
  await waitFor(() => expect(get).toHaveBeenCalledWith("gamma/index"));
  expect(draft.value).toBe("unsaved");
});

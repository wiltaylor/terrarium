// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createBookmarksStore } from "../../stores/bookmarks";

const bookmarks = createBookmarksStore(null, window.location.origin);
let title = "Start here";
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  bookmarks,
  projects: { prefixes: () => ({}) },
  api: {
    wiki: {
      tree: async () => [{ path: "guide/start here", title }],
      get: async () => ({ id: "page", title, path: "guide/start here", body: "Hello", rev: 1, updated_at: new Date().toISOString(), author_name: "operator", links: [] }),
      backlinks: async () => [],
      revisions: async () => [],
    },
    artifacts: { list: async () => [] },
  },
})); });
const { default: WikiPage, wikiHref } = await import("./WikiPage");
let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.replaceChildren();
  for (const b of bookmarks.items()) bookmarks.remove(b.href);
  title = "Start here";
});

function mount() {
  const root = document.createElement("div");
  document.body.append(root);
  const history = createMemoryHistory();
  history.set({ value: "/p/demo/knowledge/wiki/guide/start%20here" });
  dispose = render(() => createComponent(MemoryRouter, {
    history,
    get children() { return createComponent(Route, { path: "/p/:project/knowledge/wiki/*path", component: WikiPage }); },
  }), root);
  return root;
}

it("pins and unpins using the same encoded URL as the wiki tree", async () => {
  const root = mount();
  const button = () => [...root.querySelectorAll("button")].find((b) => /Pin to bookmarks|Unpin bookmark/.test(b.textContent ?? ""))!;
  await waitFor(() => expect(button()).toBeTruthy());
  expect(wikiHref("my project", "guide/start here")).toBe("/p/my%20project/knowledge/wiki/guide/start%20here");
  button().click();
  expect(button().textContent).toBe("Unpin bookmark");
  expect(button().getAttribute("aria-pressed")).toBe("true");
  const href = bookmarks.items()[0]!.href;
  expect(href).toBe("/p/demo/knowledge/wiki/guide/start%20here");
  expect(root.querySelector(`a[href="${href}"]`)).not.toBeNull();
  button().click();
  expect(button().textContent).toBe("Pin to bookmarks");
  expect(bookmarks.items()).toEqual([]);
});

it("explains why a blank-title page cannot be pinned", async () => {
  title = " ";
  const root = mount();
  const button = () => [...root.querySelectorAll("button")].find((b) => b.textContent === "Pin to bookmarks")!;
  await waitFor(() => expect(button()).toBeTruthy());
  button().click();
  expect(root.textContent).toContain("This page needs a title before it can be pinned.");
  expect(bookmarks.items()).toEqual([]);
});

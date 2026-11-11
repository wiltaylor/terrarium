// @vitest-environment jsdom
import { MemoryRouter, Route } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  const { createBookmarksStore } = await import("../stores/bookmarks");
  return instanceTestStores({ bookmarks: createBookmarksStore(null, window.location.origin) });
});
import { bookmarks } from "../stores";
import { expanded } from "./TreeRoot";
import { Bookmarks } from "./Bookmarks";
import ManageBookmarks from "./ManageBookmarks";
import { waitFor } from "../test/wait";

let dispose: (() => void) | undefined;
beforeEach(() => expanded.open("/bookmarks"));
afterEach(() => {
  dispose?.();
  for (const b of bookmarks.items()) bookmarks.remove(b.href);
  document.body.replaceChildren();
});

it("opens wiki pages in place and external URLs in a new window", () => {
  bookmarks.add("Wiki", "/p/demo/knowledge/wiki/start");
  bookmarks.add("External", "https://example.com/");
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(() => createComponent(MemoryRouter, {
    get children() { return createComponent(Route, { path: "*", component: Bookmarks }); },
  }), root);
  const wiki = root.querySelector<HTMLAnchorElement>('a[href="/p/demo/knowledge/wiki/start"]')!;
  const external = root.querySelector<HTMLAnchorElement>('a[href="https://example.com/"]')!;
  expect(wiki.getAttribute("target")).toBeNull();
  expect(external.target).toBe("_blank");
  expect(external.rel).toBe("noopener noreferrer");
  expect(root.querySelector("input")).toBeNull();
  expect(root.querySelector('button[aria-label^="Remove bookmark"]')).toBeNull();
});

it("opens management with the cog and adds, edits and removes with sidebar updates", async () => {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(() => createComponent(MemoryRouter, {
    root: (props) => [createComponent(Bookmarks, {}), props.children],
    get children() { return [createComponent(Route, { path: "/", component: () => null }), createComponent(Route, { path: "/bookmarks", component: ManageBookmarks })]; },
  }), root);
  root.querySelector<HTMLAnchorElement>('a[aria-label="Manage bookmarks"]')!.click();
  await waitFor(() => expect(root.querySelector("h1")?.textContent).toBe("Manage bookmarks"));
  expect(root.querySelector("input")).toBeNull();
  const click = (text: string) => Array.from(document.querySelectorAll("button")).find((b) => b.textContent === text)!.click();
  click("Add bookmark");
  const input = (label: string, value: string) => {
    const el = document.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`)!;
    el.value = value;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  };
  input("Bookmark title", "Example");
  input("Bookmark URL", "https://example.com/");
  const save = () => document.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  save();
  expect(bookmarks.items()).toEqual([{ title: "Example", href: "https://example.com/" }]);
  expect(root.querySelector('a[href="https://example.com/"]')?.textContent).toBe("Example");
  click("Edit");
  expect(document.querySelector<HTMLInputElement>('input[aria-label="Bookmark title"]')!.value).toBe("Example");
  input("Bookmark title", "Changed");
  input("Bookmark URL", "javascript:alert(1)");
  save();
  expect(document.querySelector('[role="alert"]')).not.toBeNull();
  expect(bookmarks.items()[0]?.title).toBe("Example");
  input("Bookmark URL", "/p/demo/knowledge/wiki/start");
  save();
  expect(root.querySelector('a[href="https://example.com/"]')).toBeNull();
  expect(root.querySelector('a[href="/p/demo/knowledge/wiki/start"]')?.textContent).toBe("Changed");
  click("Edit");
  input("Bookmark title", "Discarded");
  click("Cancel");
  expect(bookmarks.items()[0]?.title).toBe("Changed");
  expect(document.querySelector('[role="dialog"]')).toBeNull();
  click("Remove");
  expect(bookmarks.items()).toEqual([]);
  expect(root.querySelector('a[href="/p/demo/knowledge/wiki/start"]')).toBeNull();
});

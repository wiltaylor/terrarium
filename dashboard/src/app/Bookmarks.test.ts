// @vitest-environment jsdom
import { MemoryRouter, Route } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
vi.mock("../stores", async () => {
  const { createBookmarksStore } = await import("../stores/bookmarks");
  return { bookmarks: createBookmarksStore(null, window.location.origin) };
});
import { bookmarks } from "../stores";
import { expanded } from "./TreeRoot";
import { Bookmarks } from "./Bookmarks";

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
  root.querySelector<HTMLButtonElement>('button[aria-label="Remove bookmark Wiki"]')!.click();
  expect(bookmarks.has("/p/demo/knowledge/wiki/start")).toBe(false);
});

it("adds a URL through the form", () => {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(() => createComponent(MemoryRouter, {
    get children() { return createComponent(Route, { path: "*", component: Bookmarks }); },
  }), root);
  const input = (label: string, value: string) => {
    const el = root.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`)!;
    el.value = value;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  };
  input("Bookmark title", "Example");
  input("Bookmark URL", "https://example.com/");
  root.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  expect(bookmarks.items()).toEqual([{ title: "Example", href: "https://example.com/" }]);
});

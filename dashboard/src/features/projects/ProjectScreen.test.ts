// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { PROJECT_TABS } from "../../app/nav";
import ProjectScreen from "./ProjectScreen";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(Element.prototype, "scrollIntoView");
});

function mount(path: string) {
  const root = document.createElement("div");
  document.body.append(root);
  const history = createMemoryHistory();
  history.set({ value: path });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "/p/:project/*", component: ProjectScreen });
        },
      }),
    root,
  );
  return { root, history };
}

function links(root: HTMLElement) {
  return Array.from(root.querySelectorAll<HTMLAnchorElement>('nav[aria-label="Project"] a'));
}

function active(root: HTMLElement) {
  return links(root).filter((a) => a.classList.contains("active"));
}

describe("ProjectScreen", () => {
  it("shows the project title and every tab in order, with Settings last", () => {
    const { root } = mount("/p/my%20app/board");
    expect(root.querySelector("h1")?.textContent).toBe("my%20app");
    expect(links(root).map((a) => a.textContent)).toEqual(PROJECT_TABS.map((t) => t.label));
    expect(links(root).map((a) => a.getAttribute("href"))).toEqual(PROJECT_TABS.map((t) => `/p/my%20app/${t.path}`));
    expect(links(root).every((a) => a.tabIndex === 0)).toBe(true);
  });

  it("selects the tab from the first segment on deep paths", () => {
    for (const [path, label] of [
      ["/p/demo/board/tests/1", "Board"],
      ["/p/demo/knowledge/wiki/a/b", "Knowledge"],
      ["/p/demo/repos/infra", "Repos"],
      ["/p/demo/settings/secrets", "Settings"],
    ]) {
      const { root } = mount(path);
      expect(active(root).map((a) => a.textContent), path).toEqual([label]);
      dispose?.();
      document.body.replaceChildren();
    }
  });

  it("moves between tabs with browser back and forward", async () => {
    const { root, history } = mount("/p/demo/board/tests/1");
    links(root).find((a) => a.textContent === "Dashboard")!.click();
    await vi.waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Dashboard"]));
    links(root).find((a) => a.textContent === "Knowledge")!.click();
    await vi.waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Knowledge"]));
    history.back();
    await vi.waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Dashboard"]));
    history.forward();
    await vi.waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Knowledge"]));
  });

  it("scrolls the active tab into view on a phone, including after navigation", async () => {
    const scrolled: Element[] = [];
    const scrollIntoView = vi.fn(function (this: Element) {
      scrolled.push(this);
    });
    vi.stubGlobal("matchMedia", vi.fn(() => ({ matches: true })));
    Object.defineProperty(Element.prototype, "scrollIntoView", { configurable: true, value: scrollIntoView });
    const { root } = mount("/p/demo/settings/secrets");
    expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest", inline: "nearest" });
    expect(scrolled.at(-1)?.textContent).toBe("Settings");
    links(root).find((a) => a.textContent === "Board")!.click();
    await vi.waitFor(() => expect(scrolled.at(-1)?.textContent).toBe("Board"));
  });
});

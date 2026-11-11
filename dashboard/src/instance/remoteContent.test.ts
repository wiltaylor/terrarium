// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, type Component } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { InstanceProvider, createInstance, type Instance } from ".";
import { createAuthStore } from "../stores/auth";
import { Markdown } from "../components/Markdown";
import { SafeHtml } from "../components/SafeHtml";
import { waitFor } from "../test/wait";

vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  return instanceTestStores({});
});

const page = "/remote/lab/p/demo/knowledge/wiki/start";
let dispose: (() => void) | undefined;
let instance: Instance | undefined;
afterEach(() => { dispose?.(); instance?.dispose(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); window.history.replaceState({}, "", "/"); });

function mount(component: Component<{ html: string }>, html: string) {
  window.history.replaceState({}, "", page);
  instance = createInstance({ name: "lab", auth: createAuthStore() });
  const root = document.createElement("div"); document.body.append(root);
  const history = createMemoryHistory(); history.set({ value: page });
  dispose = render(() => createComponent(InstanceProvider, { instance: instance!, get children() {
    return createComponent(MemoryRouter, { history, get children() { return createComponent(Route, { path: "*", component: () => createComponent(component, { html }) }); } });
  } }), root);
  return { root, history };
}

describe.each([["Markdown", Markdown], ["SafeHtml", SafeHtml]] as const)("remote %s content", (_name, component) => {
  it.each([
    ["../../../../../../api/remotes", null],
    ["../../../../../../p/demo/board?card=C1", "/remote/lab/p/demo/board?card=C1"],
    ["/api/remotes", null],
    [`${window.location.origin}/api/remotes`, null],
    [`${window.location.origin}/p/demo/board`, "/remote/lab/p/demo/board"],
    ["/%61pi/remotes", null],
    ["/remote/lab/%2e%2e/%2e%2e/api/remotes", null],
    ["/p/demo%2f..%2f..%2fapi/board", null],
    ["/p/%252e%252e/board", null],
    ["/not-a-dashboard-route", null],
    ["javascript:alert(1)", null],
    ["data:text/html,hello", null],
    ["mailto:operator@example.com", null],
    ["?card=C1#reply", `${page}?card=C1#reply`],
    ["/p/my%20app/board", "/remote/lab/p/my%20app/board"],
    ["#reply", `${page}#reply`],
  ])("normalizes %s before plain or modified clicks and new-tab navigation", async (href, expected) => {
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    const { root, history } = mount(component, `<a href="${href}">go</a>`);
    const anchor = root.querySelector("a")!;
    expect(anchor.getAttribute("href")).toBe(expected);
    if (!expected) expect(anchor.title).toContain("Blocked");
    else expect(anchor.href).toBe(new URL(expected, window.location.origin).href);
    // Browser modified-click/new-tab targets read the actual href, not a click rewrite.
    for (const options of [{ ctrlKey: true }, { metaKey: true }, { shiftKey: true }, { button: 1 }]) {
      const event = new MouseEvent("click", { bubbles: true, cancelable: true, ...options });
      const stopNativeNavigation = (e: Event) => { expect(e.defaultPrevented).toBe(false); e.preventDefault(); };
      document.addEventListener("click", stopNativeNavigation, { once: true });
      anchor.dispatchEvent(event);
      expect(anchor.getAttribute("href")).toBe(expected);
      expect(history.get()).toBe(page);
    }
    document.addEventListener("click", (e) => e.preventDefault(), { once: true });
    anchor.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    if (component === Markdown && expected) await waitFor(() => expect(history.get()).toBe(expected));
    expect(fetch).not.toHaveBeenCalled();
  });

  it("rebases absolute home pages, blocks home APIs, and marks other HTTP origins external", () => {
    const origin = window.location.origin;
    const { root } = mount(component, `<a href="${origin}/p/demo/board">page</a><a href="${origin}/api/remotes">api</a><a href="https://remote.example/page">external</a><a href="${origin}/favicon.svg">non-page</a>`);
    const links = root.querySelectorAll("a");
    expect(links[0]!.getAttribute("href")).toBe("/remote/lab/p/demo/board");
    expect(links[1]!.hasAttribute("href")).toBe(false);
    expect(links[2]!.href).toBe("https://remote.example/page");
    expect(links[2]!.target).toBe("_blank");
    expect(links[2]!.rel).toBe("noopener noreferrer");
    expect(links[3]!.hasAttribute("href")).toBe(false);
  });

  it("removes image sources before insertion, retaining only inert alt placeholders", () => {
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    const replace = Element.prototype.replaceChildren;
    const inserted: DocumentFragment[] = [];
    vi.spyOn(Element.prototype, "replaceChildren").mockImplementation(function (this: Element, ...nodes: (Node | string)[]) {
      for (const node of nodes) if (node instanceof DocumentFragment) {
        expect(node.querySelector("img, [src], [srcset], [style]")).toBeNull();
        inserted.push(node.cloneNode(true) as DocumentFragment);
      }
      return replace.apply(this, nodes);
    });
    const { root } = mount(component, '<img src="https://remote.example/tracker" alt="remote"><img src="/settings" alt="home"><img src="../../../../settings" alt="relative"><img src="data:image/png;base64,AAAA" alt="data"><img srcset="https://remote.example/large 2x" style="background:url(/api/remotes)" alt="css">');
    expect(inserted.length).toBeGreaterThan(0);
    expect(root.querySelector("img, [src], [srcset], [style]")).toBeNull();
    for (const alt of ["remote", "home", "relative", "data", "css"]) expect(root.textContent).toContain(`[Image blocked: ${alt}]`);
    expect(fetch).not.toHaveBeenCalled();
  });

  it("inserts only home image-proxy URLs for validated artifact images", () => {
    const { root } = mount(component, '<img src="/api/images/one?size=small" alt="api"><img src="/a/01ARZ3NDEKTSV4RRFFQ69G5FAV/photo.png?sig=remote" alt="artifact"><img src="/api/%252e%252e/secret" alt="encoded">');
    const images = root.querySelectorAll("img");
    expect([...images].map((img) => img.getAttribute("src"))).toEqual([
      "/api/remotes/lab/proxy/image/a/01ARZ3NDEKTSV4RRFFQ69G5FAV/photo.png?sig=remote",
    ]);
    expect(root.textContent).toContain("[Image blocked: encoded]");
  });
});

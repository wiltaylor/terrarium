// @vitest-environment jsdom
// The links in plain text (TER-571, TER-598): an external link opens a new tab
// with no opener, a plain click on a card key opens the side panel, and a
// click with a modifier key keeps the browser's action. `openCardLink` does
// the same for rendered Markdown.
import { createComponent, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createCardPeekStore } from "../stores/cardPeek";

const cardPeek = createCardPeekStore();
vi.mock("../stores", async () => { const { instanceTestStores } = await import("../test/instance"); return instanceTestStores(({
  cardPeek,
  projects: { projects: () => [{ name: "terrarium", card_prefix: "TER" }, { name: "webshop", card_prefix: "WEB" }, { name: "plain", card_prefix: "" }] },
})); });

const { LinkText, openCardLink, readerCardKeys } = await import("./LinkText");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  cardPeek.close();
});

function mount(view: () => JSX.Element) {
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  history.set({ value: "/p/terrarium/inbox" });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "*", component: view });
        },
      }),
    el,
  );
  return el;
}

const text = (t: string) => mount(() => createComponent(LinkText, { text: t, project: "terrarium" }));

/** A click that the test can inspect: `defaultPrevented` says whether the link kept its action. */
const click = (a: Element, init: MouseEventInit = {}) => {
  const e = new MouseEvent("click", { bubbles: true, cancelable: true, button: 0, ...init });
  a.dispatchEvent(e);
  return e;
};

describe("LinkText", () => {
  it("opens an external link in a new tab with no opener", () => {
    const el = text("See https://example.com/x and [docs](https://docs.example.com/).");
    const links = [...el.querySelectorAll("a")];
    expect(links.map((a) => a.getAttribute("href"))).toEqual(["https://example.com/x", "https://docs.example.com/"]);
    for (const a of links) {
      expect(a.getAttribute("target")).toBe("_blank");
      expect(a.getAttribute("rel")).toBe("noopener noreferrer");
    }
    expect(el.textContent).toBe("See https://example.com/x and docs.");
  });

  it("renders a refused link and HTML as text", () => {
    const el = text("[x](javascript:alert(1)) <img src=x onerror=alert(1)> [[project:../x]]");
    expect(el.querySelector("a")).toBeNull();
    expect(el.querySelector("img")).toBeNull();
    expect(el.textContent).toContain("<img src=x onerror=alert(1)>");
  });

  it("opens a card key in the side panel on a plain click", () => {
    const el = text("Blocked by WEB-4.");
    const a = el.querySelector("a.card-key")!;
    expect(a.getAttribute("href")).toBe("/p/webshop/board?card=WEB-4");
    expect(a.hasAttribute("target")).toBe(false);
    const e = click(a);
    expect(e.defaultPrevented).toBe(true);
    expect(cardPeek.current()).toEqual({ project: "webshop", id: "WEB-4" });
  });

  it("keeps the browser's action for a click with a modifier key or another button", () => {
    const el = text("TER-7");
    const a = el.querySelector("a.card-key")!;
    for (const init of [{ ctrlKey: true }, { metaKey: true }, { shiftKey: true }, { altKey: true }, { button: 1 }]) {
      expect(click(a, init).defaultPrevented, JSON.stringify(init)).toBe(false);
    }
    expect(cardPeek.current()).toBeNull();
  });

  it("links a wiki page in the app", () => {
    const el = text("[the design](wiki:designs/x)");
    const a = el.querySelector("a.wikilink")!;
    expect(a.getAttribute("href")).toBe("/p/terrarium/knowledge/wiki/designs/x");
    expect(a.hasAttribute("target")).toBe(false);
  });

  it("makes card links only for the prefixes of the reader", () => {
    expect(readerCardKeys()).toEqual({ TER: "terrarium", WEB: "webshop" });
    expect(text("ABC-1").querySelector("a")).toBeNull();
  });
});

describe("openCardLink", () => {
  const prose = (html: string) => {
    const el = document.createElement("div");
    el.innerHTML = html;
    el.addEventListener("click", openCardLink);
    document.body.append(el);
    return el;
  };

  it("opens a wikilink-card link in the side panel on a plain click", () => {
    const el = prose('<a class="wikilink wikilink-card" href="/p/terrarium/board?card=TER-12"><code>TER-12</code></a>');
    const e = click(el.querySelector("code")!);
    expect(e.defaultPrevented).toBe(true);
    expect(cardPeek.current()).toEqual({ project: "terrarium", id: "TER-12" });
  });

  it("leaves a modifier click, other links, and a bad href alone", () => {
    const el = prose(
      '<a id="card" class="wikilink wikilink-card" href="/p/terrarium/board?card=TER-12">TER-12</a>' +
        '<a id="page" class="wikilink wikilink-page" href="/p/terrarium/knowledge/wiki/x">x</a>' +
        '<a id="bad" class="wikilink wikilink-card" href="https://example.com/">y</a>',
    );
    expect(click(el.querySelector("#card")!, { ctrlKey: true }).defaultPrevented).toBe(false);
    expect(click(el.querySelector("#card")!, { button: 1 }).defaultPrevented).toBe(false);
    expect(click(el.querySelector("#page")!).defaultPrevented).toBe(false);
    expect(click(el.querySelector("#bad")!).defaultPrevented).toBe(false);
    expect(cardPeek.current()).toBeNull();
  });
});

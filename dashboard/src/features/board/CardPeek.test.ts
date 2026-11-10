// @vitest-environment jsdom
// The card side panel (TER-571, TER-598): it loads the card that a key opened,
// shows its body and comments with links, the close button and the link to
// the board close it, and Escape closes only the top panel when the panel is
// open over the board drawer.
import { createComponent, createSignal, type JSX, Show } from "solid-js";
import { render } from "solid-js/web";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Card } from "../../api/types";
import { createCardPeekStore } from "../../stores/cardPeek";

const cardPeek = createCardPeekStore();
const get = vi.fn();
vi.mock("../../stores", () => ({
  api: { cards: { get } },
  cardPeek,
  projects: { projects: () => [{ name: "terrarium", card_prefix: "TER" }] },
}));

const { CardPeek } = await import("./CardPeek");
const { Drawer } = await import("../../components/ui");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  cardPeek.close();
  vi.clearAllMocks();
});

const card = (id: string, extra: Partial<Card> = {}): Card =>
  ({
    id,
    project: "terrarium",
    column: "review",
    title: `Card ${id}`,
    body: "Depends on TER-2, see https://example.com/x.",
    comments: [{ author: "01A", author_name: "terrarium/lead", body: "Moved from TER-3.", created_at: new Date().toISOString() }],
    ...extra,
  }) as unknown as Card;

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

const peek = () => document.querySelector<HTMLElement>(".card-peek");
const escape = () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

describe("CardPeek", () => {
  it("renders nothing until a card opens, then the card with links", async () => {
    get.mockResolvedValue(card("TER-1"));
    mount(() => createComponent(CardPeek, {}));
    expect(document.querySelector(".drawer")).toBeNull();
    cardPeek.open({ project: "terrarium", id: "TER-1" });
    await vi.waitFor(() => expect(peek()).not.toBeNull());
    expect(get).toHaveBeenCalledWith("TER-1");
    expect(peek()!.querySelector("h2")!.textContent).toBe("Card TER-1");
    expect(document.querySelector(".drawer .chip")!.textContent).toBe("review");
    const keys = [...peek()!.querySelectorAll("a.card-key")].map((a) => a.getAttribute("href"));
    expect(keys).toEqual(["/p/terrarium/board?card=TER-2", "/p/terrarium/board?card=TER-3"]);
    expect(peek()!.querySelector("a.external")!.getAttribute("rel")).toBe("noopener noreferrer");
    expect(peek()!.textContent).toContain("terrarium/lead");
  });

  it("replaces the card when a key in it opens another one", async () => {
    get.mockImplementation((id: string) => Promise.resolve(card(id)));
    mount(() => createComponent(CardPeek, {}));
    cardPeek.open({ project: "terrarium", id: "TER-1" });
    await vi.waitFor(() => expect(peek()).not.toBeNull());
    peek()!.querySelector<HTMLAnchorElement>("a.card-key")!.click();
    await vi.waitFor(() => expect(peek()!.querySelector("h2")!.textContent).toBe("Card TER-2"));
    expect(document.querySelectorAll(".drawer")).toHaveLength(1);
  });

  it("closes with the close button and with the link to the board", async () => {
    get.mockResolvedValue(card("TER-1", { body: "", comments: [] }));
    mount(() => createComponent(CardPeek, {}));
    cardPeek.open({ project: "terrarium", id: "TER-1" });
    await vi.waitFor(() => expect(peek()).not.toBeNull());
    expect(peek()!.textContent).toContain("No description.");
    expect(peek()!.textContent).toContain("No comments.");
    document.querySelector<HTMLButtonElement>('.drawer button[aria-label="Close"]')!.click();
    expect(cardPeek.current()).toBeNull();
    expect(document.querySelector(".drawer")).toBeNull();

    cardPeek.open({ project: "terrarium", id: "TER-1" });
    await vi.waitFor(() => expect(peek()).not.toBeNull());
    const board = [...peek()!.querySelectorAll("a")].find((a) => a.textContent === "Open on the board")!;
    expect(board.getAttribute("href")).toBe("/p/terrarium/board?card=TER-1");
    board.click();
    expect(cardPeek.current()).toBeNull();
  });

  it("shows a load error", async () => {
    get.mockRejectedValue(new Error("no such card"));
    mount(() => createComponent(CardPeek, {}));
    cardPeek.open({ project: "terrarium", id: "TER-404" });
    await vi.waitFor(() => expect(document.querySelector(".drawer")!.textContent).toContain("no such card"));
    expect(document.querySelector(".drawer")!.textContent).not.toContain("Loading the card.");
    expect(peek()).toBeNull();
  });

  it("closes only the top panel on Escape when it is open over the board drawer", async () => {
    get.mockResolvedValue(card("TER-1"));
    const [boardOpen, setBoardOpen] = createSignal(true);
    const closeBoard = vi.fn(() => setBoardOpen(false));
    mount(() => [
      createComponent(Show, {
        keyed: false,
        get when() {
          return boardOpen();
        },
        get children() {
          return createComponent(Drawer, { title: "Board card", onClose: closeBoard, children: "the board drawer" });
        },
      }),
      createComponent(CardPeek, {}),
    ]);
    cardPeek.open({ project: "terrarium", id: "TER-1" });
    await vi.waitFor(() => expect(peek()).not.toBeNull());
    expect(document.querySelectorAll(".drawer")).toHaveLength(2);

    escape();
    expect(cardPeek.current()).toBeNull();
    expect(closeBoard).not.toHaveBeenCalled();
    expect(document.querySelectorAll(".drawer")).toHaveLength(1);

    escape();
    expect(closeBoard).toHaveBeenCalledTimes(1);
    expect(document.querySelector(".drawer")).toBeNull();
  });
});

// @vitest-environment jsdom
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { expect, it } from "vitest";
import type { Board, BoardGroup, Card } from "../../api/types";
import { LanesBoard } from "./LanesBoard";

it("moves the full progress block with members instead of leaving it at home", () => {
  const group: BoardGroup = { id: "P-3", key: "batch", title: "Batch", column: "review", status: "Running", done: 1, total: 2, members: ["P-1", "P-2"], eta_at: "2026-11-09T12:00:00Z", closed_at: null, steps: [{ name: "Build", state: "running", text: "Compiling", eta_at: null }] };
  const card = (id: string, column: string, kind: "card" | "group" = "card"): Card => ({
    id, column, kind, group, position: 0, labels: [], project: "demo", title: id, body: "",
    depends_on: [], blocked_by: [], comments: [], links: [], created_by: "operator",
    created_at: "2026-11-08T12:00:00Z", updated_at: "2026-11-08T12:00:00Z", archived: false,
  });
  const [cards, setCards] = createSignal([card("P-3", "review", "group"), card("P-1", "review"), card("P-2", "done")]);
  const columns = ["ready", "review", "done"].map((name) => ({ name, title: name }));
  const el = document.createElement("div");
  document.body.append(el);
  const dispose = render(() => createComponent(LanesBoard, {
    get board() { return { columns, cards: cards() } as Board; },
    get lanes() { return [{ key: "all", cards: cards() }]; },
    phone: false, collapsed: [], onToggle: () => {}, visible: () => true,
    addButton: () => null, card: () => null, onDrop: () => {},
  }), el);
  try {
    const cell = (column: string) => el.querySelector(`[data-lane-cell="all:${column}"]`)!;
    expect(cell("review").querySelector("progress")).not.toBeNull();
    setCards(cards().map((c) => c.id === "P-1" ? { ...c, column: "ready" } : c));
    expect(cell("review").querySelector(".board-group-head")).toBeNull();
    expect(el.querySelectorAll("progress")).toHaveLength(1);
    const header = cell("ready").querySelector(".board-group-head")!;
    expect(header.querySelector("progress")?.getAttribute("value")).toBe("1");
    expect(header.querySelector("time")?.getAttribute("datetime")).toBe(group.eta_at);
    expect(header.textContent).toContain("1/2");
    expect(header.textContent).toContain("Running");
    expect(header.textContent).toContain("Build: running Compiling");
    expect(cell("done").querySelector(".board-group-head")?.textContent).toBe("Batch");
  } finally {
    dispose();
    el.remove();
  }
});

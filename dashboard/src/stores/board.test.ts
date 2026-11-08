import { describe, expect, it, vi } from "vitest";
import { createRoot } from "solid-js";
import { createMockApi } from "../api/mock";
import type { Board, Card } from "../api/types";
import { describeError, errorText } from "../lib/errors";
import { cardsIn, createBoardStore, moveCard } from "./board";

const card = (id: string, column: string, position: number): Card => ({
  id,
  project: "p",
  column,
  position,
  title: id,
  body: "",
  labels: [],
  depends_on: [],
  blocked_by: [],
  archived: false,
  comments: [],
  links: [],
  created_by: "x",
  created_at: "",
  updated_at: "",
});

const board: Board = {
  project: "p",
  columns: [{ name: "a", position: 0 }, { name: "b", position: 1 }],
  cards: [card("A1", "a", 0), card("A2", "a", 1), card("A3", "a", 2), card("B1", "b", 0)],
};

describe("moveCard", () => {
  it("moves across columns and renumbers both", () => {
    const next = moveCard(board, "A1", "b", 0);
    expect(cardsIn(next, "a").map((c) => [c.id, c.position])).toEqual([["A2", 0], ["A3", 1]]);
    expect(cardsIn(next, "b").map((c) => [c.id, c.position])).toEqual([["A1", 0], ["B1", 1]]);
  });

  it("reorders inside one column", () => {
    const next = moveCard(board, "A1", "a", 2);
    expect(cardsIn(next, "a").map((c) => c.id)).toEqual(["A2", "A3", "A1"]);
  });

  it("clamps the index and ignores unknown cards", () => {
    expect(cardsIn(moveCard(board, "B1", "a", 99), "a").map((c) => c.id)).toEqual(["A1", "A2", "A3", "B1"]);
    expect(moveCard(board, "nope", "a", 0)).toBe(board);
  });

  it("does not mutate its input", () => {
    moveCard(board, "A1", "b", 0);
    expect(board.cards[0]!.column).toBe("a");
  });
});

describe("createBoardStore", () => {
  it("loads Done only on request and appends bounded pages", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0 });
    const get = vi.spyOn(api.board, "get");
    get.mockResolvedValueOnce({ ...board, cards: [card("D1", "done", 0)], next_cursor: "1" });
    get.mockResolvedValueOnce({ ...board, cards: [card("D2", "done", 1)], next_cursor: null });
    const s = createBoardStore(api);
    await s.load("p", true);
    expect(get).toHaveBeenNthCalledWith(1, "p", { done: true });
    await s.more();
    expect(get).toHaveBeenNthCalledWith(2, "p", { done: true, cursor: "1" });
    expect(s.state.board!.cards.map(c => c.id)).toEqual(["D1", "D2"]);
    expect(s.state.board!.next_cursor).toBeNull();
  });

  it("does not append an old project's Done page after navigation", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0 });
    let finish!: (b: Board) => void;
    const get = vi.spyOn(api.board, "get");
    get.mockResolvedValueOnce({ ...board, next_cursor: "1" });
    get.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    get.mockResolvedValueOnce({ ...board, project: "other", cards: [] });
    const s = createBoardStore(api);
    await s.load("p", true);
    const pending = s.more();
    await s.load("other");
    finish({ ...board, cards: [card("D1", "done", 0)] });
    await pending;
    expect(s.state.board!.project).toBe("other");
    expect(s.state.board!.cards).toEqual([]);
  });

  const withStore = async (fn: (s: ReturnType<typeof createBoardStore>) => Promise<void>) => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0 });
    let dispose = () => {};
    const store = createRoot((d) => {
      dispose = d;
      return createBoardStore(api);
    });
    try {
      await fn(store);
    } finally {
      dispose();
    }
  };

  it("loads a board and applies an accepted move", async () => {
    await withStore(async (s) => {
      await s.load("terrarium");
      expect(s.state.board?.cards.length).toBeGreaterThan(0);
      const ok = await s.move("TER-17", "ready", 0);
      expect(ok).toBe(true);
      expect(s.state.board!.cards.find((c) => c.id === "TER-17")!.column).toBe("ready");
      expect(s.state.error).toBeNull();
    });
  });

  it("rolls back a refused move and keeps the daemon's message", async () => {
    await withStore(async (s) => {
      await s.load("terrarium");
      const ok = await s.move("TER-14", "in_progress", 0);
      expect(ok).toBe(false);
      expect(s.state.board!.cards.find((c) => c.id === "TER-14")!.column).toBe("ready");
      expect(errorText(s.state.error)).toMatch(/depends on/);
      expect(describeError(s.state.error)!.details).toEqual([{ label: "Blocked by", value: "TER-12", pre: false }]);
    });
  });
});

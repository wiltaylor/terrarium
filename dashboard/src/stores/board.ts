// Kanban board for one project. Moves are optimistic and roll back on refusal.

import { createStore, reconcile, unwrap } from "solid-js/store";
import type { Api } from "../api/client";
import type { Board, Card } from "../api/types";

/**
 * Move a card to `toColumn` at `toIndex` (index among that column's cards,
 * after removing the card). Pure. Positions are renumbered 0..n per column.
 */
export function moveCard(board: Board, id: string, toColumn: string, toIndex: number): Board {
  const card = board.cards.find((c) => c.id === id);
  if (!card) return board;
  const fromColumn = card.column;
  const byCol = (col: string) =>
    board.cards.filter((c) => c.column === col && c.id !== id).sort((a, b) => a.position - b.position);
  const target = byCol(toColumn);
  const index = Math.max(0, Math.min(toIndex, target.length));
  target.splice(index, 0, { ...card, column: toColumn });
  const renumbered = new Map<string, Card>();
  target.forEach((c, i) => renumbered.set(c.id, { ...c, position: i }));
  if (fromColumn !== toColumn) byCol(fromColumn).forEach((c, i) => renumbered.set(c.id, { ...c, position: i }));
  return { ...board, cards: board.cards.map((c) => renumbered.get(c.id) ?? c) };
}

export function cardsIn(board: Board, column: string): Card[] {
  return board.cards.filter((c) => c.column === column).sort((a, b) => a.position - b.position);
}

export function createBoardStore(api: Pick<Api, "board" | "cards">) {
  const [state, setState] = createStore<{ board: Board | null; loading: boolean; error: string | null }>({
    board: null,
    loading: false,
    error: null,
  });
  let project: string | null = null;

  const setBoard = (b: Board) => setState("board", reconcile(b, { key: "id", merge: false }));

  const load = async (name: string) => {
    project = name;
    setState({ loading: true, error: null });
    try {
      const b = await api.board.get(name);
      if (project === name) setBoard(b);
    } catch (e) {
      setState("error", (e as Error).message);
    } finally {
      setState("loading", false);
    }
  };

  return {
    state,
    load,
    reload: () => (project ? load(project) : Promise.resolve()),
    clearError: () => setState("error", null),

    /** Optimistic move. Resolves true when the daemon accepts it. */
    async move(id: string, toColumn: string, toIndex: number, comment?: string): Promise<boolean> {
      const before = state.board;
      if (!before) return false;
      const snapshot = structuredClone(unwrap(before)) as Board;
      setBoard(moveCard(snapshot, id, toColumn, toIndex));
      try {
        const saved = await api.cards.update(id, { column: toColumn, position: toIndex, comment });
        const b = state.board;
        if (b) setBoard({ ...b, cards: b.cards.map((c) => (c.id === id ? saved : c)) });
        setState("error", null);
        return true;
      } catch (e) {
        setBoard(snapshot);
        setState("error", (e as Error).message);
        return false;
      }
    },

    async create(title: string, column = "backlog"): Promise<Card | null> {
      if (!project) return null;
      try {
        const card = await api.cards.create(project, { title, column });
        const b = state.board;
        if (b) setBoard({ ...b, cards: [...b.cards, card] });
        return card;
      } catch (e) {
        setState("error", (e as Error).message);
        return null;
      }
    },

    /** Replace one card after an edit or a comment. */
    put(card: Card) {
      const b = state.board;
      if (b) setBoard({ ...b, cards: b.cards.map((c) => (c.id === card.id ? card : c)) });
    },
  };
}

export type BoardStore = ReturnType<typeof createBoardStore>;

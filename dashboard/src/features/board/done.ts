import type { Board } from "../../api/types";
import { DONE } from "./columns";

/** Keep completed cards on their own screen, including version lanes and phone navigation. */
export function boardView(board: Board, done = false): Board {
  return {
    ...board,
    columns: board.columns.filter((c) => (c.name === DONE) === done),
    cards: board.cards.filter((c) => (c.column === DONE) === done),
  };
}

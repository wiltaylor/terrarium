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

/**
 * The link of the done toggle: the done screen and the board swap, and the
 * version filter and the grouping stay (TER-245). The open card, the phone
 * column and other query keys do not carry over.
 */
export function doneToggleHref(base: string, done: boolean, search: { version?: string; group?: string }): string {
  const q = new URLSearchParams();
  const version = search.version?.trim();
  if (version) q.set("version", version);
  if (search.group === "version") q.set("group", "version");
  const query = q.toString();
  return `${base}/board${done ? "" : "/done"}${query ? `?${query}` : ""}`;
}

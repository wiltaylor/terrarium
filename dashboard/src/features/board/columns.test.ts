import { describe, expect, it } from "vitest";
import type { Column } from "../../api/types";
import { COLUMN_ROLES } from "../../api/types";
import { checkColumnEdits } from "../../api/mock/boardColumns";
import {
  addRow,
  canMove,
  checkColumns,
  columnTitle,
  dragRow,
  isBlockedColumn,
  isClean,
  moveRow,
  overAfterSave,
  overLimit,
  removeBlocker,
  roleColumn,
  rowsFromBoard,
  setRole,
  toEdits,
} from "./columns";

const board: Column[] = [
  { name: "backlog", title: "backlog", position: 0, count: 2 },
  { name: "ready", title: "ready", position: 1, count: 0 },
  { name: "in_progress", title: "in progress", position: 2, wip_limit: 3, role: "working", count: 1 },
  { name: "review", title: "Code review", position: 3, count: 0 },
  { name: "blocked", title: "blocked", position: 4, role: "blocked", count: 0 },
  { name: "done", title: "done", position: 5, count: 4 },
];

describe("board columns", () => {
  it.each(["validating", "review", "merge_queue", "accepted"] as const)("round-trips the %s role through edits and the mock", (role) => {
    expect(COLUMN_ROLES).toContain(role);
    const rows = setRole(rowsFromBoard(board), 1, role);
    expect(isClean(checkColumns(rows))).toBe(true);
    const saved = checkColumnEdits(board, toEdits(rows));
    expect(roleColumn(saved, role)?.name).toBe("ready");
    expect(rowsFromBoard(saved)[1]!.role).toBe(role);
    expect(roleColumn([{ name: role, position: 0 }], role)?.name).toBe(role);
  });

  it("finds the role columns, with the old names for a daemon without roles", () => {
    expect(roleColumn(board, "working")?.name).toBe("in_progress");
    expect(isBlockedColumn(board, "blocked")).toBe(true);
    const renamed = board.map((c) => (c.name === "blocked" ? { ...c, name: "stuck" } : c));
    expect(isBlockedColumn(renamed, "stuck")).toBe(true);
    const old = board.map(({ role: _r, ...c }) => c);
    expect(roleColumn(old, "working")?.name).toBe("in_progress");
    const none = board.map((c) => (c.role === "blocked" ? { ...c, role: undefined } : c));
    expect(roleColumn(none, "blocked")).toBeUndefined();
  });

  it("titles and the over-limit state", () => {
    expect(columnTitle({ name: "in_progress" })).toBe("in progress");
    expect(columnTitle({ name: "review", title: "Code review" })).toBe("Code review");
    expect(overLimit({ wip_limit: 2 }, 3)).toBe(true);
    expect(overLimit({ wip_limit: 2 }, 2)).toBe(false);
    expect(overLimit({}, 9)).toBe(false);
  });

  it("round-trips the board with no change", () => {
    const rows = rowsFromBoard(board);
    expect(isClean(checkColumns(rows))).toBe(true);
    expect(toEdits(rows)).toEqual([
      { name: "backlog", title: null, role: null },
      { name: "ready", title: null, role: null },
      { name: "in_progress", title: null, role: "working", wip_limit: 3 },
      { name: "review", title: "Code review", role: null },
      { name: "blocked", title: null, role: "blocked" },
      { name: "done", title: null, role: null },
    ]);
  });

  it("adds before done, renames with rename_from, and keeps backlog and done in place", () => {
    let rows = addRow(rowsFromBoard(board));
    expect(rows.at(-2)!.name).toBe("");
    expect(rows.at(-1)!.name).toBe("done");
    rows = rows.map((r, i) => (i === rows.length - 2 ? { ...r, name: "qa", title: "QA", wip: "2" } : r));
    rows = rows.map((r) => (r.name === "in_progress" ? { ...r, name: "doing" } : r));
    const edits = toEdits(rows);
    expect(edits.find((e) => e.name === "doing")).toMatchObject({ rename_from: "in_progress", role: "working", wip_limit: 3 });
    expect(edits.at(-2)).toEqual({ name: "qa", title: "QA", role: null, wip_limit: 2 });
    expect(edits.at(-2)).not.toHaveProperty("rename_from");
    // Up and down: backlog and done do not move, and nothing passes them.
    expect(canMove(rows, 0, 1)).toBe(false);
    expect(canMove(rows, 1, -1)).toBe(false);
    expect(canMove(rows, rows.length - 2, 1)).toBe(false);
    expect(moveRow(rows, 1, 1).map((r) => r.name).slice(0, 3)).toEqual(["backlog", "doing", "ready"]);
    expect(dragRow(rows, 3, 0)[1]!.name).toBe("review");
    expect(dragRow(rows, 1, 99).at(-1)!.name).toBe("done");
    expect(dragRow(rows, 0, 3)[0]!.name).toBe("backlog");
  });

  it("checks names, titles, WIP limits, and roles before the save", () => {
    const rows = rowsFromBoard(board);
    const k = (name: string) => rows.find((r) => r.name === name)!.key;
    const edit = (f: (r: (typeof rows)[number]) => (typeof rows)[number]) => checkColumns(rows.map(f));
    expect(edit((r) => (r.name === "ready" ? { ...r, name: "Ready-1" } : r)).rows[k("ready")]?.name).toBeTruthy();
    expect(edit((r) => (r.name === "ready" ? { ...r, name: "review" } : r)).rows[k("ready")]?.name).toMatch(/Two columns/);
    expect(edit((r) => (r.name === "ready" ? { ...r, name: "done" } : r)).rows[k("ready")]?.name).toMatch(/fixed/);
    expect(edit((r) => (r.name === "ready" ? { ...r, name: "working" } : r)).rows[k("ready")]?.name).toMatch(/kept/);
    expect(edit((r) => (r.name === "ready" ? { ...r, title: "x".repeat(41) } : r)).rows[k("ready")]?.title).toBeTruthy();
    expect(edit((r) => (r.name === "ready" ? { ...r, wip: "0" } : r)).rows[k("ready")]?.wip).toBeTruthy();
    expect(edit((r) => (r.name === "ready" ? { ...r, wip: "1.5" } : r)).rows[k("ready")]?.wip).toBeTruthy();
    expect(edit((r) => (r.name === "backlog" ? { ...r, wip: "3" } : r)).rows[k("backlog")]?.wip).toMatch(/no WIP/);
    expect(edit((r) => (r.role === "working" ? { ...r, role: "" } : r)).list[0]).toMatch(/Working/);
    expect(edit((r) => (r.name === "ready" ? { ...r, role: "blocked" } : r)).list[0]).toMatch(/one column/);
    // A role moves: the old column loses it.
    const moved = setRole(rows, 1, "working");
    expect(moved.filter((r) => r.role === "working").map((r) => r.name)).toEqual(["ready"]);
    expect(isClean(checkColumns(moved))).toBe(true);
  });

  it("says why a column cannot go, and warns of a limit under the count", () => {
    const rows = rowsFromBoard(board);
    expect(removeBlocker(rows[0]!)).toMatch(/fixed/);
    expect(removeBlocker(rows[2]!)).toMatch(/1 card out/);
    expect(removeBlocker(rows[1]!)).toBeNull();
    expect(overAfterSave({ ...rows[2]!, wip: "1", count: 2 })).toBe(true);
    expect(overAfterSave({ ...rows[2]!, wip: "", count: 2 })).toBe(false);
  });
});

// The columns of a board (docs/api.md §8.1) and the Edit columns dialog: pure helpers. No Solid imports.
// The daemon checks the same rules again; these give the errors before the save.

import type { Column, ColumnEdit, ColumnRole } from "../../api/types";

/** The first column: new cards go there. It cannot be removed, renamed, or moved, and takes no WIP limit and no role. */
export const BACKLOG = "backlog";
/** The last column, fixed as `backlog` is. */
export const DONE = "done";
export const FIXED_COLUMNS = [BACKLOG, DONE] as const;
export const TITLE_MAX = 40;
/** A new column name: a lower-case letter, then at most 31 lower-case letters, digits, or underscores. */
export const COLUMN_NAME_RE = /^[a-z][a-z0-9_]{0,31}$/;

export const isFixed = (name: string) => name === BACKLOG || name === DONE;

/** The default title of a column: the name with spaces for the underscores. */
export const defaultTitle = (name: string) => name.replace(/_/g, " ");

/** The label of a column header. */
export const columnTitle = (c: Pick<Column, "name" | "title">) => c.title || defaultTitle(c.name);

/**
 * The column with `role`. A board from a daemon without roles: the old fixed names
 * (`in_progress` for working, `blocked` for blocked).
 */
export function roleColumn(columns: readonly Column[], role: ColumnRole): Column | undefined {
  if (columns.some((c) => c.role)) return columns.find((c) => c.role === role);
  return columns.find((c) => c.name === (role === "working" ? "in_progress" : role));
}

/** Is `name` the blocked column (a move there needs a comment)? */
export const isBlockedColumn = (columns: readonly Column[], name: string) => roleColumn(columns, "blocked")?.name === name;

/** More cards than the WIP limit: the column refuses more until cards leave. */
export const overLimit = (c: Pick<Column, "wip_limit">, count: number) => c.wip_limit !== undefined && count > c.wip_limit;

/** One row of the Edit columns dialog, as the user types it. */
export interface ColumnRow {
  /** A stable key for the list. */
  key: string;
  name: string;
  /** The name on the board before this edit; none for a new column. */
  original?: string;
  title: string;
  /** The WIP limit as typed: "" for none. */
  wip: string;
  role: ColumnRole | "";
  /** Active cards in the column (a new column: 0). */
  count: number;
}

let seq = 0;
const key = () => `col-${++seq}`;

export function rowsFromBoard(columns: readonly Column[]): ColumnRow[] {
  return columns.map((c) => ({
    key: key(),
    name: c.name,
    original: c.name,
    title: c.title && c.title !== defaultTitle(c.name) ? c.title : "",
    wip: c.wip_limit !== undefined ? String(c.wip_limit) : "",
    role: c.role ?? "",
    count: c.count ?? 0,
  }));
}

/** A new empty row, before `done`. */
export function addRow(rows: readonly ColumnRow[]): ColumnRow[] {
  const at = rows.findIndex((r) => r.original === DONE);
  const row: ColumnRow = { key: key(), name: "", title: "", wip: "", role: "", count: 0 };
  const out = [...rows];
  out.splice(at < 0 ? out.length : at, 0, row);
  return out;
}

/** Can the row at `i` move by `delta` (-1 up, +1 down)? `backlog` stays first and `done` last. */
export function canMove(rows: readonly ColumnRow[], i: number, delta: -1 | 1): boolean {
  const j = i + delta;
  const row = rows[i];
  const other = rows[j];
  if (!row || !other) return false;
  return !isFixed(row.original ?? "") && !isFixed(other.original ?? "");
}

export function moveRow(rows: readonly ColumnRow[], i: number, delta: -1 | 1): ColumnRow[] {
  if (!canMove(rows, i, delta)) return [...rows];
  const out = [...rows];
  [out[i], out[i + delta]] = [out[i + delta]!, out[i]!];
  return out;
}

/** Move the row `from` to the place of the row `to` (drag and drop). Fixed rows do not move, and nothing goes before `backlog` or after `done`. */
export function dragRow(rows: readonly ColumnRow[], from: number, to: number): ColumnRow[] {
  const row = rows[from];
  if (!row || isFixed(row.original ?? "") || from === to) return [...rows];
  const out = [...rows];
  out.splice(from, 1);
  const first = out.findIndex((r) => r.original === BACKLOG);
  const last = out.findIndex((r) => r.original === DONE);
  const at = Math.min(Math.max(to, first + 1), last < 0 ? out.length : last);
  out.splice(at, 0, row);
  return out;
}

/** Why the row cannot go, or null: a fixed column, or a column with cards. */
export function removeBlocker(row: ColumnRow): string | null {
  if (isFixed(row.original ?? "")) return `${row.original} is a fixed column`;
  if (row.count > 0) return `Move the ${row.count} card${row.count === 1 ? "" : "s"} out first`;
  return null;
}

/** Give `role` to the row at `i`; another row with it loses it (one column for each role). */
export function setRole(rows: readonly ColumnRow[], i: number, role: ColumnRole | ""): ColumnRow[] {
  return rows.map((r, j) => (j === i ? { ...r, role } : role && r.role === role ? { ...r, role: "" } : r));
}

export type RowErrors = Partial<Record<"name" | "title" | "wip", string>>;
export interface ColumnsCheck {
  rows: Record<string, RowErrors>;
  /** Errors of the whole list (the roles). */
  list: string[];
}

export function checkColumns(rows: readonly ColumnRow[]): ColumnsCheck {
  const out: ColumnsCheck = { rows: {}, list: [] };
  const bad = (r: ColumnRow, k: keyof RowErrors, m: string) => ((out.rows[r.key] ??= {})[k] = m);
  const seen = new Map<string, number>();
  for (const r of rows) seen.set(r.name.trim(), (seen.get(r.name.trim()) ?? 0) + 1);
  for (const r of rows) {
    const name = r.name.trim();
    const renamed = r.original !== name;
    if (!name) bad(r, "name", "Give a name");
    else if (renamed && !COLUMN_NAME_RE.test(name)) bad(r, "name", "a-z first, then a-z, 0-9, or _ (at most 32)");
    else if (renamed && isFixed(name)) bad(r, "name", `${name} is a fixed column`);
    else if ((seen.get(name) ?? 0) > 1) bad(r, "name", "Two columns have this name");
    else if ((name === "working" || name === "blocked") && r.role !== name) bad(r, "name", `The name ${name} is kept for the ${name} column`);
    if (r.title.trim().length > TITLE_MAX) bad(r, "title", `At most ${TITLE_MAX} characters`);
    if (isFixed(r.original ?? "") && r.wip.trim()) bad(r, "wip", `${r.original} takes no WIP limit`);
    else if (r.wip.trim()) {
      const n = Number(r.wip.trim());
      if (!Number.isInteger(n) || n < 1) bad(r, "wip", "A whole number, 1 or more");
    }
  }
  if (rows.some((r) => isFixed(r.original ?? "") && r.role)) out.list.push("backlog and done take no role.");
  const working = rows.filter((r) => r.role === "working").length;
  if (working !== 1) out.list.push("One column must have the role Working: card claim moves cards there.");
  if (rows.filter((r) => r.role === "blocked").length > 1) out.list.push("Only one column can have the role Blocked.");
  return out;
}

export const isClean = (c: ColumnsCheck) => !c.list.length && !Object.keys(c.rows).length;

/** The body of the save: the whole list, in order, with `rename_from` for a renamed column. */
export function toEdits(rows: readonly ColumnRow[]): ColumnEdit[] {
  return rows.map((r) => {
    const name = r.name.trim();
    const e: ColumnEdit = { name, title: r.title.trim() || null, role: r.role || null };
    if (r.wip.trim()) e.wip_limit = Number(r.wip.trim());
    if (r.original && r.original !== name) e.rename_from = r.original;
    return e;
  });
}

/** A row whose WIP limit is under its card count: the column is over its limit after the save. */
export const overAfterSave = (r: ColumnRow) => !!r.wip.trim() && Number(r.wip) >= 1 && r.count > Number(r.wip);

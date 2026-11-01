// The column rules of `PATCH /api/projects/{name}/board` (store/board.rs check_columns), for the mock.

import type { Column, ColumnEdit, ColumnRole } from "../types";
import { ApiError } from "../http";
import { BACKLOG, COLUMN_NAME_RE, defaultTitle, DONE, isFixed, TITLE_MAX } from "../../features/board/columns";

const bad = (m: string): never => {
  throw new ApiError(422, m, undefined, "invalid");
};

/** The new column list, or a 422. A column with cards that goes is the caller's check (409). */
export function checkColumnEdits(current: readonly Column[], edits: readonly ColumnEdit[]): Column[] {
  const byName = new Map(current.map((c) => [c.name, c]));
  if (edits[0]?.name !== BACKLOG || edits[0]?.rename_from) bad("`backlog` must be the first column; it cannot be removed, renamed, or moved");
  if (edits.at(-1)?.name !== DONE || edits.at(-1)?.rename_from) bad("`done` must be the last column; it cannot be removed, renamed, or moved");
  const names = new Set<string>();
  const sources = new Set<string>();
  const out: Column[] = edits.map((e, i) => {
    const name = e.name;
    if (names.has(name)) bad(`column \`${name}\` is twice in the list`);
    names.add(name);
    const source = e.rename_from ?? name;
    if (sources.has(source)) bad(`column \`${source}\` is twice in the list`);
    sources.add(source);
    let old: Column | undefined;
    if (e.rename_from) {
      old = byName.get(e.rename_from) ?? bad(`no column \`${e.rename_from}\` to rename`);
      if (isFixed(e.rename_from)) bad(`column \`${e.rename_from}\` cannot be renamed`);
      if (e.rename_from !== name && byName.has(name)) bad(`column \`${name}\` exists; rename it first, or choose another name`);
    } else old = byName.get(name);
    const isNew = (!!e.rename_from && e.rename_from !== name) || !byName.has(name);
    if (isNew && !COLUMN_NAME_RE.test(name)) bad(`bad column name \`${name}\`: a lower-case letter, then at most 31 lower-case letters, digits, or underscores`);
    if (isNew && isFixed(name)) bad(`\`${name}\` is a fixed column; another column cannot take its name`);
    if (e.wip_limit !== undefined && e.wip_limit !== null) {
      if (isFixed(name)) bad(`column \`${name}\` takes no WIP limit`);
      if (!Number.isInteger(e.wip_limit) || e.wip_limit < 1) bad(`column \`${name}\`: wip_limit must be 1 or more, or null`);
    }
    const role: ColumnRole | undefined = e.role === undefined ? old?.role : (e.role ?? undefined);
    if (role !== undefined && role !== "working" && role !== "blocked") bad(`column \`${name}\`: bad role \`${String(role)}\`; use working, blocked, or null`);
    if (role && isFixed(name)) bad(`column \`${name}\` takes no role`);
    if ((name === "working" || name === "blocked") && role !== name) bad(`the name \`${name}\` is kept for the column with the role \`${name}\`; give this column the role \`${name}\`, or another name`);
    const rawTitle = e.title === undefined ? old?.title : e.title;
    const t = rawTitle?.trim() ?? "";
    if (t.length > TITLE_MAX) bad(`column \`${name}\`: the title has more than ${TITLE_MAX} characters`);
    const title = t && t !== defaultTitle(name) ? t : undefined;
    return { name, position: i, ...(title ? { title } : {}), ...(e.wip_limit ? { wip_limit: e.wip_limit } : {}), ...(role ? { role } : {}) };
  });
  const working = out.filter((c) => c.role === "working").length;
  if (working !== 1) bad(`exactly one column must have the role \`working\` (\`terrarium card claim\` moves a card there); this list has ${working}`);
  const blocked = out.filter((c) => c.role === "blocked").length;
  if (blocked > 1) bad(`at most one column can have the role \`blocked\`; this list has ${blocked}`);
  return out;
}

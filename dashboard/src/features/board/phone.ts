// The phone layout of the Cards tab: pure helpers. No Solid imports. On a
// phone (max-width 760px, styles/global.css) the board shows one column at
// a time. The URL query keeps the column: `?col=<column name>`. The filters
// fold into a Filters button with a count of the filters that apply.

/** The phone breakpoint of the dashboard. It is the same as the `max-width: 760px` rules of styles/global.css. */
export const PHONE_QUERY = "(max-width: 760px)";

/**
 * The column that the phone board shows, from the `col` query value. A
 * value that is not a column of the board (an old link, a column that the
 * board no longer has) gives the first column. A board with no columns
 * gives undefined.
 */
export function columnFromQuery(raw: string | undefined, columns: readonly string[]): string | undefined {
  if (raw && columns.includes(raw)) return raw;
  return columns[0];
}

/** The query value for a column: no `col` key for the first column, so the plain board URL stays plain. */
export function columnQuery(column: string, columns: readonly string[]): string | undefined {
  return column === columns[0] ? undefined : column;
}

/** The number of filters that apply. An empty value is "any". */
export function activeFilterCount(filters: { label: string; version: string; assignee: string }): number {
  return [filters.label, filters.version, filters.assignee].filter((v) => v !== "").length;
}

/**
 * The column nearest to the scroll position of the board: the one whose
 * left edge is nearest to `scrollLeft`. Each entry is a column name and the
 * left edge of the column in the scroll box. Undefined for no columns.
 */
export function nearestColumn(scrollLeft: number, columns: readonly { name: string; left: number }[]): string | undefined {
  let best: { name: string; left: number } | undefined;
  for (const c of columns) {
    if (!best || Math.abs(c.left - scrollLeft) < Math.abs(best.left - scrollLeft)) best = c;
  }
  return best?.name;
}

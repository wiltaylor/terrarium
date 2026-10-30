// Parse the unified diff hunks of the repo browser (docs/api.md §5.3) into
// rows with old and new line numbers. Pure, so tests can read it. The text
// stays text: the view puts each row in a text node.

export type DiffRowKind = "hunk" | "add" | "del" | "ctx" | "note";

export interface DiffRow {
  kind: DiffRowKind;
  /** The line number in the old file (del, ctx). */
  old?: number;
  /** The line number in the new file (add, ctx). */
  new?: number;
  /** The line without its `+`, `-`, or ` ` mark. A hunk row has its `@@` header. */
  text: string;
}

const HUNK = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;

export function parsePatch(patch: string): DiffRow[] {
  const rows: DiffRow[] = [];
  let o = 0;
  let n = 0;
  const lines = patch.split("\n");
  // A patch ends with a newline: no empty last row.
  if (lines.length && lines[lines.length - 1] === "") lines.pop();
  for (const line of lines) {
    const h = HUNK.exec(line);
    if (h) {
      o = Number(h[1]);
      n = Number(h[2]);
      rows.push({ kind: "hunk", text: line });
    } else if (line.startsWith("+")) {
      rows.push({ kind: "add", new: n++, text: line.slice(1) });
    } else if (line.startsWith("-")) {
      rows.push({ kind: "del", old: o++, text: line.slice(1) });
    } else if (line.startsWith("\\")) {
      rows.push({ kind: "note", text: line });
    } else {
      rows.push({ kind: "ctx", old: o++, new: n++, text: line.startsWith(" ") ? line.slice(1) : line });
    }
  }
  return rows;
}

/** The extensions that the file viewer shows as an image (never SVG). */
export const IMAGE_TYPES: Record<string, string> = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp" };

export function imageType(path: string): string | undefined {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return path.includes(".") ? IMAGE_TYPES[ext] : undefined;
}

export const isMarkdown = (path: string): boolean => /\.(md|markdown)$/i.test(path);

/** `a/b/c` -> [["a", "a"], ["b", "a/b"], ["c", "a/b/c"]]. */
export function crumbs(path: string): [string, string][] {
  const parts = path.split("/").filter(Boolean);
  return parts.map((p, i) => [p, parts.slice(0, i + 1).join("/")]);
}

/** The directory of a file path ("" at the root). */
export const dirOf = (path: string): string => (path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "");

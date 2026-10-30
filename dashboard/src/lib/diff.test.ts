import { describe, expect, it } from "vitest";
import { crumbs, dirOf, imageType, isMarkdown, parsePatch } from "./diff";

describe("parsePatch", () => {
  it("numbers the old and the new lines of each hunk", () => {
    const rows = parsePatch("@@ -3,3 +3,4 @@ fn x\n a\n-b\n+B\n+C\n c\n\\ No newline at end of file\n");
    expect(rows.map((r) => [r.kind, r.old, r.new, r.text])).toEqual([
      ["hunk", undefined, undefined, "@@ -3,3 +3,4 @@ fn x"],
      ["ctx", 3, 3, "a"],
      ["del", 4, undefined, "b"],
      ["add", undefined, 4, "B"],
      ["add", undefined, 5, "C"],
      ["ctx", 5, 6, "c"],
      ["note", undefined, undefined, "\\ No newline at end of file"],
    ]);
  });

  it("reads a hunk with no count and keeps markup as text", () => {
    const rows = parsePatch("@@ -0,0 +1 @@\n+<script>alert(1)</script>\n");
    expect(rows[1]).toEqual({ kind: "add", new: 1, text: "<script>alert(1)</script>" });
    expect(parsePatch("")).toEqual([]);
  });
});

describe("file helpers", () => {
  it("shows only raster images, never SVG", () => {
    expect(imageType("docs/logo.PNG")).toBe("image/png");
    expect(imageType("a.webp")).toBe("image/webp");
    expect(imageType("icon.svg")).toBeUndefined();
    expect(imageType("png")).toBeUndefined();
    expect(isMarkdown("README.md")).toBe(true);
    expect(isMarkdown("a.rs")).toBe(false);
  });

  it("makes breadcrumbs and the parent dir", () => {
    expect(crumbs("a/b/c")).toEqual([
      ["a", "a"],
      ["b", "a/b"],
      ["c", "a/b/c"],
    ]);
    expect(crumbs("")).toEqual([]);
    expect(dirOf("a/b/c.txt")).toBe("a/b");
    expect(dirOf("c.txt")).toBe("");
  });
});

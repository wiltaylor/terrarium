import { describe, expect, it } from "vitest";
import type { TreeEntry, TreeListing } from "../../api/types";
import { ancestorDirs, createFileTree, createTreeCache, entryAt, isEntryKey, sortEntries, visibleRows, type DirState } from "./fileTree";

const dir = (path: string): TreeEntry => ({ name: path.split("/").pop()!, path, kind: "dir" });
const file = (path: string): TreeEntry => ({ name: path.split("/").pop()!, path, kind: "file", size: 1 });
const ready = (...entries: TreeEntry[]): DirState => ({ status: "ready", entries: sortEntries(entries), truncated: false });

// A repo: README.md, src/main.rs, docs/guides/README.md, docs/logo.png.
const FILES: Record<string, TreeEntry[]> = {
  "": [file("README.md"), dir("src"), dir("docs"), file(".gitignore")],
  src: [file("src/main.rs")],
  docs: [file("docs/logo.png"), dir("docs/guides")],
  "docs/guides": [file("docs/guides/README.md")],
};

/** A loader that counts its calls, and fails for an unknown folder. */
function loader() {
  const calls: string[] = [];
  const load = (d: string): Promise<TreeListing> => {
    calls.push(d);
    const entries = FILES[d];
    return entries ? Promise.resolve({ path: d, entries, truncated: false }) : Promise.reject(new Error(`no directory ${d}`));
  };
  return { calls, load };
}

const paths = (rows: ReturnType<typeof visibleRows>) => rows.map((r) => (r.type === "entry" ? `${"  ".repeat(r.depth)}${r.entry.name}${r.open ? "/-" : r.entry.kind === "dir" ? "/+" : ""}` : `${"  ".repeat(r.depth)}(${r.note})`));

describe("sortEntries", () => {
  it("puts folders first, then the other entries, each group in name order", () => {
    const s = sortEntries([file("b.txt"), dir("z"), file("C.md"), { name: "l", path: "l", kind: "symlink", target: "x" }, dir("a"), file("a.md"), file("A.md")]);
    expect(s.map((e) => e.name)).toEqual(["a", "z", "A.md", "a.md", "b.txt", "C.md", "l"]);
  });
});

describe("ancestorDirs", () => {
  it("gives the folders above a path, from the root", () => {
    expect(ancestorDirs("a/b/c.md")).toEqual(["", "a", "a/b"]);
    expect(ancestorDirs("README.md")).toEqual([""]);
    expect(ancestorDirs("")).toEqual([]);
  });
});

describe("visibleRows", () => {
  const dirs = { "": ready(...FILES[""]!), docs: ready(...FILES.docs!), "docs/guides": { status: "loading" } as DirState };

  it("shows the root, and each open folder under its row", () => {
    expect(paths(visibleRows(dirs, new Set()))).toEqual(["docs/+", "src/+", ".gitignore", "README.md"]);
    expect(paths(visibleRows(dirs, new Set(["docs", "docs/guides"])))).toEqual(["docs/-", "  guides/-", "    (loading)", "  logo.png", "src/+", ".gitignore", "README.md"]);
  });

  it("shows a note for a folder that is empty, cut, or did not load", () => {
    const d = { "": ready(dir("e"), dir("x"), dir("t")), e: ready(), x: { status: "error", error: new Error("no") } as DirState, t: { status: "ready", entries: [file("t/1")], truncated: true } as DirState };
    expect(paths(visibleRows(d, new Set(["e", "x", "t"])))).toEqual(["e/-", "  (empty)", "t/-", "  1", "  (truncated)", "x/-", "  (error)"]);
  });

  it("gives note rows keys that no path can have", () => {
    const rows = visibleRows(dirs, new Set(["docs", "docs/guides"]));
    expect(rows.filter((r) => !isEntryKey(r.key)).map((r) => r.type)).toEqual(["note"]);
  });

  it("filters the loaded entries by name (no case), with the folders above each match open", () => {
    const d = { ...dirs, "docs/guides": ready(...FILES["docs/guides"]!) };
    // The open set does not matter while a filter is on.
    expect(paths(visibleRows(d, new Set(), "readme"))).toEqual(["docs/-", "  guides/-", "    README.md", "README.md"]);
    expect(paths(visibleRows(d, new Set(), "GUI"))).toEqual(["docs/-", "  guides/+"]);
    // src is not loaded: its files do not match.
    expect(paths(visibleRows(d, new Set(), "main"))).toEqual([]);
    expect(paths(visibleRows(d, new Set(["docs"]), "  "))).toEqual(paths(visibleRows(d, new Set(["docs"]))));
  });
});

describe("createFileTree", () => {
  it("loads a folder once, on the first open, and keeps it", async () => {
    const { calls, load } = loader();
    const t = createFileTree(load);
    await t.ensure("");
    await t.open("src");
    t.close("src");
    await t.open("src");
    await Promise.all([t.ensure("docs"), t.ensure("docs")]);
    expect(calls).toEqual(["", "src", "docs"]);
    expect([...t.expanded()]).toEqual(["src"]);
    expect(t.entry("src/main.rs")?.kind).toBe("file");
    expect(t.entry("nope")).toBeUndefined();
  });

  it("opens and loads each folder above a deep path", async () => {
    const { calls, load } = loader();
    const t = createFileTree(load);
    await t.reveal("docs/guides/README.md");
    expect(calls.sort()).toEqual(["", "docs", "docs/guides"]);
    expect([...t.expanded()].sort()).toEqual(["docs", "docs/guides"]);
    expect(entryAt(t.dirs(), "docs/guides/README.md")?.name).toBe("README.md");
    expect(paths(visibleRows(t.dirs(), t.expanded()))).toEqual(["docs/-", "  guides/-", "    README.md", "  logo.png", "src/+", ".gitignore", "README.md"]);
  });

  it("keeps a failed load as an error, and tries again on the next open", async () => {
    const { calls, load } = loader();
    const t = createFileTree(load);
    await t.open("gone");
    expect(t.dirs().gone?.status).toBe("error");
    await t.open("gone");
    expect(calls).toEqual(["gone", "gone"]);
  });

  it("toggles a folder", async () => {
    const t = createFileTree(loader().load);
    t.toggle("src");
    expect(t.expanded().has("src")).toBe(true);
    t.toggle("src");
    expect(t.expanded().has("src")).toBe(false);
  });
});

describe("createTreeCache", () => {
  it("keeps one tree for each key, and drops the oldest when full", () => {
    const cache = createTreeCache(2);
    const { load } = loader();
    const a = cache("a", load);
    expect(cache("a", load)).toBe(a);
    const b = cache("b", load);
    cache("a", load); // a is now the newest
    cache("c", load); // b goes
    expect(cache("a", load)).toBe(a);
    expect(cache("b", load)).not.toBe(b);
  });
});

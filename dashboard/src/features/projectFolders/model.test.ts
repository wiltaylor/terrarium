import { describe, expect, it } from "vitest";
import type { ProjectFolder, ProjectPlacement } from "../../api/types";
import {
  ancestors,
  buildTree,
  canMoveTo,
  folderKey,
  folderList,
  folderNode,
  folderOfNode,
  folderPath,
  folderTreeKeys,
  parentFolders,
  projectAncestors,
  projectCount,
  type TreeEntry,
} from "./model";

const f = (id: string, parent: string | null, position: number, name = id.toUpperCase()): ProjectFolder => ({
  id,
  name,
  parent_id: parent,
  position,
  created_at: "t",
  updated_at: "t",
});
const at = (project: string, folder: string | null, position: number): ProjectPlacement => ({ project, folder_id: folder, position });

/** The tree as text: `[folder: ...children]` and project names. */
const show = (entries: TreeEntry[]): string[] => entries.map((e) => (e.kind === "project" ? e.name : `[${e.folder.name}: ${show(e.children).join(" ")}]`));

// work (0) > infra (0); home (1) at the root.
const folders = [f("home", null, 1), f("infra", "work", 0), f("work", null, 0)];

describe("project folders model", () => {
  it("builds the tree: folders first in position order, then placed projects, then the rest by name", () => {
    const tree = buildTree(folders, [at("api", "infra", 1), at("web", "infra", 0), at("ops", null, 0), at("site", "work", 0)], ["zed", "api", "ops", "web", "site", "abc"]);
    expect(show(tree)).toEqual(["[WORK: [INFRA: web api] site]", "[HOME: ]", "ops", "abc", "zed"]);
  });

  it("puts projects of a gone folder, and folders of a gone parent, at the root, and leaves out unknown projects", () => {
    const tree = buildTree([f("lost", "gone", 0)], [at("a", "gone", 0), at("ghost", null, 0)], ["a"]);
    expect(show(tree)).toEqual(["[LOST: ]", "a"]);
  });

  it("shows a loop of parents at the root, once", () => {
    const tree = buildTree([f("a", "b", 0), f("b", "a", 0)], [], []);
    expect(show(tree).sort()).toEqual(["[A: ]", "[B: ]"]);
  });

  it("gives the ancestors, the path, and the folders of a project", () => {
    expect(ancestors(folders, "infra")).toEqual(["work"]);
    expect(ancestors(folders, "work")).toEqual([]);
    expect(folderPath(folders, "infra")).toBe("WORK / INFRA");
    expect(projectAncestors(folders, [at("api", "infra", 0)], "api")).toEqual(["work", "infra"]);
    expect(projectAncestors(folders, [at("api", null, 0)], "api")).toEqual([]);
    expect(projectAncestors(folders, [], "api")).toEqual([]);
  });

  it("refuses a move into itself, a subfolder, too deep, or where it is", () => {
    const pl = [at("api", "infra", 0)];
    expect(canMoveTo(folders, pl, { kind: "folder", id: "work" }, "infra")).toBe(false);
    expect(canMoveTo(folders, pl, { kind: "folder", id: "work" }, "work")).toBe(false);
    expect(canMoveTo(folders, pl, { kind: "folder", id: "work" }, "home")).toBe(true);
    expect(canMoveTo(folders, pl, { kind: "folder", id: "infra" }, "work")).toBe(false);
    expect(canMoveTo(folders, pl, { kind: "folder", id: "infra" }, null)).toBe(true);
    expect(canMoveTo(folders, pl, { kind: "folder", id: "work" }, null)).toBe(false);
    expect(canMoveTo(folders, pl, { kind: "project", name: "api" }, "infra")).toBe(false);
    expect(canMoveTo(folders, pl, { kind: "project", name: "api" }, null)).toBe(true);
    expect(canMoveTo(folders, pl, { kind: "project", name: "new" }, null)).toBe(false);
    expect(canMoveTo(folders, pl, { kind: "project", name: "new" }, "home")).toBe(true);
    expect(canMoveTo(folders, pl, { kind: "project", name: "api" }, "gone")).toBe(false);

    // A chain of 8: nothing goes under the last; a folder with a subfolder cannot go under level 7.
    const chain = Array.from({ length: 8 }, (_, i) => f(`l${i + 1}`, i ? `l${i}` : null, 0));
    const all = [...chain, f("x", null, 1), f("y", "x", 0)];
    expect(canMoveTo(all, [], { kind: "folder", id: "y" }, "l8")).toBe(false);
    expect(canMoveTo(all, [], { kind: "folder", id: "y" }, "l7")).toBe(true);
    expect(canMoveTo(all, [], { kind: "folder", id: "x" }, "l7")).toBe(false);
    expect(canMoveTo(all, [], { kind: "folder", id: "x" }, "l6")).toBe(true);
  });

  it("lists the folders in tree order with their level", () => {
    expect(folderList(folders).map((x) => `${x.folder.id}:${x.level}`)).toEqual(["work:1", "infra:2", "home:1"]);
  });

  it("gives the rows in screen order, and the parent folder of each row", () => {
    const tree = buildTree(folders, [at("api", "infra", 0), at("site", "work", 0)], ["api", "site", "ops"]);
    const rows = (p: string) => [`p:${p}`];
    expect(folderTreeKeys(tree, () => false, rows)).toEqual(["f:work", "f:home", "p:ops"]);
    const open = new Set([folderNode("work"), folderNode("infra")]);
    expect(folderTreeKeys(tree, (n) => open.has(n), rows)).toEqual(["f:work", "f:infra", "p:api", "p:site", "f:home", "p:ops"]);
    const parents = parentFolders(tree);
    expect(parents.projects.get("api")).toBe(folderKey("infra"));
    expect(parents.projects.get("ops")).toBeUndefined();
    expect(parents.folders.get("infra")).toBe(folderKey("work"));
    expect(parents.folders.get("work")).toBeUndefined();
    expect(projectCount(tree[0]!)).toBe(2);
  });

  it("names folders in the open set apart from projects", () => {
    expect(folderNode("01ABC")).toBe("folder:01ABC");
    expect(folderOfNode("folder:01ABC")).toBe("01ABC");
    expect(folderOfNode("alpha")).toBeNull();
  });
});

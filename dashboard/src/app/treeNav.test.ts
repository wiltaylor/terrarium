import { describe, expect, it } from "vitest";
import { treeKeyAction } from "./treeNav";

describe("treeKeyAction", () => {
  // The Agents tree, open: the root and two agents.
  const keys = ["root", "a:1", "a:2"];
  const root = { key: "root", open: true };
  const leaf = (key: string) => ({ key, parent: "root" });

  it("moves up, down, to the first row, and to the last row", () => {
    expect(treeKeyAction(keys, root, "ArrowDown")).toEqual({ focus: "a:1" });
    expect(treeKeyAction(keys, leaf("a:1"), "ArrowDown")).toEqual({ focus: "a:2" });
    expect(treeKeyAction(keys, leaf("a:2"), "ArrowDown")).toEqual({ none: true });
    expect(treeKeyAction(keys, leaf("a:2"), "ArrowUp")).toEqual({ focus: "a:1" });
    expect(treeKeyAction(keys, root, "ArrowUp")).toEqual({ none: true });
    expect(treeKeyAction(keys, leaf("a:2"), "Home")).toEqual({ focus: "root" });
    expect(treeKeyAction(keys, root, "End")).toEqual({ focus: "a:2" });
  });

  it("opens and closes a node with Right and Left, and goes to the child and the parent", () => {
    expect(treeKeyAction(["root"], { key: "root", open: false }, "ArrowRight")).toEqual({ open: true });
    expect(treeKeyAction(keys, root, "ArrowRight")).toEqual({ focus: "a:1" });
    expect(treeKeyAction(["root"], { key: "root", open: true }, "ArrowRight")).toEqual({ none: true });
    expect(treeKeyAction(keys, root, "ArrowLeft")).toEqual({ close: true });
    expect(treeKeyAction(["root"], { key: "root", open: false }, "ArrowLeft")).toEqual({ none: true });
    expect(treeKeyAction(keys, leaf("a:1"), "ArrowLeft")).toEqual({ focus: "root" });
    // Right on a leaf is not for the tree.
    expect(treeKeyAction(keys, leaf("a:1"), "ArrowRight")).toBeNull();
  });

  it("lets Enter follow a link, clicks a link on Space, and activates a button row on both", () => {
    expect(treeKeyAction(keys, leaf("a:1"), "Enter")).toBeNull();
    expect(treeKeyAction(keys, leaf("a:1"), " ")).toEqual({ click: true });
    const project = { key: "p:x", parent: "root", open: false, button: true };
    expect(treeKeyAction(["root", "p:x"], project, "Enter")).toEqual({ activate: true });
    expect(treeKeyAction(["root", "p:x"], project, " ")).toEqual({ activate: true });
    expect(treeKeyAction(keys, leaf("a:1"), "a")).toBeNull();
  });
});

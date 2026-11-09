import { describe, expect, it } from "vitest";
import { treeKeyAction } from "./treeNav";

describe("treeKeyAction", () => {
  // A tree, open: the root and two rows under it.
  const keys = ["root", "a:1", "a:2"];
  const root = { key: "root", open: true };
  const leaf = (key: string) => ({ key, parent: "root" });

  it("gives m, F2, and Delete only to the rows that have them", () => {
    const keys = ["root", "f:1", "p:a"];
    expect(treeKeyAction(keys, { key: "p:a", move: true }, "m")).toEqual({ move: true });
    expect(treeKeyAction(keys, { key: "p:a" }, "m")).toBeNull();
    expect(treeKeyAction(keys, { key: "f:1", edit: true }, "F2")).toEqual({ rename: true });
    expect(treeKeyAction(keys, { key: "f:1", edit: true }, "Delete")).toEqual({ remove: true });
    expect(treeKeyAction(keys, { key: "p:a" }, "Delete")).toBeNull();
  });
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

  it("walks one project row after another and activates a project on Enter and Space", () => {
    const keys = ["root", "p:x", "p:y"];
    const project = { key: "p:x", parent: "root", button: true };
    expect(treeKeyAction(keys, project, "ArrowDown")).toEqual({ focus: "p:y" });
    expect(treeKeyAction(keys, project, "ArrowLeft")).toEqual({ focus: "root" });
    expect(treeKeyAction(keys, project, "ArrowRight")).toBeNull();
    expect(treeKeyAction(keys, project, "Enter")).toEqual({ activate: true });
    expect(treeKeyAction(keys, project, " ")).toEqual({ activate: true });
    expect(treeKeyAction(keys, { key: "p:y", parent: "root", button: true }, "ArrowUp")).toEqual({ focus: "p:x" });
  });

  it("gives `+` to a row with an add action only", () => {
    expect(treeKeyAction(keys, leaf("a:1"), "+")).toBeNull();
    expect(treeKeyAction(keys, { ...leaf("a:1"), button: true }, "+")).toBeNull();
  });

});

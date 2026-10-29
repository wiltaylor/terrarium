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

  it("walks the third level: a project, its Agents node, and the agents", () => {
    const keys = ["root", "p:x", "i:x/agents", "pa:1", "pa:2", "i:x/board"];
    const node = (open: boolean) => ({ key: "i:x/agents", parent: "p:x", open });
    const agent = (key: string) => ({ key, parent: "i:x/agents" });
    expect(treeKeyAction(["root", "p:x", "i:x/agents", "i:x/board"], node(false), "ArrowRight")).toEqual({ open: true });
    expect(treeKeyAction(keys, node(true), "ArrowRight")).toEqual({ focus: "pa:1" });
    expect(treeKeyAction(keys, node(true), "ArrowLeft")).toEqual({ close: true });
    expect(treeKeyAction(keys, node(false), "ArrowLeft")).toEqual({ focus: "p:x" });
    expect(treeKeyAction(keys, agent("pa:2"), "ArrowLeft")).toEqual({ focus: "i:x/agents" });
    expect(treeKeyAction(keys, agent("pa:2"), "ArrowDown")).toEqual({ focus: "i:x/board" });
    expect(treeKeyAction(keys, agent("pa:1"), "ArrowUp")).toEqual({ focus: "i:x/agents" });
    expect(treeKeyAction(keys, agent("pa:1"), "End")).toEqual({ focus: "i:x/board" });
    // The node is a link: Enter follows it, Space clicks it.
    expect(treeKeyAction(keys, node(true), "Enter")).toBeNull();
    expect(treeKeyAction(keys, node(true), " ")).toEqual({ click: true });
  });
});

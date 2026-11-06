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

  it("walks the third level: a project, its pages, its Agents node last, and the agents", () => {
    const keys = ["root", "p:x", "i:x/board", "i:x/schedules", "i:x/agents", "pa:1", "pa:2", "p:y"];
    const node = (open: boolean) => ({ key: "i:x/agents", parent: "p:x", open });
    const agent = (key: string) => ({ key, parent: "i:x/agents" });
    expect(treeKeyAction(keys, { key: "i:x/schedules", parent: "p:x" }, "ArrowDown")).toEqual({ focus: "i:x/agents" });
    expect(treeKeyAction(["root", "p:x", "i:x/board", "i:x/agents", "p:y"], node(false), "ArrowRight")).toEqual({ open: true });
    expect(treeKeyAction(keys, node(true), "ArrowRight")).toEqual({ focus: "pa:1" });
    expect(treeKeyAction(keys, node(true), "ArrowUp")).toEqual({ focus: "i:x/schedules" });
    expect(treeKeyAction(keys, node(true), "ArrowLeft")).toEqual({ close: true });
    expect(treeKeyAction(keys, node(false), "ArrowLeft")).toEqual({ focus: "p:x" });
    expect(treeKeyAction(keys, agent("pa:2"), "ArrowLeft")).toEqual({ focus: "i:x/agents" });
    expect(treeKeyAction(keys, agent("pa:2"), "ArrowDown")).toEqual({ focus: "p:y" });
    expect(treeKeyAction(keys, agent("pa:1"), "ArrowUp")).toEqual({ focus: "i:x/agents" });
    expect(treeKeyAction(keys, agent("pa:1"), "End")).toEqual({ focus: "p:y" });
    // The node is not a link: Enter and Space toggle it, and `+` runs its add action.
    const button = { ...node(true), button: true, add: true };
    expect(treeKeyAction(keys, button, "Enter")).toEqual({ activate: true });
    expect(treeKeyAction(keys, button, " ")).toEqual({ activate: true });
    expect(treeKeyAction(keys, button, "+")).toEqual({ add: true });
  });

  it("gives `p` to an agent row only: pin or unpin", () => {
    expect(treeKeyAction(keys, { ...leaf("pa:1"), pin: true }, "p")).toEqual({ pin: true });
    expect(treeKeyAction(keys, leaf("a:1"), "p")).toBeNull();
  });

  it("gives `+` to a row with an add action only", () => {
    expect(treeKeyAction(keys, leaf("a:1"), "+")).toBeNull();
    expect(treeKeyAction(keys, { ...leaf("a:1"), button: true }, "+")).toBeNull();
  });

  it("walks a Repos node and its repos before the next page", () => {
    const keys = ["root", "p:x", "i:x/board", "i:x/repos", "pr:x/infra", "pr:x/web", "i:x/logs", "i:x/agents"];
    const repo = (key: string) => ({ key, parent: "i:x/repos" });
    const node = { key: "i:x/repos", parent: "p:x", open: true, button: true, add: true };
    expect(treeKeyAction(keys, node, "ArrowRight")).toEqual({ focus: "pr:x/infra" });
    expect(treeKeyAction(keys, repo("pr:x/web"), "ArrowDown")).toEqual({ focus: "i:x/logs" });
    expect(treeKeyAction(keys, repo("pr:x/web"), "ArrowLeft")).toEqual({ focus: "i:x/repos" });
    expect(treeKeyAction(keys, repo("pr:x/web"), "Enter")).toBeNull();
  });
});

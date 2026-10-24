import type { WikiPageSummary } from "../../api/types";

export interface TreeNode {
  name: string;
  path: string;
  page?: WikiPageSummary;
  children: TreeNode[];
}

/** Build a folder tree from slash paths. `index` sorts first, then folders, then pages. */
export function buildTree(pages: WikiPageSummary[]): TreeNode[] {
  const root: TreeNode = { name: "", path: "", children: [] };
  for (const p of pages) {
    const parts = p.path.split("/");
    let node = root;
    parts.forEach((part, i) => {
      const path = parts.slice(0, i + 1).join("/");
      let child = node.children.find((c) => c.name === part);
      if (!child) {
        child = { name: part, path, children: [] };
        node.children.push(child);
      }
      if (i === parts.length - 1) child.page = p;
      node = child;
    });
  }
  const sort = (nodes: TreeNode[]) => {
    nodes.sort((a, b) => {
      if (a.path === "index") return -1;
      if (b.path === "index") return 1;
      const fa = a.children.length > 0 ? 0 : 1;
      const fb = b.children.length > 0 ? 0 : 1;
      return fa - fb || a.name.localeCompare(b.name);
    });
    nodes.forEach((n) => sort(n.children));
    return nodes;
  };
  return sort(root.children);
}

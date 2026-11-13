import type { ProjectPageList } from "../../api/types";

/**
 * Where a project click goes (TER-1077), as a path under `/p/<name>`. With
 * project pages ON: the Board built-in page, else the first page, else the
 * legacy Board route. OFF, a failed read, or no capability: the Board route.
 */
export function projectIndexPath(list: ProjectPageList | undefined): string {
  if (list?.capabilities?.project_pages_v2 !== true) return "board";
  const pages = [...list.items].sort((a, b) => a.position - b.position || a.slug.localeCompare(b.slug));
  const page = pages.find(item => item.builtin === "board") ?? pages[0];
  return page ? `pages/${encodeURIComponent(page.slug)}` : "board";
}

import type { ProjectPageList } from "../../api/types";

/** Initial loads have no response. A same-scope refresh keeps its last authoritative response. */
export function projectPagesEnabled(
  response: { scope: string; value: ProjectPageList } | undefined,
  currentScope: string,
  error: unknown,
): boolean {
  return !error && response?.scope === currentScope && response.value.capabilities?.project_pages_v2 === true;
}

import type { ProjectPageList } from "../../api/types";

/** Never use resource.latest for this check. Scope is captured by the fetch. */
export function projectPagesEnabled(
  response: { scope: string; value: ProjectPageList } | undefined,
  currentScope: string,
  loading: boolean,
  error: unknown,
): boolean {
  return !loading && !error && response?.scope === currentScope && response.value.capabilities?.project_pages_v2 === true;
}

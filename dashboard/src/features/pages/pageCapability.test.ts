import { expect, it } from "vitest";
import { projectPagesEnabled } from "./pageCapability";
import type { ProjectPageList } from "../../api/types";

it("requires explicit true on the current instance/project response", () => {
  const value: ProjectPageList = { items: [], can_edit: true, capabilities: { project_pages_v2: true } };
  const response = { scope: "local/project", value };
  expect(projectPagesEnabled(response, "local/project", false, undefined)).toBe(true);
  expect(projectPagesEnabled(response, "remote:other/project", false, undefined)).toBe(false);
  expect(projectPagesEnabled(response, "local/other", false, undefined)).toBe(false);
  expect(projectPagesEnabled(response, "local/project", true, undefined)).toBe(false);
  expect(projectPagesEnabled(response, "local/project", false, new Error("failed"))).toBe(false);
  expect(projectPagesEnabled(undefined, "local/project", false, undefined)).toBe(false);
  expect(projectPagesEnabled({ scope: response.scope, value: { items: [], can_edit: true } }, response.scope, false, undefined)).toBe(false);
  for (const flag of [false, "true", 1, null]) {
    expect(projectPagesEnabled({ scope: response.scope, value: { ...value, capabilities: { project_pages_v2: flag as boolean } } }, response.scope, false, undefined)).toBe(false);
  }
});

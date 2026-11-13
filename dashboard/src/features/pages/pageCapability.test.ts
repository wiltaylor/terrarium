import { expect, it } from "vitest";
import { projectPagesEnabled } from "./pageCapability";
import type { ProjectPageList } from "../../api/types";

it("requires explicit true on the current instance/project response", () => {
  const value: ProjectPageList = { items: [], can_edit: true, capabilities: { project_pages_v2: true } };
  const response = { scope: "local/project", value };
  expect(projectPagesEnabled(response, "local/project", undefined)).toBe(true);
  expect(projectPagesEnabled(response, "remote:other/project", undefined)).toBe(false);
  expect(projectPagesEnabled(response, "local/other", undefined)).toBe(false);
  expect(projectPagesEnabled(response, "local/project", new Error("failed"))).toBe(false);
  expect(projectPagesEnabled(undefined, "local/project", undefined)).toBe(false);
  expect(projectPagesEnabled({ scope: response.scope, value: { items: [], can_edit: true } }, response.scope, undefined)).toBe(false);
  for (const flag of [false, "true", 1, null]) {
    expect(projectPagesEnabled({ scope: response.scope, value: { ...value, capabilities: { project_pages_v2: flag as boolean } } }, response.scope, undefined)).toBe(false);
  }
});

it("retains only an authoritative same-scope response during a pending refresh", () => {
  const response = { scope: "local/project", value: { items: [], can_edit: true, capabilities: { project_pages_v2: true } } };
  expect(projectPagesEnabled(response, "local/project", undefined)).toBe(true);
  expect(projectPagesEnabled({ scope: response.scope, value: { ...response.value, capabilities: { project_pages_v2: false } } }, response.scope, undefined)).toBe(false);
  expect(projectPagesEnabled(undefined, "local/project", undefined)).toBe(false);
});

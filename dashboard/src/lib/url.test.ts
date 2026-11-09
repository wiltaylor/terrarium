import { describe, expect, it } from "vitest";
import { decodePath, decodePathParam, escapeMalformedPath, projectBase } from "./url";
import boardPage from "../features/board/BoardPage.tsx?raw";
import dbRecordsPage from "../features/db/DbRecordsPage.tsx?raw";
import projectSettings from "../features/projects/ProjectSettings.tsx?raw";
import repoPage from "../features/repos/RepoPage.tsx?raw";
import testRunPage from "../features/tests/TestRunPage.tsx?raw";

describe("project route URLs", () => {
  it("decodes the title value and encodes the project base once", () => {
    expect(decodePathParam("my%20app")).toBe("my app");
    expect(projectBase("my%20app")).toBe("/p/my%20app");
    expect(projectBase("my%20app")).not.toContain("my%2520app");
  });

  it.each([
    ["BoardPage", boardPage],
    ["DbRecordsPage", dbRecordsPage],
    ["ProjectSettings", projectSettings],
    ["RepoPage", repoPage],
    ["TestRunPage", testRunPage],
  ])("%s uses the single-encoding helper for my app", (_name, source) => {
    expect(source).toContain("projectBase(params.project)");
    expect(source).not.toContain("encodeURIComponent(params.project)");
    expect(projectBase("my%20app")).toBe("/p/my%20app");
  });

  it("falls back to raw values with bad percent-encoding", () => {
    expect(decodePathParam("a%zz")).toBe("a%zz");
    expect(decodePath("/p/a%zz/board")).toBe("/p/a%zz/board");
    expect(projectBase("a%zz")).toBe("/p/a%25zz");
    expect(escapeMalformedPath("/p/a%zz/board")).toBe("/p/a%25zz/board");
    expect(escapeMalformedPath("/p/my%20app/board")).toBe("/p/my%20app/board");
  });
});

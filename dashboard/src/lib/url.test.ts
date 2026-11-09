import { describe, expect, it } from "vitest";
import { decodePath, decodePathParam, projectBase } from "./url";

describe("project route URLs", () => {
  it("decodes the title value and encodes the project base once", () => {
    expect(decodePathParam("my%20app")).toBe("my app");
    expect(projectBase("my%20app")).toBe("/p/my%20app");
    expect(projectBase("my%20app")).not.toContain("my%2520app");
  });

  it("falls back to raw values with bad percent-encoding", () => {
    expect(decodePathParam("a%zz")).toBe("a%zz");
    expect(decodePath("/p/a%zz/board")).toBe("/p/a%zz/board");
    expect(projectBase("a%zz")).toBe("/p/a%25zz");
  });
});

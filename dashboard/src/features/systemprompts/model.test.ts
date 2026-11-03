import { describe, expect, it } from "vitest";
import { overLimitNote } from "./model";

describe("overLimitNote", () => {
  it("names the limit for an entry over it, and nothing otherwise", () => {
    expect(overLimitNote({ over_limit: true })).toContain("TERRARIUM_SYSTEM_PROMPT_MAX");
    expect(overLimitNote({ over_limit: false })).toBeNull();
    // An older daemon sends no flag.
    expect(overLimitNote({})).toBeNull();
  });
});

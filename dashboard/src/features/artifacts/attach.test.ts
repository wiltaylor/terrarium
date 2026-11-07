import { describe, expect, it } from "vitest";
import { attachTarget } from "./attach";

describe("artifact attach target", () => {
  it("reads page, card, and label links, and a version as its label", () => {
    expect(attachTarget(" card:TER-12 ")).toEqual({ kind: "card", ref: "TER-12" });
    expect(attachTarget("page:reports/phase-1")).toEqual({ kind: "page", ref: "reports/phase-1" });
    expect(attachTarget("label:version:1.4.0")).toEqual({ kind: "label", ref: "version:1.4.0" });
    expect(attachTarget("version:vnext")).toEqual({ kind: "label", ref: "version:vnext" });
    expect(attachTarget("")).toBeUndefined();
    expect(attachTarget("card:")).toBeUndefined();
    expect(attachTarget("memory:MEM-1")).toBeUndefined();
  });
});

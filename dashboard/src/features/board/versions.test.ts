import { describe, expect, it } from "vitest";
import type { Version } from "../../api/types";
import { seed } from "../../api/mock/data";
import { matchesVersion, NO_VERSION, parseVersionFilter, VERSION_CHIP, versionOptions, versionQuery, versionsByCard } from "./versions";

const v = (id: string, name: string, cards: string[], over: Partial<Version> = {}): Version => ({
  id,
  project: "p",
  name,
  status: "draft",
  notes: "",
  created_by: "operator",
  created_at: "2026-10-28T10:00:00.000Z",
  cards: cards.map((c) => ({ id: c, title: c, column: "todo" })),
  artifacts: [],
  reviews: [],
  ...over,
});

const list = [v("V2", "v0.2.0", ["P-1", "P-3"], { status: "in_review" }), v("V1", "v0.1.0", ["P-1", "P-2", "P-2"], { status: "released" })];

describe("card versions", () => {
  it("maps each card to every version that has it, in the order of the list, once per version", () => {
    const by = versionsByCard(list);
    expect(by.get("P-1")?.map((x) => x.id)).toEqual(["V2", "V1"]);
    expect(by.get("P-2")?.map((x) => x.id)).toEqual(["V1"]);
    expect(by.get("P-3")?.map((x) => x.id)).toEqual(["V2"]);
    expect(by.has("P-4")).toBe(false);
  });

  it("filters by membership: any version, one version, or no version", () => {
    const by = versionsByCard(list);
    const pass = (f: string) => ["P-1", "P-2", "P-3", "P-4"].filter((c) => matchesVersion(c, f, by));
    expect(pass("")).toEqual(["P-1", "P-2", "P-3", "P-4"]);
    expect(pass("V1")).toEqual(["P-1", "P-2"]);
    expect(pass("V2")).toEqual(["P-1", "P-3"]);
    expect(pass(NO_VERSION)).toEqual(["P-4"]);
  });

  it("reads the filter from the `version` query: an ID, a name, or `none`; unknown or not loaded is any version", () => {
    expect(parseVersionFilter(undefined, list)).toBe("");
    expect(parseVersionFilter("", list)).toBe("");
    expect(parseVersionFilter("V1", list)).toBe("V1");
    expect(parseVersionFilter("v0.2.0", list)).toBe("V2");
    expect(parseVersionFilter("none", list)).toBe(NO_VERSION);
    expect(parseVersionFilter("V9", list)).toBe("");
    // Before the versions load, the board shows every card.
    expect(parseVersionFilter("V1", undefined)).toBe("");
    expect(parseVersionFilter("none", undefined)).toBe("");
    // Any version removes the key from the URL.
    expect(versionQuery("")).toBeUndefined();
    expect(versionQuery("V1")).toBe("V1");
    expect(versionQuery(NO_VERSION)).toBe("none");
  });

  it("offers any version, each version with its status, then no version", () => {
    expect(versionOptions(list)).toEqual([
      { value: "", label: "Any version" },
      { value: "V2", label: "v0.2.0 (in review)" },
      { value: "V1", label: "v0.1.0 (released)" },
      { value: "none", label: "No version" },
    ]);
    expect(versionOptions([]).map((o) => o.value)).toEqual(["", "none"]);
  });

  it("gives each status a chip colour", () => {
    expect(VERSION_CHIP).toEqual({ draft: "", in_review: "warn", approved: "ok", rejected: "bad", released: "info" });
  });

  it("works on the mock: a card of the mock board is in a version, a card in two versions has two chips, and some cards are in none", () => {
    const w = seed(Date.parse("2026-10-31T12:00:00Z"));
    const versions = w.versions.filter((x) => x.project === "terrarium");
    const by = versionsByCard(versions);
    const cards = w.cards.filter((c) => c.project === "terrarium" && !c.archived);
    expect(cards.some((c) => matchesVersion(c.id, versions[0]!.id, by))).toBe(true);
    expect(cards.some((c) => (by.get(c.id)?.length ?? 0) >= 2)).toBe(true);
    expect(cards.some((c) => matchesVersion(c.id, NO_VERSION, by))).toBe(true);
  });
});

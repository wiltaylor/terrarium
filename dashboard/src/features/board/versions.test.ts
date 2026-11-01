import { describe, expect, it } from "vitest";
import type { Version } from "../../api/types";
import { seed } from "../../api/mock/data";
import { VERSION_CHIP, versionsByCard } from "./versions";

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

  it("gives each status a chip colour", () => {
    expect(VERSION_CHIP).toEqual({ draft: "", in_review: "warn", approved: "ok", rejected: "bad", released: "info" });
  });

  it("works on the mock: a card of the mock board is in a version, a card in two versions has two chips, and some cards are in none", () => {
    const w = seed(Date.parse("2026-10-31T12:00:00Z"));
    const versions = w.versions.filter((x) => x.project === "terrarium");
    const by = versionsByCard(versions);
    const cards = w.cards.filter((c) => c.project === "terrarium" && !c.archived);
    expect(cards.some((c) => by.get(c.id)?.some((x) => x.id === versions[0]!.id))).toBe(true);
    expect(cards.some((c) => (by.get(c.id)?.length ?? 0) >= 2)).toBe(true);
    expect(cards.some((c) => !by.has(c.id))).toBe(true);
  });
});

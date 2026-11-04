import { describe, expect, it } from "vitest";
import type { Card } from "../../api/types";
import { editCardForm, editCardPatch } from "./editCard";

const card = (over: Partial<Card> = {}): Card => ({
  id: "P-3",
  project: "p",
  column: "ready",
  position: 0,
  title: "Fix it",
  body: "Some text",
  labels: ["bug", "ui"],
  depends_on: ["P-1", "P-2"],
  blocked_by: ["P-1"],
  comments: [],
  links: [],
  created_by: "operator",
  created_at: "t",
  updated_at: "t",
  archived: false,
  ...over,
});

describe("card edit form", () => {
  it("starts with the card's fields; the lists are comma-separated", () => {
    expect(editCardForm(card())).toEqual({ title: "Fix it", body: "Some text", labels: "bug, ui", dependsOn: "P-1, P-2" });
  });

  it("sends nothing when nothing changed", () => {
    expect(editCardPatch(card(), editCardForm(card()))).toEqual({});
    // Spaces and a trailing comma give the same list.
    expect(editCardPatch(card(), { ...editCardForm(card()), dependsOn: " P-1 ,P-2, " })).toEqual({});
  });

  it("sends only the fields that changed", () => {
    expect(editCardPatch(card(), { ...editCardForm(card()), title: "Fix it now", labels: "bug" })).toEqual({ title: "Fix it now", labels: ["bug"] });
    expect(editCardPatch(card(), { ...editCardForm(card()), dependsOn: "P-2, P-4" })).toEqual({ depends_on: ["P-2", "P-4"] });
  });

  it("sends an empty list when the user clears the dependencies or the labels (issue #13)", () => {
    expect(editCardPatch(card(), { ...editCardForm(card()), dependsOn: "" })).toEqual({ depends_on: [] });
    expect(editCardPatch(card(), { ...editCardForm(card()), labels: " , " })).toEqual({ labels: [] });
  });

  it("sends no empty list for a card that has none", () => {
    const c = card({ labels: [], depends_on: [] });
    expect(editCardPatch(c, editCardForm(c))).toEqual({});
  });
});

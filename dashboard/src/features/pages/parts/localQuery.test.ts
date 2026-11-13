import { expect, it } from "vitest";
import { patchQuery, widgetSearch, widgetView } from "./localQuery";

it("merges a query patch and removes undefined and empty values", () => {
  const start: { card?: string; col?: string; version?: string } = { card: "TER-1", col: "review" };
  expect(patchQuery(start, { col: undefined, version: "1.0.0" })).toEqual({ card: "TER-1", version: "1.0.0" });
  expect(patchQuery(start, { card: "" })).toEqual({ col: "review" });
  expect(start).toEqual({ card: "TER-1", col: "review" });
});

it("resolves the saved view, the first view when absent, and nothing for an unknown view", () => {
  expect(widgetView("board", undefined)).toBe("lanes");
  expect(widgetView("board", { view: "columns" })).toBe("columns");
  expect(widgetView("agents", { view: "card_agents" })).toBe("card_agents");
  expect(widgetView("tests", { view: "lanes" })).toBeUndefined();
});

it("trims the saved search", () => {
  expect(widgetSearch({ search: "  unit " })).toBe("unit");
  expect(widgetSearch({ search: "   " })).toBeUndefined();
  expect(widgetSearch(undefined)).toBeUndefined();
});

import { describe, expect, it } from "vitest";
import { emptyNewCardForm, newCardBody, newCardColumn, nextNewCardForm } from "./newCard";

const NOW = Date.parse("2026-11-01T12:00:00Z");

describe("new card column", () => {
  it("gives backlog when the board has it, also when it is not the first column", () => {
    expect(newCardColumn(["backlog", "ready", "done"])).toBe("backlog");
    expect(newCardColumn(["inbox", "backlog", "done"])).toBe("backlog");
  });

  it("gives the first column for a board with no backlog column, and undefined for no columns", () => {
    expect(newCardColumn(["todo", "doing", "done"])).toBe("todo");
    expect(newCardColumn([])).toBeUndefined();
  });
});

describe("new card body", () => {
  it("sends only the title and the column for a form with only a title", () => {
    expect(newCardBody({ ...emptyNewCardForm("backlog"), title: "  Fix the login  " }, NOW)).toEqual({ body: { title: "Fix the login", column: "backlog" } });
  });

  it("sends each field that has a value", () => {
    const f = { title: "T", body: "Some **text**", labels: "bug, ui,\nurgent", assignee: "operator", column: "ready", dependsOn: "TER-1, TER-2", eta: "2h30m" };
    expect(newCardBody(f, NOW)).toEqual({
      body: { title: "T", column: "ready", body: "Some **text**", labels: ["bug", "ui", "urgent"], assignee: "operator", depends_on: ["TER-1", "TER-2"], eta_at: "2h30m" },
    });
  });

  it("refuses an empty title", () => {
    expect(newCardBody({ ...emptyNewCardForm("backlog"), title: "   " }, NOW)).toEqual({ errors: { title: "A card needs a title." } });
  });

  it("refuses an ETA that the daemon refuses", () => {
    const r = newCardBody({ ...emptyNewCardForm("backlog"), title: "T", eta: "soon" }, NOW);
    expect("errors" in r && r.errors.eta).toMatch(/bad ETA/);
    const far = newCardBody({ ...emptyNewCardForm("backlog"), title: "T", eta: "40d" }, NOW);
    expect("errors" in far && far.errors.eta).toMatch(/30 days/);
  });
});

describe("next new card form", () => {
  it("empties the text fields and keeps the column, the assignee, and the labels", () => {
    const f = { title: "T", body: "B", labels: "bug", assignee: "a1", column: "ready", dependsOn: "TER-1", eta: "1h" };
    expect(nextNewCardForm(f)).toEqual({ title: "", body: "", labels: "bug", assignee: "a1", column: "ready", dependsOn: "", eta: "" });
  });
});

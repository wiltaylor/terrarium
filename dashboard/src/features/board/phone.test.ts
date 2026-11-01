import { describe, expect, it } from "vitest";
import { activeFilterCount, columnFromQuery, columnQuery, nearestColumn } from "./phone";

const cols = ["backlog", "ready", "in_progress", "done"];

describe("phone board column", () => {
  it("gives the column of the query when the board has it", () => {
    expect(columnFromQuery("in_progress", cols)).toBe("in_progress");
  });

  it("gives the first column for no query, an empty query, or a column that is not on the board", () => {
    expect(columnFromQuery(undefined, cols)).toBe("backlog");
    expect(columnFromQuery("", cols)).toBe("backlog");
    expect(columnFromQuery("archived", cols)).toBe("backlog");
  });

  it("gives undefined for a board with no columns", () => {
    expect(columnFromQuery("ready", [])).toBeUndefined();
  });

  it("keeps the first column out of the query", () => {
    expect(columnQuery("backlog", cols)).toBeUndefined();
    expect(columnQuery("done", cols)).toBe("done");
  });

  it("finds the column whose left edge is nearest to the scroll position", () => {
    const at = [
      { name: "backlog", left: 0 },
      { name: "ready", left: 370 },
      { name: "done", left: 740 },
    ];
    expect(nearestColumn(0, at)).toBe("backlog");
    expect(nearestColumn(180, at)).toBe("backlog");
    expect(nearestColumn(190, at)).toBe("ready");
    expect(nearestColumn(9999, at)).toBe("done");
    expect(nearestColumn(10, [])).toBeUndefined();
  });
});

describe("filter count", () => {
  it("counts each filter that is not any", () => {
    expect(activeFilterCount({ label: "", assignee: "" })).toBe(0);
    expect(activeFilterCount({ label: "api", assignee: "" })).toBe(1);
    expect(activeFilterCount({ label: "api", assignee: "lead" })).toBe(2);
  });
});

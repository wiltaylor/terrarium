// Home, Work in progress (TER-43): grouping, labels, status tones, the
// assignee, the ETA text, and the summary.
import { describe, expect, it } from "vitest";
import type { ActiveCard } from "../../api/types";
import { assigneeText, byProject, columnLabel, etaText, statusTone, summary } from "./active";

const card = (over: Partial<ActiveCard>): ActiveCard =>
  ({
    id: "TER-1",
    project: "terrarium",
    column: "in_progress",
    column_kind: "working",
    position: 0,
    title: "T",
    body: "",
    labels: [],
    depends_on: [],
    blocked_by: [],
    comments: [],
    links: [],
    created_by: "operator",
    created_by_name: "operator",
    created_at: "",
    updated_at: "",
    archived: false,
    ...over,
  }) as ActiveCard;

describe("work in progress", () => {
  it("groups by project in name order and keeps the card order", () => {
    const g = byProject([
      card({ id: "TER-2", project: "terrarium" }),
      card({ id: "VM-1", project: "vmlab" }),
      card({ id: "TER-3", project: "terrarium", column: "research", column_kind: "research" }),
      card({ id: "AB-9", project: "abc" }),
    ]);
    expect(g.map((x) => x.project)).toEqual(["abc", "terrarium", "vmlab"]);
    expect(g[1]!.cards.map((c) => c.id)).toEqual(["TER-2", "TER-3"]);
    expect(byProject([])).toEqual([]);
  });

  it("names the columns, the status tones, and the assignee", () => {
    expect(columnLabel("working")).toBe("in progress");
    expect(columnLabel("research")).toBe("research");
    expect(statusTone("working")).toBe("ok");
    expect(statusTone("waiting")).toBe("info");
    expect(statusTone("blocked")).toBe("bad");
    expect(statusTone("idle")).toBe("");
    expect(statusTone(undefined)).toBe("");
    expect(assigneeText({ assignee: "A1", assignee_name: "terrarium/rust-ci" })).toBe("terrarium/rust-ci");
    expect(assigneeText({ assignee: "operator", assignee_name: "operator" })).toBe("operator");
    expect(assigneeText({})).toBe("unassigned");
  });

  it("gives the ETA time left or overdue, and none without an ETA", () => {
    const now = Date.parse("2026-11-05T10:00:00Z");
    expect(etaText(card({ eta_at: "2026-11-05T11:12:00Z" }), now)).toEqual({ label: "1h 12m left", overdue: false });
    expect(etaText(card({ eta_at: "2026-11-05T09:48:00Z" }), now)).toEqual({ label: "overdue 12m", overdue: true });
    expect(etaText(card({}), now)).toBeNull();
  });

  it("sums up the columns", () => {
    expect(summary([card({}), card({ id: "TER-2" })])).toBe("2 in progress");
    expect(summary([card({}), card({ id: "R-1", column_kind: "research" })])).toBe("1 in progress, 1 in research");
    expect(summary([])).toBe("0 in progress");
  });
});

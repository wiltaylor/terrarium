import { describe, expect, it } from "vitest";
import { idleBadge } from "./idle";

const NOW = Date.parse("2026-11-06T05:00:00Z");

describe("idleBadge", () => {
  it("shows the idle time of a marked card", () => {
    const b = idleBadge({ idle_since: "2026-11-06T04:35:00.000Z", column: "in_progress", archived: false }, NOW);
    expect(b?.label).toBe("idle 25m");
    expect(b?.title).toContain("leads were told");
    expect(idleBadge({ idle_since: "2026-11-06T02:50:00.000Z", column: "research", archived: false }, NOW)?.label).toBe("idle 2h 10m");
  });

  it("shows nothing without a mark, or for a done or archived card", () => {
    expect(idleBadge({ idle_since: null, column: "in_progress", archived: false }, NOW)).toBeNull();
    expect(idleBadge({ column: "in_progress", archived: false }, NOW)).toBeNull();
    expect(idleBadge({ idle_since: "2026-11-06T04:35:00.000Z", column: "done", archived: false }, NOW)).toBeNull();
    expect(idleBadge({ idle_since: "2026-11-06T04:35:00.000Z", column: "in_progress", archived: true }, NOW)).toBeNull();
    expect(idleBadge({ idle_since: "soon", column: "in_progress", archived: false }, NOW)).toBeNull();
  });
});

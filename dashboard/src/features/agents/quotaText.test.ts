import { describe, expect, it } from "vitest";
import type { UsageMe, UsageTotals } from "../../api/types";
import { quotaLine, shortWindow } from "./quotaText";

const NOW = Date.parse("2026-11-05T10:00:00Z");
const totals = (over: Partial<UsageTotals> = {}): UsageTotals => ({ input_tokens: 0, output_tokens: 0, cache_read: 0, cache_write: 0, messages: 0, turns: 0, ...over }) as UsageTotals;
const me = (over: Partial<UsageMe> = {}): UsageMe => ({
  agent: { id: "A1", name: "w", project: "p" },
  subscription: {
    name: "claude-ultra",
    display_name: "Claude Ultra",
    provider: "anthropic",
    windows: [
      { window: "7d", used_pct: 18, at: "2026-11-05T09:59:00Z" },
      { window: "5h", used_pct: 42.4, resets_at: "2026-11-05T12:10:00Z", at: "2026-11-05T09:59:00Z" },
    ],
  } as UsageMe["subscription"],
  today: { since: "2026-11-05T00:00:00Z", mine: totals({ input_tokens: 100_000, output_tokens: 20_000, turns: 34 }), share_pct: 3 },
  holds: [],
  ...over,
});

describe("the quota line of the agent page (issue #22)", () => {
  it("shows the plan, the windows in order with the reset, and today", () => {
    expect(quotaLine(me(), "claude-ultra", NOW)).toBe("Claude Ultra · 5h 42% (resets in 2h 10m) · 7d 18% · today 120.0k tokens, 34 turns (3% of the subscription)");
  });
  it("shows the plan name only with no usage numbers", () => {
    expect(quotaLine(null, "claude-ultra", NOW)).toBe("claude-ultra");
    expect(quotaLine(undefined, "claude-ultra", NOW)).toBe("claude-ultra");
    const empty = me({ today: { since: "2026-11-05T00:00:00Z", mine: totals() } });
    empty.subscription!.windows = [];
    expect(quotaLine(empty, "claude-ultra", NOW)).toBe("Claude Ultra");
    expect(quotaLine(me({ subscription: undefined, today: { since: "x", mine: totals() } }), "claude-ultra", NOW)).toBe("claude-ultra");
  });
  it("leaves out a missing share, and says one turn", () => {
    const m = me({ today: { since: "x", mine: totals({ output_tokens: 500, turns: 1 }) } });
    m.subscription!.windows = [];
    expect(quotaLine(m, "s", NOW)).toBe("Claude Ultra · today 500 tokens, 1 turn");
  });
  it("names a local estimate", () => {
    const m = me();
    m.subscription!.local_estimate_only = true;
    expect(quotaLine(m, "s", NOW).endsWith(" · local estimate")).toBe(true);
  });
  it("keeps window codes short and long names readable", () => {
    expect(shortWindow("5h")).toBe("5h");
    expect(shortWindow("7d_opus")).toBe("7d opus");
    expect(shortWindow("monthly")).toBe("Monthly");
    expect(shortWindow("extra")).toBe("Extra usage");
  });
});

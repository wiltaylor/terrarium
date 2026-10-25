import { describe, expect, it } from "vitest";
import type { HarnessReport, HarnessVersion } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { harnessBadge, offPin, outOfRange, policyNote } from "./harnesses";

const row = (over: Partial<HarnessVersion>): HarnessVersion => ({
  harness: "claude",
  package: "@anthropic-ai/claude-code",
  pinned: "2.1.310",
  tested: ">=2.1.310, <2.2.0",
  installed: "2.1.310",
  in_range: true,
  checked_at: "2026-10-25T10:00:00Z",
  ...over,
});

describe("harness versions table", () => {
  it("badges a row in range, out of range, or unknown", () => {
    expect(harnessBadge(row({}))).toMatchObject({ cls: "ok", text: "in range" });
    const out = harnessBadge(row({ installed: "2.3.0", in_range: false }));
    expect(out).toMatchObject({ cls: "bad", text: "out of range" });
    expect(out.title).toContain("just harness::update claude");
    expect(harnessBadge(row({ installed: null, in_range: null, error: "cannot run claude" }))).toEqual({ cls: "warn", text: "unknown", title: "cannot run claude" });
  });

  it("counts the rows out of range and spots a version off the pin", () => {
    const r: HarnessReport = { policy: "warn", harnesses: [row({}), row({ harness: "pi", installed: "0.86.0", in_range: false }), row({ harness: "codex", installed: null, in_range: null })] };
    expect(outOfRange(r).map((h) => h.harness)).toEqual(["pi"]);
    expect(outOfRange(undefined)).toEqual([]);
    expect(offPin(row({ installed: "2.1.311" }))).toBe(true);
    expect(offPin(row({}))).toBe(false);
    expect(offPin(row({ installed: null }))).toBe(false);
  });

  it("names the policy", () => {
    expect(policyNote({ policy: "warn", harnesses: [] })).toContain("warning event");
    expect(policyNote({ policy: "refuse", harnesses: [] })).toContain("refused");
  });

  it("the mock answers GET /api/harnesses in the daemon's shape", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.harnesses();
    expect(r.policy).toBe("warn");
    expect(r.harnesses.map((h) => h.harness)).toEqual(["claude", "codex", "copilot", "pi"]);
    for (const h of r.harnesses) {
      expect(Object.keys(h).sort()).toEqual(["checked_at", "harness", "in_range", "installed", "package", "pinned", "tested"]);
    }
    expect(outOfRange(r).map((h) => h.harness)).toEqual(["pi"]);
  });
});

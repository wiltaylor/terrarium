import { describe, expect, it } from "vitest";
import type { EffectiveRule, RuleFinding } from "../../api/types";
import { changeText, eventsText, findingText, flowText, offerMigration, ruleRows, runTone, stepsText, toggleText } from "./model";

const rule = (over: Partial<EffectiveRule>): EffectiveRule => ({
  name: "merge-pass",
  enabled: true,
  on: ["job.verdict"],
  when: [{ path: "data.verdict", op: "eq", value: "passed" }],
  actions: [{ verb: "card.label" }, { verb: "card.comment" }],
  line: 1,
  origin: "override",
  wcl: 'rule "merge-pass" {}\n',
  ...over,
});

const missing: RuleFinding = { code: "missing_column", column: "@accepted", rules: ["merge-pass"], message: "the board has no column `@accepted`: each run of `merge-pass` fails" };

describe("rules model", () => {
  it("makes one row for each rule, in order, with its origin, events, and verbs", () => {
    const rows = ruleRows([rule({}), rule({ name: "agent-park", origin: "pack", on: ["card"], when: [], actions: [], enabled: false, description: "parks" })]);
    expect(rows.map((r) => r.name)).toEqual(["merge-pass", "agent-park"]);
    expect(rows[0]).toMatchObject({ originLabel: "pack, changed in rules.wcl", events: "job.verdict (1 when)", verbs: "card.label, card.comment", enabled: true });
    expect(rows[1]).toMatchObject({ originLabel: "pack", events: "card", verbs: "-", enabled: false, description: "parks" });
    expect(eventsText({ on: [] })).toBe("-");
    expect(ruleRows([rule({ origin: "own" })])[0]!.originLabel).toBe("rules.wcl");
  });

  it("writes the notice of a switch, with the lint findings as a warning", () => {
    expect(toggleText("job-cancelled", { enabled: false, changed: true, findings: [] })).toBe("job-cancelled is off: rules.wcl changed.");
    expect(toggleText("agent-park", { enabled: true, changed: false })).toBe("agent-park was on already.");
    expect(toggleText("merge-pass", { enabled: true, changed: true, findings: [missing] })).toBe(
      "merge-pass is on: rules.wcl changed. Lint: Missing column @accepted: the board has no column `@accepted`: each run of `merge-pass` fails.",
    );
  });

  it("says who runs the flow", () => {
    expect(flowText("rules")).toMatch(/built-in pack runs the card flow/);
    expect(flowText("code")).toMatch(/daemon runs the card flow/);
  });

  it("offers the migration on the coded flow, or when the pack names a missing column", () => {
    expect(offerMigration("code", [])).toBe(true);
    expect(offerMigration("rules", [])).toBe(false);
    expect(offerMigration("rules", [missing])).toBe(true);
    expect(offerMigration("rules", [{ ...missing, code: "stuck_column", column: "validating" }])).toBe(false);
  });

  it("writes a finding, a change, a run state, and the steps of a run", () => {
    expect(findingText(missing)).toBe("Missing column @accepted: the board has no column `@accepted`: each run of `merge-pass` fails");
    expect(findingText({ ...missing, code: "stuck_column", column: "validating", message: "x" })).toBe("Stuck column validating: x");
    expect(changeText({ rule: "merge-pass", change: "mapped", reasons: ["a", "b"] })).toBe("mapped merge-pass: a; b");
    expect(runTone("failed")).toBe("bad");
    expect(runTone("noop")).toBe("ok");
    expect(runTone("skipped")).toBe("warn");
    expect(stepsText({ steps: [{ verb: "card.label", ok: true, changed: true }, { verb: "agent.park", ok: true, changed: false }, { verb: "card.move", ok: false, error: "column `@accepted` is not on the board" }], reason: null })).toBe(
      "card.label ok, agent.park no change, card.move failed: column `@accepted` is not on the board",
    );
    expect(stepsText({ steps: [], reason: "rate limit" })).toBe("rate limit");
    expect(stepsText({ steps: [] })).toBe("-");
  });
});

import { describe, expect, it } from "vitest";
import type { TriggerSpec } from "../../api/types";
import { buildTriggerSpec, ciFailurePreset, emptyTriggerFields, isTriggerAction, ruleFromRow, rowFromRule, splitList, triggerFieldsFromSpec, type TriggerFields } from "./trigger";

const fields = (init: Partial<TriggerFields> = {}): TriggerFields => ({ ...emptyTriggerFields(), kinds: "ci.run.failed", ...init });

describe("the kinds", () => {
  it("split by commas and white space, with no empty items", () => {
    expect(splitList(" ci.run.failed,ci.job.* \n pr.*, ")).toEqual(["ci.run.failed", "ci.job.*", "pr.*"]);
    expect(buildTriggerSpec(fields({ kinds: "ci.run.failed, ci.job.*" }))).toEqual({ value: { kind: "on", kinds: ["ci.run.failed", "ci.job.*"] } });
  });

  it("need one kind or more", () => {
    expect(buildTriggerSpec(fields({ kinds: " , " }))).toEqual({ error: "Name one or more event kinds, for example ci.run.failed.", field: "kinds" });
  });
});

describe("the match rules", () => {
  it("map each op: eq, ne, and glob take one value; in takes the comma list", () => {
    const b = buildTriggerSpec(
      fields({
        rules: [
          { path: "data.branch", op: "eq", value: " main " },
          { path: "actor", op: "ne", value: "source:_/nightly" },
          { path: "data.workflow", op: "glob", value: "ci*.yml" },
          { path: "data.repo", op: "in", value: "wil/terrarium, wil/site ,," },
        ],
      }),
    );
    expect(b).toEqual({
      value: {
        kind: "on",
        kinds: ["ci.run.failed"],
        match: [
          { path: "data.branch", op: "eq", value: "main" },
          { path: "actor", op: "ne", value: "source:_/nightly" },
          { path: "data.workflow", op: "glob", value: "ci*.yml" },
          { path: "data.repo", op: "in", value: ["wil/terrarium", "wil/site"] },
        ],
      },
    });
  });

  it("drop the empty rows, and leave `match` out when none is left", () => {
    expect(buildTriggerSpec(fields({ rules: [{ path: " ", op: "eq", value: "" }] }))).toEqual({ value: { kind: "on", kinds: ["ci.run.failed"] } });
  });

  it("refuse a bad path, an empty in list, and an empty glob, with the rule number", () => {
    expect(buildTriggerSpec(fields({ rules: [{ path: "data.branch", op: "eq", value: "main" }, { path: "branch", op: "eq", value: "x" }] }))).toEqual({ error: "Rule 2: the path is project, kind, actor, summary, or data.<path>.", field: "rules" });
    expect(buildTriggerSpec(fields({ rules: [{ path: "data.", op: "eq", value: "x" }] }))).toMatchObject({ field: "rules" });
    expect(buildTriggerSpec(fields({ rules: [{ path: "data.repo", op: "in", value: " , " }] }))).toEqual({ error: 'Rule 1: "in" needs one or more values, split by commas.', field: "rules" });
    expect(buildTriggerSpec(fields({ rules: [{ path: "kind", op: "glob", value: "" }] }))).toEqual({ error: 'Rule 1: "glob" needs a pattern.', field: "rules" });
  });

  it("read back from a rule, a number or a list as text", () => {
    expect(rowFromRule({ path: "data.attempt", value: 2 })).toEqual({ path: "data.attempt", op: "eq", value: "2" });
    expect(rowFromRule({ path: "data.repo", op: "in", value: ["a", "b"] })).toEqual({ path: "data.repo", op: "in", value: "a, b" });
    expect(ruleFromRow(rowFromRule({ path: "data.repo", op: "in", value: ["a", "b"] }))).toEqual({ path: "data.repo", op: "in", value: ["a", "b"] });
  });
});

describe("dedupe and escalate are optional", () => {
  it("stay out of the spec when they are off, even with text in their fields", () => {
    const b = buildTriggerSpec(fields({ dedupe: false, dedupeKey: "x", escalate: false, escalateKey: "y" }));
    expect(b).toEqual({ value: { kind: "on", kinds: ["ci.run.failed"] } });
  });

  it("dedupe: the key and a window from 1s to 7d", () => {
    expect(buildTriggerSpec(fields({ dedupe: true, dedupeKey: " {{event.data.repo}} ", dedupeWindow: "10m" }))).toMatchObject({ value: { dedupe: { key: "{{event.data.repo}}", window: "10m" } } });
    expect(buildTriggerSpec(fields({ dedupe: true, dedupeKey: " " }))).toMatchObject({ field: "dedupeKey" });
    expect(buildTriggerSpec(fields({ dedupe: true, dedupeWindow: "8d" }))).toMatchObject({ field: "dedupeWindow" });
    expect(buildTriggerSpec(fields({ dedupe: true, dedupeWindow: "soon" }))).toMatchObject({ field: "dedupeWindow" });
  });

  it("escalate: key and after; reset_on and to only when given", () => {
    const on = { escalate: true, escalateKey: "{{event.data.repo}}", escalateAfter: "3" };
    expect(buildTriggerSpec(fields({ ...on, escalateResetOn: "", escalateTo: "" }))).toMatchObject({ value: { escalate: { key: "{{event.data.repo}}", after: 3 } } });
    const full = buildTriggerSpec(fields({ ...on, escalateResetOn: "ci.run.succeeded, ci.run.cancelled", escalateTo: "terrarium/lead" }));
    expect(full).toMatchObject({ value: { escalate: { key: "{{event.data.repo}}", after: 3, reset_on: ["ci.run.succeeded", "ci.run.cancelled"], to: "terrarium/lead" } } });
    const plain = buildTriggerSpec(fields({ ...on, escalateResetOn: "", escalateTo: "" }));
    expect("value" in plain && plain.value.escalate).not.toHaveProperty("to");
    expect("value" in plain && plain.value.escalate).not.toHaveProperty("reset_on");
    expect(buildTriggerSpec(fields({ ...on, escalateAfter: "0" }))).toMatchObject({ field: "escalateAfter" });
    expect(buildTriggerSpec(fields({ ...on, escalateAfter: "2.5" }))).toMatchObject({ field: "escalateAfter" });
    expect(buildTriggerSpec(fields({ ...on, escalateKey: "" }))).toMatchObject({ field: "escalateKey" });
  });

  it("max_per_hour: empty is the daemon's default; else 1 to 600", () => {
    expect(buildTriggerSpec(fields({ maxPerHour: "" }))).toEqual({ value: { kind: "on", kinds: ["ci.run.failed"] } });
    expect(buildTriggerSpec(fields({ maxPerHour: "12" }))).toMatchObject({ value: { max_per_hour: 12 } });
    expect(buildTriggerSpec(fields({ maxPerHour: "601" }))).toMatchObject({ field: "maxPerHour" });
    expect(buildTriggerSpec(fields({ maxPerHour: "0" }))).toMatchObject({ field: "maxPerHour" });
  });
});

describe("Edit: the fields of a spec", () => {
  it("come back to the same spec", () => {
    const spec: TriggerSpec = {
      kind: "on",
      kinds: ["ci.run.failed", "ci.job.*"],
      match: [
        { path: "data.branch", op: "glob", value: "main" },
        { path: "data.repo", op: "in", value: ["wil/terrarium", "wil/site"] },
      ],
      dedupe: { key: "{{event.data.repo}}:{{event.data.branch}}", window: "10m" },
      escalate: { key: "{{event.data.repo}}:{{event.data.branch}}", after: 3, reset_on: ["ci.run.succeeded"], to: "operator" },
      max_per_hour: 30,
    };
    const f = triggerFieldsFromSpec(spec);
    expect(f).toMatchObject({ kinds: "ci.run.failed, ci.job.*", dedupe: true, escalate: true, escalateAfter: "3", escalateTo: "operator", maxPerHour: "30" });
    expect(buildTriggerSpec(f)).toEqual({ value: spec });
  });

  it("leave dedupe and escalate off when the spec has none", () => {
    expect(triggerFieldsFromSpec({ kind: "on", kinds: ["git.push"] })).toMatchObject({ kinds: "git.push", rules: [], dedupe: false, escalate: false, maxPerHour: "" });
  });
});

describe("the actions of a trigger", () => {
  it("are mail, exec, and card only", () => {
    expect(["mail", "turn", "exec", "agent", "probe", "card"].filter((t) => isTriggerAction(t as never))).toEqual(["mail", "exec", "card"]);
  });
});

describe("the preset CI failure → mail an agent", () => {
  it("fills the api.md example: ci.run.failed on main, dedupe and escalate per repo, a mail with templates", () => {
    const p = ciFailurePreset({ name: "", mailTo: "" }, "terrarium/lead");
    expect(p).toMatchObject({ name: "ci-failed", kinds: "ci.run.failed", actionType: "mail", mailTo: "terrarium/lead", mailSubject: "CI failed: {{event.data.repo}}" });
    expect(buildTriggerSpec({ ...emptyTriggerFields(), ...p })).toEqual({
      value: {
        kind: "on",
        kinds: ["ci.run.failed"],
        match: [{ path: "data.branch", op: "eq", value: "main" }],
        dedupe: { key: "{{event.data.repo}}", window: "10m" },
        escalate: { key: "{{event.data.repo}}", after: 3, reset_on: ["ci.run.succeeded"] },
      },
    });
  });

  it("keeps the name and the recipient that the form has", () => {
    const p = ciFailurePreset({ name: "mine", mailTo: "terrarium/w1" }, "terrarium/lead");
    expect(p).not.toHaveProperty("name");
    expect(p).not.toHaveProperty("mailTo");
  });
});

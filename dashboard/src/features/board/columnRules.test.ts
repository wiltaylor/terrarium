import { describe, expect, it } from "vitest";
import { ApiError } from "../../api/http";
import type { ColumnOptions, ColumnRule } from "../../api/types";
import { cleanRule, isStale, rateLimits, rateText, problemsByPath, problemsUnder, putBody, queueText, ruleSentence, seconds, triggerText } from "./columnRules";

const read: ColumnOptions = {
  project: "p", column: "merge_queue", version: 3, can_edit: true,
  checks: { require_comment_on_enter: false, require_dependencies_met: false, terminal_for_members: false, clear_eta_on_enter: true },
  queue: { release: "batch", batch_size: 4, max_wait_seconds: 1800, max_batch_cards: 8 },
  rules: [],
};

describe("column rules in plain words (TER-1116)", () => {
  it("says each trigger as the start of a sentence", () => {
    expect(triggerText({ kind: "lands" })).toBe("When a card lands in this column (after its queue)");
    expect(triggerText({ kind: "job_result", events: ["verdict", "ended"], job_kinds: ["merge"] })).toBe("When the merge job of a card here has a verdict or ends");
    expect(triggerText({ kind: "timer", every_seconds: 3600 })).toBe("Every 1 hour while a card is here");
    expect(triggerText({ kind: "count_reached", count: 4 })).toBe("When this column holds 4 cards");
    expect(seconds(1800)).toBe("30 minutes");
  });

  it("says a whole rule with its conditions and actions", () => {
    const r: ColumnRule = {
      name: "merge-pass", trigger: { kind: "job_result", events: ["verdict"], job_kinds: ["merge"] },
      if_card: [{ lacks_labels: ["hold"] }], if_job: [{ outcome: ["passed"] }], max_per_hour: 60,
      actions: [{ verb: "card.move", args: { to: "@accepted" } }, { verb: "card.label", args: { add: ["merged"] } }],
    };
    expect(ruleSentence(r)).toBe("When the merge job of a card here has a verdict, if the card: it has none of the labels hold and the job outcome is passed: move the card to the accepted column and then add the label merged. At most 60 runs an hour, 60 for one card.");
  });

  it("says the queue", () => {
    expect(queueText(read.queue)).toBe("Cards are released in batches of 4, or when the oldest has waited 30 minutes, at most 8 cards in a batch.");
    expect(queueText({ release: "wip" })).toBe("Cards enter when the WIP limit has room.");
  });
});

describe("the PUT of the cog", () => {
  it("sends the read version and drops the batch numbers of another release", () => {
    const b = putBody(read, { checks: read.checks, queue: { ...read.queue, release: "wip" }, rules: [] });
    expect(b).toEqual({ base_version: 3, checks: read.checks, queue: { release: "wip", batch_size: null, max_wait_seconds: null, max_batch_cards: null }, rules: [] });
  });

  it("drops empty conditions, empty arguments and job conditions of another trigger", () => {
    const r = cleanRule({ id: "r1", name: " x ", description: " ", trigger: { kind: "lands" }, if_card: [{}], if_job: [{ split: true }], actions: [{ verb: "card.label", args: { add: [], remove: ["a"], x: "" } }] });
    expect(r).toEqual({ id: "r1", name: "x", enabled: true, trigger: { kind: "lands" }, actions: [{ verb: "card.label", args: { remove: ["a"] } }] });
  });

  it("finds a stale base and the field of each 422 problem", () => {
    expect(isStale(new ApiError(409, "stale", undefined, "stale_base", { version: 4 }))).toBe(true);
    expect(isStale(new ApiError(409, "wip", undefined, "wip_limit"))).toBe(false);
    const e = new ApiError(422, "rules[1].actions[0].args.to: unknown column", undefined, "invalid", {
      path: "rules[1].actions[0].args.to",
      problems: [{ path: "rules[1].actions[0].args.to", message: "unknown column" }, { path: "rules[0].name", message: "bad name" }],
    });
    const p = problemsByPath(e);
    expect(p).toEqual({ "rules[1].actions[0].args.to": "unknown column", "rules[0].name": "bad name" });
    expect(problemsUnder(p, "rules[1]")).toEqual(["rules[1].actions[0].args.to: unknown column"]);
    expect(problemsByPath(new ApiError(422, "queue.batch_size: needed", undefined, "invalid", { path: "queue.batch_size" }))).toEqual({ "queue.batch_size": "queue.batch_size: needed" });
    expect(problemsByPath(new Error("x"))).toEqual({});
  });
});

describe("the loop guards of a rule (C2a D4)", () => {
  it("shows the defaults of 30 an hour and 10 for one card, marked as defaults", () => {
    expect(rateLimits({})).toEqual({ perRule: 30, perCard: 10, defaults: true });
    expect(rateText({})).toBe("At most 30 runs an hour, 10 for one card (the defaults).");
  });
  it("raises the card cap to an explicit max_per_hour, up to the ceiling of 600", () => {
    expect(rateText({ max_per_hour: 120 })).toBe("At most 120 runs an hour, 120 for one card.");
    expect(rateText({ max_per_hour: 5 })).toBe("At most 5 runs an hour, 5 for one card.");
    expect(rateLimits({ max_per_hour: 900 })).toEqual({ perRule: 600, perCard: 600, defaults: false });
  });
});

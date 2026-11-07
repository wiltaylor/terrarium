import { describe, expect, it } from "vitest";
import type { CardJob } from "../api/types";
import { cardState, tileLabels } from "./cardState";

const card = (over: { column?: string; archived?: boolean; job?: CardJob; assignee?: string; assignee_status?: string } = {}) => ({
  column: "in_progress",
  archived: false,
  assignee: "01AGENT",
  ...over,
});

const job = (over: Partial<CardJob>): CardJob => ({ id: "01JOB", kind: "validate", state: "queued", ...over });

describe("cardState", () => {
  it("shows the assignee status: working, waiting, idle", () => {
    expect(cardState(card({ assignee_status: "working" }))?.label).toBe("working");
    expect(cardState(card({ assignee_status: "waiting" }))?.label).toBe("waiting");
    expect(cardState(card({ assignee_status: "idle" }))?.label).toBe("idle");
    expect(cardState(card({ assignee_status: "idle" }))?.kind).toBe("idle");
  });

  it("prefers the live agent status to the card's stored one", () => {
    expect(cardState(card({ assignee_status: "idle" }), "working")?.label).toBe("working");
  });

  it("shows another agent status as itself", () => {
    const s = cardState(card({ assignee_status: "blocked" }));
    expect(s?.kind).toBe("agent");
    expect(s?.label).toBe("blocked");
    expect(s?.tone).toBe("bad");
  });

  it("shows the queue place of a queued validation: queued n of m", () => {
    const s = cardState(card({ column: "validating", assignee_status: "idle", job: job({ place: 2, queued: 5 }) }));
    expect(s?.kind).toBe("queued");
    expect(s?.label).toBe("queued 2 of 5");
    expect(s?.title).toContain("number 2 of 5");
    expect(cardState(card({ job: job({ place: 1, queued: 1 }) }))?.title).toContain("starts next");
    expect(cardState(card({ job: job({}) }))?.label).toBe("queued");
  });

  it("shows a running job as running, for a validation and a merge", () => {
    expect(cardState(card({ column: "validating", assignee_status: "working", job: job({ state: "running" }) }))?.label).toBe("running");
    const m = cardState(card({ column: "review", job: job({ kind: "merge", state: "running" }) }));
    expect(m?.label).toBe("running");
    expect(m?.title).toContain("merge");
  });

  it("shows a queued merge as merge queued, not awaiting review", () => {
    const s = cardState(card({ column: "review", assignee: undefined, job: job({ kind: "merge", place: 1, queued: 2 }) }));
    expect(s?.kind).toBe("merge_queued");
    expect(s?.label).toBe("merge queued");
  });

  it("shows awaiting review for a card in review with no job", () => {
    expect(cardState(card({ column: "review", assignee: undefined }))?.label).toBe("awaiting review");
    expect(cardState(card({ column: "review", assignee_status: "working" }))?.label).toBe("awaiting review");
  });

  it("shows nothing for a done, accepted, or archived card, or with no job and no agent", () => {
    expect(cardState(card({ column: "done", assignee_status: "working" }))).toBeNull();
    expect(cardState(card({ column: "accepted", job: job({}) }))).toBeNull();
    expect(cardState(card({ archived: true, assignee_status: "working" }))).toBeNull();
    expect(cardState(card({ assignee: undefined }))).toBeNull();
    expect(cardState(card({ assignee: "operator" }))).toBeNull();
    expect(cardState(card({}))).toBeNull();
  });

  it("does not read labels", () => {
    // A stale label and no job: the chip follows the agent, not the label.
    const c = { ...card({ assignee_status: "idle" }), labels: ["validation:running", "validation:queued"] };
    expect(cardState(c)?.label).toBe("idle");
  });
});

describe("tileLabels", () => {
  it("hides the validation labels and keeps the others", () => {
    expect(tileLabels(["feature", "validation:queued", "validation:running", "by:ter-1", "validation:failed"])).toEqual(["feature", "by:ter-1"]);
    expect(tileLabels([])).toEqual([]);
  });
});

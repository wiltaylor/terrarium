import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentEvent } from "../../api/types";
import { createDebounce, DONE_RELOAD_MS, eventCards, eventTouchesCard, RELOAD_MAX_WAIT_MS } from "./liveReload";

const ev = (kind: AgentEvent["kind"], data: Record<string, unknown>, card?: string): AgentEvent =>
  ({ seq: 1, id: "e", at: "t", kind, project: "p", summary: "", data, ...(card ? { card } : {}) }) as AgentEvent;

describe("eventTouchesCard", () => {
  it("matches card events by card and data.card_id, without case", () => {
    expect(eventTouchesCard(ev("card", { card_id: "TER-5" }), "ter-5")).toBe(true);
    expect(eventTouchesCard(ev("card", {}, "ter-5"), "TER-5")).toBe(true);
    expect(eventTouchesCard(ev("card", { card_id: "TER-50" }), "TER-5")).toBe(false);
  });
  it("matches job events by data.card and the batch data.cards", () => {
    expect(eventTouchesCard(ev("job", { card: "TER-7" }), "TER-7")).toBe(true);
    expect(eventTouchesCard(ev("job", { card: "TER-1", cards: ["TER-6", "ter-7"] }), "TER-7")).toBe(true);
    expect(eventTouchesCard(ev("job", { card: "TER-1" }), "TER-7")).toBe(false);
  });
  it("matches test_run events by data.cards and skips values that are not strings", () => {
    expect(eventTouchesCard(ev("test_run", { cards: ["TER-9"] }), "ter-9")).toBe(true);
    expect(eventCards(ev("test_run", { cards: [1, null, "TER-9"], card: 3 }))).toEqual(["TER-9"]);
    expect(eventTouchesCard(ev("test_run", { cards: "TER-9" }), "TER-9")).toBe(false);
  });
});

describe("createDebounce", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("runs once, 2000 ms after the last event of a short burst", () => {
    const fn = vi.fn();
    const d = createDebounce(fn, RELOAD_MAX_WAIT_MS);
    d.schedule(DONE_RELOAD_MS);
    vi.advanceTimersByTime(1500);
    d.schedule(DONE_RELOAD_MS);
    vi.advanceTimersByTime(1999);
    expect(fn).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("runs at the max wait while events keep coming, then starts a new burst", () => {
    const fn = vi.fn();
    const d = createDebounce(fn, RELOAD_MAX_WAIT_MS);
    // An event every second: the trailing timer alone would never fire.
    for (let t = 0; t < RELOAD_MAX_WAIT_MS; t += 1000) {
      d.schedule(DONE_RELOAD_MS);
      vi.advanceTimersByTime(t + 1000 === RELOAD_MAX_WAIT_MS ? 999 : 1000);
    }
    expect(fn).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(fn).toHaveBeenCalledTimes(1);
    d.schedule(DONE_RELOAD_MS);
    vi.advanceTimersByTime(DONE_RELOAD_MS);
    expect(fn).toHaveBeenCalledTimes(2);
  });

  it("does not run after cancel", () => {
    const fn = vi.fn();
    const d = createDebounce(fn, RELOAD_MAX_WAIT_MS);
    d.schedule(DONE_RELOAD_MS);
    d.cancel();
    vi.advanceTimersByTime(RELOAD_MAX_WAIT_MS);
    expect(fn).not.toHaveBeenCalled();
  });
});

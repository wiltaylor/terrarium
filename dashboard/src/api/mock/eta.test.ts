import { describe, expect, it } from "vitest";
import { ApiError } from "../http";
import { createMockApi } from ".";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0 });

// The card ETA of docs/api.md §8.2, as store/board.rs set_eta has it.
describe("mock card ETA", () => {
  it("seeds ETAs: minutes left, hours left, overdue, and done", async () => {
    const b = await mockBoard();
    const left = (id: string) => Date.parse(b.find((c) => c.id === id)!.eta_at!) - Date.now();
    expect(left("TER-12")).toBeLessThan(3_600_000);
    expect(left("TER-16")).toBeGreaterThan(3_600_000);
    expect(left("TER-15")).toBeLessThan(0);
    const done = b.find((c) => c.id === "TER-11")!;
    expect(done.column).toBe("done");
    expect(done.done_at).toBeTruthy();
  });

  it("sets, validates, audits, and clears an ETA", async () => {
    const api = mock();
    const before = Date.now();
    const c = await api.cards.update("TER-13", { eta_at: "2h30m" });
    expect(Date.parse(c.eta_at!) - before).toBeGreaterThanOrEqual(150 * 60_000);
    expect(c.eta_set_by_name).toBe("operator");
    for (const bad of ["31d", "2000-01-01T00:00:00Z", "soon"]) {
      const e = await api.cards.update("TER-13", { eta_at: bad }).catch((x: unknown) => x);
      expect((e as ApiError).status, bad).toBe(422);
    }
    // TER-12 is runner-dev's card: the operator's change is audited.
    await api.cards.update("TER-12", { eta_at: "1h" });
    const audit = await api.audit({ action: "card.eta" });
    expect(audit.items.map((a) => a.target)).toContain("TER-12");
    const ev = await api.cards.events("TER-13");
    expect(ev.items.some((e) => e.data.change === "eta")).toBe(true);
    const cleared = await api.cards.update("TER-13", { eta_at: null });
    expect(cleared.eta_at).toBeUndefined();
  });

  it("keeps the ETA on done, sets done_at, and refuses a new ETA there", async () => {
    const api = mock();
    await api.cards.update("TER-16", { eta_at: "1h" });
    const d = await api.cards.update("TER-16", { column: "done" });
    expect(d.eta_at).toBeTruthy();
    expect(d.done_at).toBeTruthy();
    const e = await api.cards.update("TER-16", { eta_at: "2h" }).catch((x: unknown) => x);
    expect((e as ApiError).status).toBe(409);
    const r = await api.cards.update("TER-16", { column: "review" });
    expect(r.done_at).toBeUndefined();
  });
});

async function mockBoard() {
  const api = mock();
  return [...(await api.board.get("terrarium")).cards, ...(await api.board.get("terrarium", { done: true })).cards];
}

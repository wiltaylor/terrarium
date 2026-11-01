// The test runs of the mock answer in the daemon's shapes (docs/api.md, test runs).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent, TestFormat } from "../types";
import { ApiError } from "../http";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });

describe("mock test runs", () => {
  it("has a run in every format, newest first, with the summary fields of the contract", async () => {
    const api = mock();
    const page = await api.testRuns.list("terrarium");
    const formats = new Set(page.items.map((r) => r.format));
    for (const f of ["junit", "trx", "ctrf", "libtest-json", "vitest-json", "jest-json", "tap"] as TestFormat[]) expect(formats).toContain(f);
    const times = page.items.map((r) => Date.parse(r.created_at));
    expect([...times].sort((a, b) => b - a)).toEqual(times);
    const r = page.items[0]!;
    for (const k of ["id", "project", "name", "format", "tool", "status", "counts", "duration_ms", "started_at", "commit", "branch", "file_name", "raw_size", "notes", "cards", "versions", "created_by", "created_by_name", "created_at", "url"]) expect(r).toHaveProperty(k);
    expect(r.url).toBe(`/p/terrarium/board/tests/${r.id}`);
    expect(r.counts.total).toBe(r.counts.passed + r.counts.failed + r.counts.errored + r.counts.skipped);
    expect(page.items.some((x) => x.status === "passed")).toBe(true);
    expect(page.items.some((x) => x.status === "failed")).toBe(true);
  });

  it("filters by card, version (name or ID), name, and status, and pages with a cursor", async () => {
    const api = mock();
    const byCard = (await api.testRuns.list("terrarium", { card: "TER-12" })).items;
    expect(byCard.map((r) => r.name)).toEqual(["unit", "unit"]);
    const v = (await api.versions.list("terrarium")).find((x) => x.name === "v0.1.0")!;
    const byName = (await api.testRuns.list("terrarium", { version: "v0.1.0" })).items.map((r) => r.id);
    expect((await api.testRuns.list("terrarium", { version: v.id })).items.map((r) => r.id)).toEqual(byName);
    expect(byName).toHaveLength(2);
    expect((await api.testRuns.list("terrarium", { status: "failed" })).items.every((r) => r.status === "failed")).toBe(true);
    expect((await api.testRuns.list("terrarium", { name: "smoke" })).items).toHaveLength(1);
    const p1 = await api.testRuns.list("terrarium", { limit: 3 });
    expect(p1.items).toHaveLength(3);
    const p2 = await api.testRuns.list("terrarium", { limit: 3, cursor: p1.next! });
    expect(p2.items.map((r) => r.id)).not.toContain(p1.items[2]!.id);
  });

  it("gives the card chip of the newest linked run on the board", async () => {
    const api = mock();
    const board = await api.board.get("terrarium");
    const card = board.cards.find((c) => c.id === "TER-12")!;
    const newest = (await api.testRuns.list("terrarium", { card: "TER-12" })).items[0]!;
    expect(card.tests).toEqual({ id: newest.id, name: "unit", status: newest.status, counts: newest.counts, created_at: newest.created_at });
    expect(board.cards.find((c) => c.id === "TER-17")!.tests).toBeUndefined();
  });

  it("filters and pages the cases of a run: failed means failed or errored", async () => {
    const api = mock();
    const jest = (await api.testRuns.list("terrarium", { name: "frontend-jest" })).items[0]!;
    const failed = await api.testRuns.get(jest.id, { status: "failed" });
    expect(failed.cases.map((c) => c.status).sort()).toEqual(["errored", "failed"]);
    expect(failed.cases_total).toBe(2);
    expect(failed.suites.length).toBeGreaterThan(0);
    const unit = (await api.testRuns.list("terrarium", { name: "unit" })).items[0]!;
    const one = await api.testRuns.get(unit.id, { limit: 3 });
    expect(one.cases).toHaveLength(3);
    expect(one.next_cursor).toBe("3");
    const two = await api.testRuns.get(unit.id, { limit: 3, cursor: one.next_cursor! });
    expect(two.cases[0]!.seq).toBe(4);
    expect((await api.testRuns.get(unit.id, { q: "SCREEN::" })).cases.every((c) => c.suite.includes("screen"))).toBe(true);
  });

  it("compares with the previous run of the same name", async () => {
    const api = mock();
    const [newer, older] = (await api.testRuns.list("terrarium", { name: "unit" })).items;
    const c = await api.testRuns.compare(newer!.id);
    expect(c.base?.id).toBe(older!.id);
    expect(c.newly_failing.map((x) => x.name)).toEqual(["resize_clamps_to_min"]);
    expect(c.fixed.map((x) => x.name)).toEqual(["resize_sends_sigwinch"]);
    expect(c.still_failing.map((x) => x.name)).toEqual(["child_exit_closes_reader"]);
    expect(c.new_tests.map((x) => x.name)).toEqual(["resize_debounces_bursts"]);
    expect(c.removed_tests.map((x) => x.name)).toEqual(["legacy_prompt_regex"]);
    const none = await api.testRuns.compare(older!.id);
    expect(none.base).toBeNull();
    expect(none.counts.newly_failing).toBe(0);
  });

  it("links and unlinks cards and versions with the edit rules, and emits test_run events", async () => {
    const api = mock();
    const seen: AgentEvent[] = [];
    const stream = api.events.stream({ since: api.world.seq });
    stream.subscribe((e) => seen.push(e));
    await new Promise((r) => setTimeout(r, 5));
    const smoke = (await api.testRuns.list("terrarium", { name: "smoke" })).items[0]!;
    const linked = await api.testRuns.links(smoke.id, { add_cards: ["TER-13"], add_versions: ["v0.1.1"] });
    expect(linked.cards).toContain("TER-13");
    expect(linked.versions.map((v) => v.name)).toEqual(["v0.1.1"]);
    const unlinked = await api.testRuns.links(smoke.id, { remove_cards: ["TER-13"] });
    expect(unlinked.cards).not.toContain("TER-13");
    // v0.1.0 is in review: its links do not change.
    await expect(api.testRuns.links(smoke.id, { add_versions: ["v0.1.0"] })).rejects.toMatchObject({ status: 409 });
    await expect(api.testRuns.links(smoke.id, { add_cards: ["WEB-1"] })).rejects.toBeInstanceOf(ApiError);
    const runs = seen.filter((e) => e.kind === "test_run");
    expect(runs.map((e) => e.data.change)).toEqual(["linked", "unlinked"]);
    expect(runs[0]!.data).toMatchObject({ project: "terrarium", id: smoke.id, name: "smoke", versions: ["v0.1.1"] });
    stream.close();
  });

  it("gives the original file and deletes a run", async () => {
    const api = mock();
    const smoke = (await api.testRuns.list("terrarium", { name: "smoke" })).items[0]!;
    const raw = await api.testRuns.raw(smoke.id);
    expect(raw.fileName).toBe("smoke.tap");
    expect(await raw.blob.text()).toMatch(/^TAP version 14/);
    await api.testRuns.remove(smoke.id);
    await expect(api.testRuns.get(smoke.id)).rejects.toMatchObject({ status: 404 });
  });
});

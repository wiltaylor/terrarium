import { describe, expect, it } from "vitest";
import { buildQueue } from "./queue";
import type { InboxItem, Schedule } from "../../api/types";

const sched = (name: string, project: string | null, status: "ok" | "failed", at: string) =>
  ({ id: name, name, project, last_run: { at, status, summary: "exit 1", run_id: "r" } }) as Schedule;

describe("buildQueue: failed schedule runs", () => {
  it("adds a schedule item for each failed last run, after cards, newest first", () => {
    const board = { project: "p", card_prefix: "P", columns: [], cards: [{ id: "P-1", column: "blocked", updated_at: "2026-10-26T08:00:00Z" }] } as never;
    const q = buildQueue([{ project: "p", versions: [], pages: [], board }], [], [sched("a", "p", "failed", "2026-10-26T09:00:00Z"), sched("g", null, "failed", "2026-10-26T10:00:00Z"), sched("ok", "p", "ok", "2026-10-26T11:00:00Z")]);
    expect(q.map((i) => i.kind)).toEqual(["card", "schedule", "schedule"]);
    expect(q.filter((i) => i.kind === "schedule").map((i) => [i.project, i.at])).toEqual([
      ["", "2026-10-26T10:00:00Z"],
      ["p", "2026-10-26T09:00:00Z"],
    ]);
  });

  it("keeps the old call without schedules", () => {
    expect(buildQueue([])).toEqual([]);
  });
});

describe("buildQueue: the Inbox", () => {
  const item = (id: string, over: Partial<InboxItem> = {}): InboxItem => ({ id, kind: "question", open: true, urgent: false, project: "p", at: "2026-10-26T07:00:00Z", ...over });

  it("puts the open Inbox items first, in their own order, and leaves the done ones out", () => {
    const board = { project: "p", card_prefix: "P", columns: [], cards: [{ id: "P-1", column: "blocked", updated_at: "2026-10-26T08:00:00Z" }] } as never;
    const inbox = [item("question:u", { urgent: true }), item("mail:m", { kind: "mail", project: null, at: "2026-10-26T09:00:00Z" }), item("notice:d", { kind: "notice", open: false })];
    const q = buildQueue([{ project: "p", versions: [], pages: [], board }], inbox);
    expect(q.map((i) => (i.kind === "inbox" ? i.item.id : i.kind))).toEqual(["question:u", "mail:m", "card"]);
    expect(q[1]!.project).toBe("");
  });
});

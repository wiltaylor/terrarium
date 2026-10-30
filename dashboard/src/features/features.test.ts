import { describe, expect, it } from "vitest";
import { fakeUlid, seed } from "../api/mock/data";
import { createMockApi } from "../api/mock";
import { buildQueue } from "./home/queue";
import { buildTree } from "./wiki/tree";

const world = seed(Date.parse("2026-10-24T12:00:00Z"));

describe("home queue", () => {
  it("collects the open Inbox items first, then versions in review, pages for review, and blocked cards", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const q = buildQueue(
      world.projects.map((p) => ({
        project: p.name,
        versions: world.versions.filter((v) => v.project === p.name),
        pages: world.pages.filter((x) => x.project === p.name),
        board: { project: p.name, columns: [], cards: world.cards.filter((c) => c.project === p.name) },
      })),
      await api.inbox.list(),
    );
    expect(q.map((i) => (i.kind === "inbox" ? `${i.kind}:${i.item.kind}` : i.kind))).toEqual([
      "inbox:question",
      "inbox:mail",
      // The bundle: an Inbox item of the kind `question`.
      "inbox:question",
      "inbox:mail",
      "inbox:notice",
      "inbox:system",
      "inbox:mail",
      "inbox:question",
      "version",
      "page",
      "card",
      "card",
    ]);
    // The urgent question first; then the newest first.
    expect(q[0]!.kind === "inbox" && q[0]!.item.urgent).toBe(true);
    const ids = q.flatMap((i) => (i.kind === "inbox" ? [i.item.id] : []));
    expect(ids).toContain(`mail:${fakeUlid("msg", 3)}`);
    expect(ids).toContain(`thread:${fakeUlid("th", 7)}`);
    // The "needs a human" notice is not a mail item: the thread item stands for it.
    expect(ids).not.toContain(`mail:${fakeUlid("msg", 12)}`);
    expect(q[4]!.project).toBe("terrarium");
  });

  it("leaves out the done Inbox items", () => {
    expect(buildQueue([], [{ id: "notice:x", kind: "notice", open: false, urgent: false, project: null, at: "2026-10-24T12:00:00Z" }])).toEqual([]);
  });
});

describe("wiki tree", () => {
  it("nests by path with index first", () => {
    const tree = buildTree(world.pages.filter((p) => p.project === "terrarium"));
    expect(tree.map((n) => n.path)).toEqual(["index", "designs", "reports", "security"]);
    expect(tree[1]!.children[0]!.page?.title).toBe("Runner v2");
  });
});

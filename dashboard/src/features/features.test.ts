import { describe, expect, it } from "vitest";
import { fakeUlid, seed } from "../api/mock/data";
import { createMockApi } from "../api/mock";
import { buildQueue } from "./home/queue";
import { annotate, needsAttention } from "./mail/log";
import { buildTree } from "./wiki/tree";

const world = seed(Date.parse("2026-10-24T12:00:00Z"));

describe("home queue", () => {
  it("collects versions in review, pages for review, blocked cards, and the mail that needs the operator", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const mail = annotate((await api.mail.list({ limit: 1000 })).items, { prefixes: { TER: "terrarium" } });
    const q = buildQueue(
      world.projects.map((p) => ({
        project: p.name,
        versions: world.versions.filter((v) => v.project === p.name),
        pages: world.pages.filter((x) => x.project === p.name),
        board: { project: p.name, columns: [], cards: world.cards.filter((c) => c.project === p.name) },
      })),
      mail.filter(needsAttention),
    );
    expect(q.map((i) => i.kind)).toEqual(["version", "page", "card", "card", "mail", "mail", "mail", "mail", "mail"]);
    expect(q[0]!.kind === "version" && q[0]!.version.name).toBe("v0.1.0");
    const reasons = Object.fromEntries(q.flatMap((i) => (i.kind === "mail" ? [[i.entry.message.id, i.entry.reasons]] : [])));
    expect(reasons).toEqual({
      [fakeUlid("msg", 3)]: ["unread"],
      [fakeUlid("msg", 11)]: ["held"],
      [fakeUlid("msg", 12)]: ["unread", "needs_human", "paused"],
      [fakeUlid("msg", 13)]: ["unread", "blocked"],
      [fakeUlid("msg", 16)]: ["unread", "quota"],
    });
    // The blocked notice names its project by the card prefix; the quota alert has none.
    expect(q.find((i) => i.kind === "mail" && i.entry.message.id === fakeUlid("msg", 13))!.project).toBe("terrarium");
    expect(q.find((i) => i.kind === "mail" && i.entry.message.id === fakeUlid("msg", 16))!.project).toBe("");
  });

  it("leaves out mail with no reason", () => {
    expect(buildQueue([], annotate(world.mail.filter((m) => m.state === "read")))).toEqual([]);
  });
});

describe("wiki tree", () => {
  it("nests by path with index first", () => {
    const tree = buildTree(world.pages.filter((p) => p.project === "terrarium"));
    expect(tree.map((n) => n.path)).toEqual(["index", "designs", "reports", "security"]);
    expect(tree[1]!.children[0]!.page?.title).toBe("Runner v2");
  });
});

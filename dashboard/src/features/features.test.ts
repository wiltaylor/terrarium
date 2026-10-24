import { describe, expect, it } from "vitest";
import { fakeUlid, seed } from "../api/mock/data";
import { buildQueue } from "./home/queue";
import { groupThreads } from "./mail/threads";
import { buildTree } from "./wiki/tree";

const world = seed(Date.parse("2026-10-24T12:00:00Z"));

describe("home queue", () => {
  it("collects versions in review, pages for review, blocked cards, and unread inbox items", () => {
    const q = buildQueue(
      world.projects.map((p) => ({
        project: p.name,
        versions: world.versions.filter((v) => v.project === p.name),
        pages: world.pages.filter((x) => x.project === p.name),
        board: { project: p.name, columns: [], cards: world.cards.filter((c) => c.project === p.name) },
        inbox: world.mail.filter((m) => m.to === `project:${p.name}`),
      })),
    );
    expect(q.map((i) => i.kind)).toEqual(["version", "page", "card", "card", "inbox", "inbox"]);
    expect(q[0]!.kind === "version" && q[0]!.version.name).toBe("v0.1.0");
  });
});

describe("mail threads", () => {
  it("groups by thread and counts unread", () => {
    const threads = groupThreads(world.mail);
    const th1 = threads.find((t) => t.id === fakeUlid("th", 1))!;
    expect(th1.messages.map((m) => m.id)).toEqual([fakeUlid("msg", 1), fakeUlid("msg", 2)]);
    expect(th1.subject).toBe("TER-15 blocked: Codex adapter: resume by session id");
    expect(th1.participants).toEqual(["project:terrarium", "operator", "terrarium/lead", "terrarium/adapter-codex"]);
    expect(th1.unread).toBe(1);
    expect(threads[0]!.last_at >= threads.at(-1)!.last_at).toBe(true);
  });
});

describe("wiki tree", () => {
  it("nests by path with index first", () => {
    const tree = buildTree(world.pages.filter((p) => p.project === "terrarium"));
    expect(tree.map((n) => n.path)).toEqual(["index", "designs", "reports"]);
    expect(tree[1]!.children[0]!.page?.title).toBe("Runner v2");
  });
});

import { describe, expect, it } from "vitest";
import type { InboxItem } from "../../api/types";
import { badge, countItems, EMPTY_COUNTS, emptyText, inboxQuery, isInboxEvent, matchesTabs, putItem, sortItems } from "./model";

const item = (id: string, over: Partial<InboxItem> = {}): InboxItem => ({
  id,
  kind: "question",
  open: true,
  urgent: false,
  project: "p",
  at: "2026-10-28T10:00:00.000Z",
  ...over,
});

describe("inbox model", () => {
  it("sorts urgent first, then the newest", () => {
    const list = [
      item("old", { at: "2026-10-28T08:00:00.000Z" }),
      item("new", { at: "2026-10-28T12:00:00.000Z" }),
      item("urgent-old", { urgent: true, at: "2026-10-28T07:00:00.000Z" }),
      item("mid", { kind: "mail", at: "2026-10-28T10:00:00.000Z" }),
    ];
    expect(sortItems(list).map((i) => i.id)).toEqual(["urgent-old", "new", "mid", "old"]);
  });

  it("builds the query of the tabs", () => {
    expect(inboxQuery("all", "open", "")).toEqual({ status: "open" });
    expect(inboxQuery("notice", "done", "webshop")).toEqual({ status: "done", kind: "notice", project: "webshop" });
    expect(inboxQuery("system", "open", "")).toEqual({ status: "open", kind: "system" });
  });

  it("matches the tabs: kind, project, open or done; a system item is never done", () => {
    expect(matchesTabs(item("a"), "all", "open")).toBe(true);
    expect(matchesTabs(item("a"), "question", "open")).toBe(true);
    expect(matchesTabs(item("a"), "notice", "open")).toBe(false);
    expect(matchesTabs(item("a"), "all", "done")).toBe(false);
    expect(matchesTabs(item("a", { open: false }), "all", "done")).toBe(true);
    expect(matchesTabs(item("a", { kind: "system", open: false }), "all", "done")).toBe(false);
    expect(matchesTabs(item("a"), "all", "open", "q")).toBe(false);
    expect(matchesTabs(item("a"), "all", "open", "p")).toBe(true);
  });

  it("puts a changed item in place, and drops it when it leaves the tab", () => {
    const list = sortItems([item("a", { at: "2026-10-28T09:00:00.000Z" }), item("b")]);
    const open = (i: InboxItem) => matchesTabs(i, "all", "open");
    expect(putItem(list, item("a", { at: "2026-10-28T09:00:00.000Z", urgent: true }), open).map((i) => i.id)).toEqual(["a", "b"]);
    expect(putItem(list, item("a", { open: false }), open).map((i) => i.id)).toEqual(["b"]);
    expect(putItem(list, item("c", { at: "2026-10-28T11:00:00.000Z" }), open).map((i) => i.id)).toEqual(["c", "b", "a"]);
  });

  it("counts the open items, and makes the badge", () => {
    const c = countItems([
      item("q"),
      item("u", { urgent: true }),
      item("n", { kind: "notice" }),
      item("m", { kind: "mail" }),
      item("s", { kind: "system" }),
      item("done", { open: false, kind: "mail" }),
    ]);
    expect(c).toEqual({ open: 5, urgent: 1, question: 2, notice: 1, mail: 1, system: 1 });
    expect(badge(c)).toEqual({ n: 5, cls: "nav-count", title: "Waiting for you: 2 question, 1 notice, 1 mail, 1 paused thread. 1 urgent" });
    expect(badge({ ...c, urgent: 0 })!.cls).toBe("nav-count info");
    expect(badge(EMPTY_COUNTS)).toBeNull();
  });

  it("follows inbox and question events, and says why a list is empty", () => {
    expect(isInboxEvent({ kind: "inbox" })).toBe(true);
    expect(isInboxEvent({ kind: "question" })).toBe(true);
    expect(isInboxEvent({ kind: "mail" })).toBe(false);
    expect(emptyText("all", "open")).toBe("Nothing waits for you.");
    expect(emptyText("system", "done")).toMatch(/only while it is open/);
  });
});

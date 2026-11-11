import { describe, expect, it } from "vitest";
import type { InboxItem, InboxWatch } from "../../api/types";
import { answerLine, ASK_TEXT_MAX, askBody, badge, canForward, countItems, EMPTY_COUNTS, emptyText, INBOX_TABS, inboxQuery, isCritical, isInboxEvent, matchesTabs, putItem, questionFlags, sortItems, STATUS_TABS, statusTabOf, systemMailLink, watchState, watchTitle } from "./model";

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
  it("keeps only open questions and critical notices in the default Inbox", () => {
    const rows = [
      item("question"), item("normal", { kind: "notice" }),
      item("urgent", { kind: "notice", urgent: true }),
      item("critical", { kind: "notice", urgent: true, critical: true }),
      item("mail", { kind: "mail" }), item("system", { kind: "system" }),
      item("closed", { open: false }),
    ];
    expect(rows.filter((i) => matchesTabs(i, "all", "open")).map((i) => i.id)).toEqual(["question", "critical"]);
    expect(countItems(rows)).toMatchObject({ open: 2, question: 1, notice: 1, critical: 1, urgent: 1 });
  });
  it("sorts urgent first, then the newest", () => {
    const list = [
      item("old", { at: "2026-10-28T08:00:00.000Z" }),
      item("new", { at: "2026-10-28T12:00:00.000Z" }),
      item("urgent-old", { urgent: true, at: "2026-10-28T07:00:00.000Z" }),
      item("mid", { kind: "mail", at: "2026-10-28T10:00:00.000Z" }),
    ];
    expect(sortItems(list).map((i) => i.id)).toEqual(["urgent-old", "new", "mid", "old"]);
  });

  it("sorts critical first, then urgent, then the newest", () => {
    const list = [
      item("urgent", { urgent: true, at: "2026-10-28T12:00:00.000Z" }),
      item("normal", { at: "2026-10-28T13:00:00.000Z" }),
      item("critical-old", { urgent: true, critical: true, at: "2026-10-28T06:00:00.000Z" }),
    ];
    expect(sortItems(list).map((i) => i.id)).toEqual(["critical-old", "urgent", "normal"]);
  });

  it("counts critical items as critical and as urgent; the badge names them", () => {
    const c = countItems([item("c", { urgent: true, critical: true }), item("u", { urgent: true }), item("n"), item("closed", { open: false, critical: true })]);
    expect(c).toMatchObject({ open: 3, urgent: 2, critical: 1 });
    expect(badge(c)!.title).toContain(". 1 critical. 2 urgent");
    expect(badge({ ...c, urgent: 0 })!.cls).toBe("nav-count");
  });

  it("reads critical from the question when an older daemon sends no flag", () => {
    const q = { priority: "critical", status: "open" } as const;
    expect(isCritical(item("a", { question: q as never }))).toBe(true);
    expect(isCritical(item("a", { open: false, question: q as never }))).toBe(false);
    expect(isCritical(item("a", { critical: false, question: q as never }))).toBe(false);
    expect(questionFlags({ priority: "critical", status: "open" })).toEqual({ open: true, urgent: true, critical: true });
    expect(questionFlags({ priority: "urgent", status: "open" })).toEqual({ open: true, urgent: true, critical: false });
    expect(questionFlags({ priority: "critical", status: "acknowledged" })).toEqual({ open: false, urgent: false, critical: false });
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
    expect(c).toEqual({ open: 2, urgent: 1, critical: 0, question: 2, notice: 0, mail: 1, system: 1, watch: 0, thread: 0, thread_urgent: 0, thread_critical: 0 });
    expect(badge(c)).toEqual({ n: 2, cls: "nav-count", title: "Waiting for you: 2 question. 1 urgent" });
    expect(badge({ ...c, urgent: 0 })!.cls).toBe("nav-count info");
    expect(badge(EMPTY_COUNTS)).toBeNull();
  });

  it("leaves the threads out of the Inbox badge: they count for the Threads badge (TER-466)", () => {
    const c = countItems([item("m", { kind: "mail" }), item("t1", { kind: "thread", urgent: true }), item("t2", { kind: "thread", urgent: true, critical: true }), item("t3", { kind: "thread", open: false })]);
    expect(c).toMatchObject({ open: 0, urgent: 0, critical: 0, thread: 2, thread_urgent: 2, thread_critical: 1 });
    expect(badge(c)).toBeNull();
    expect(badge({ ...EMPTY_COUNTS, thread: 3, thread_urgent: 1 })).toBeNull();
  });

  it("follows inbox and question events, and says why a list is empty", () => {
    expect(isInboxEvent({ kind: "inbox" })).toBe(true);
    expect(isInboxEvent({ kind: "question" })).toBe(true);
    expect(isInboxEvent({ kind: "mail" })).toBe(false);
    expect(emptyText("all", "open")).toBe("Nothing waits for you.");
    expect(emptyText("system", "done")).toMatch(/only while it is open/);
  });

  it("links a daemon mail to its dashboard page", () => {
    const quota = { from_kind: "system" as const, body: "Subscription `max` has used 81%.\n\nOpen: http://localhost:8080/subscriptions\n" };
    expect(systemMailLink(quota)).toEqual({ href: "/subscriptions", label: "Subscriptions" });
    expect(systemMailLink({ ...quota, body: "Open: http://localhost:8080/settings/subscriptions\n" })).toEqual({ href: "/settings/subscriptions", label: "Subscriptions" });
    expect(systemMailLink({ ...quota, body: "Open: /subscriptions?add=api_key\n" })).toEqual({ href: "/subscriptions?add=api_key", label: "Subscriptions" });
    expect(systemMailLink({ ...quota, body: "Open: /subscriptions-old\n" })?.label).toBe("Open");
    expect(systemMailLink({ ...quota, body: "Budget.\n\nOpen: /p/web\n" })).toEqual({ href: "/p/web", label: "Project" });
    expect(systemMailLink({ ...quota, from_kind: "agent" })).toBeNull();
    expect(systemMailLink({ ...quota, body: "Open: javascript:alert(1)" })).toBeNull();
    expect(systemMailLink({ ...quota, body: "Open: //evil.example/x" })).toBeNull();
    expect(systemMailLink({ ...quota, body: "no link" })).toBeNull();
  });
});

describe("ask and forward (GitHub #24)", () => {
  const watch = (over: Partial<InboxWatch> = {}): InboxWatch => ({ thread_id: "T1", agent: "A1", agent_name: "demo/w1", message_id: "M1", subject: "Question from the operator: why?", created_at: "2026-11-05T10:00:00Z", waiting: true, replies: 0, ...over });
  const watchItem = (over: Partial<InboxItem> = {}): InboxItem => ({ id: "watch:T1", kind: "watch", open: true, urgent: false, critical: false, project: "demo", at: "2026-11-05T10:00:00Z", watch: watch(), ...over });

  it("has an Asked status tab next to Done that lists only the waiting asks (TER-42)", () => {
    expect(STATUS_TABS.find((t) => t.id === "asked")!.label).toBe("Asked");
    expect(INBOX_TABS.map((t) => t.id as string)).not.toContain("watch");
    // The query: the open watches, whatever the kind tab.
    expect(inboxQuery("all", "asked", "")).toEqual({ status: "open", kind: "watch" });
    expect(inboxQuery("question", "asked", "demo")).toEqual({ status: "open", kind: "watch", project: "demo" });
    const mail = item("mail:M", { kind: "mail" });
    // Asked: a waiting ask, never another item, whatever the kind tab.
    expect(matchesTabs(watchItem(), "all", "asked")).toBe(true);
    expect(matchesTabs(watchItem(), "question", "asked")).toBe(true);
    expect(matchesTabs(watchItem({ open: false }), "all", "asked")).toBe(false);
    expect(matchesTabs(mail, "all", "asked")).toBe(false);
    // Open: never a waiting ask; the reply (a mail) shows there.
    expect(matchesTabs(watchItem(), "all", "open")).toBe(false);
    expect(matchesTabs(mail, "all", "open")).toBe(false);
    // Done: an answered or cancelled ask.
    expect(matchesTabs(watchItem({ open: false }), "all", "done")).toBe(true);
    expect(matchesTabs(watchItem(), "all", "done")).toBe(false);
    expect(emptyText("all", "asked")).toContain("Ask an agent");
  });

  it("has no Threads tab and no Tracking tab: the threads have their own screen (TER-466)", () => {
    expect(INBOX_TABS.map((t) => t.id as string)).not.toContain("thread");
    expect(STATUS_TABS.map((t) => t.id)).toEqual(["open", "done", "asked"]);
    expect(statusTabOf("tracking", "thread")).toBe("open");
    expect(statusTabOf("tracking", undefined)).toBe("open");
  });

  it("opens Asked for an old ?tab=watch link", () => {
    expect(statusTabOf(undefined, undefined)).toBe("open");
    expect(statusTabOf("done", undefined)).toBe("done");
    expect(statusTabOf("asked", undefined)).toBe("asked");
    expect(statusTabOf(undefined, "watch")).toBe("asked");
    expect(statusTabOf("done", "watch")).toBe("done");
    expect(statusTabOf("bogus", "question")).toBe("open");
  });

  it("does not count an open watch in the badge", () => {
    const c = countItems([watchItem(), item("mail:M", { kind: "mail" })]);
    expect(c).toMatchObject({ open: 1, mail: 1, watch: 1 });
    expect(badge(c)!.n).toBe(1);
    expect(badge(countItems([watchItem()]))).toBeNull();
  });

  it("checks the body of an ask and a forward", () => {
    expect(askBody("", "why?")).toEqual({ error: "Pick an agent." });
    expect(askBody("demo/w1", "  ")).toEqual({ error: "Write the question." });
    expect(askBody("demo/w1", " why? ")).toEqual({ body: { to: "demo/w1", text: "why?" } });
    expect(askBody("demo/w1", "", "Q1")).toEqual({ body: { to: "demo/w1", question: "Q1" } });
    expect(askBody("demo/w1", "a note", "Q1")).toEqual({ body: { to: "demo/w1", text: "a note", question: "Q1" } });
    expect("error" in askBody("demo/w1", "x".repeat(ASK_TEXT_MAX + 1))).toBe(true);
  });

  it("names the state of a watch and the answer of a mail", () => {
    expect(watchTitle(watch())).toBe("Waiting for demo/w1: Question from the operator: why?");
    expect(watchTitle(watch({ waiting: false, replies: 1 }))).toBe("Answered by demo/w1: Question from the operator: why?");
    expect(watchTitle(watch({ cancelled_at: "x" }))).toBe("No longer waiting for demo/w1: Question from the operator: why?");
    expect(watchState(watch())).toEqual({ label: "waiting", chip: "info" });
    expect(watchState(watch({ replies: 1 }))).toEqual({ label: "waiting again", chip: "info" });
    expect(watchState(watch({ waiting: false, replies: 1 }))).toEqual({ label: "answered", chip: "ok" });
    expect(watchState(watch({ cancelled_at: "x" })).label).toBe("cancelled");
    expect(answerLine(watch({ question_id: "Q9" }))).toBe("answer to your forward of Q9");
    expect(answerLine(watch())).toBe('answer to your question "Question from the operator: why?"');
    expect(canForward({ status: "open" })).toBe(true);
    expect(canForward({ status: "answered" })).toBe(false);
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InboxItem, Question } from "../../api/types";
import { ALERT_FAVICON, ALERT_TITLE, announce, createTitleFlasher, criticalBanner, inboxItemDomId, inboxItemHref, shortText, type FlashEnv } from "./critical";

const question = (id: string, over: Partial<Question> = {}): Question => ({
  id,
  kind: "notice",
  agent: "A1",
  agent_name: "shop/lead",
  project: "shop",
  text: `text of ${id}`,
  options: [],
  priority: "critical",
  status: "open",
  thread_id: "T1",
  created_at: "2026-10-28T10:00:00.000Z",
  updated_at: "2026-10-28T10:00:00.000Z",
  revision: 1,
  ...over,
});

const item = (id: string, at: string, over: Partial<InboxItem> = {}): InboxItem => ({
  id: `notice:${id}`,
  kind: "notice",
  open: true,
  urgent: true,
  critical: true,
  project: "shop",
  at,
  question: question(id, { created_at: at }),
  ...over,
});

describe("criticalBanner", () => {
  it("hides with no open critical item", () => {
    expect(criticalBanner([])).toBeNull();
    expect(criticalBanner([item("u", "2026-10-28T10:00:00.000Z", { critical: false })])).toBeNull();
  });

  it("shows the newest open critical item, its agent, its short text, and the count of the others", () => {
    const b = criticalBanner([
      item("old", "2026-10-28T08:00:00.000Z"),
      item("new", "2026-10-28T12:00:00.000Z", { question: question("new", { text: "Prod is down.\nDetails follow." }) }),
      item("urgent", "2026-10-28T13:00:00.000Z", { critical: false }),
      item("mid", "2026-10-28T10:00:00.000Z"),
    ])!;
    expect(b.item.id).toBe("notice:new");
    expect(b.more).toBe(2);
    expect(b.from).toBe("shop/lead");
    expect(b.text).toBe("Prod is down.");
    expect(b.href).toBe("/inbox?item=notice%3Anew");
  });

  it("hides after the acknowledgement: a closed item is not critical", () => {
    const open = [item("a", "2026-10-28T08:00:00.000Z")];
    expect(criticalBanner(open)).not.toBeNull();
    expect(criticalBanner([{ ...open[0]!, open: false, critical: false }])).toBeNull();
  });

  it("cuts a long text", () => {
    expect(shortText("x".repeat(300), 10)).toBe(`${"x".repeat(9)}…`);
    expect(shortText("\n\n  first  \nsecond")).toBe("first");
  });

  it("makes a DOM id and a link from an item id", () => {
    expect(inboxItemDomId("notice:01ABC")).toBe("inbox-item-notice-01ABC");
    expect(inboxItemHref("question:Q1")).toBe("/inbox?item=question%3AQ1");
  });

  it("announces an item once, and a new item again", () => {
    let r = announce(new Set(), "notice:a");
    expect(r.announce).toBe(true);
    r = announce(r.seen, "notice:a");
    expect(r.announce).toBe(false);
    r = announce(r.seen, "notice:b");
    expect(r.announce).toBe(true);
    expect(announce(r.seen, null).announce).toBe(false);
  });
});

describe("createTitleFlasher", () => {
  let title: string;
  let icon: string | null;
  let env: FlashEnv;

  beforeEach(() => {
    vi.useFakeTimers();
    title = "Terrarium";
    icon = "/favicon.svg";
    env = {
      getTitle: () => title,
      setTitle: (t) => (title = t),
      getFavicon: () => icon,
      setFavicon: (h) => (icon = h),
      setInterval: (fn, ms) => setInterval(fn, ms),
      clearInterval: (h) => clearInterval(h as ReturnType<typeof setInterval>),
    };
  });
  afterEach(() => vi.useRealTimers());

  it("does nothing while the operator looks at the page, or with no critical item", () => {
    const f = createTitleFlasher(env);
    f.update({ critical: true, attentive: true });
    f.update({ critical: false, attentive: false });
    vi.advanceTimersByTime(5000);
    expect(f.flashing()).toBe(false);
    expect(title).toBe("Terrarium");
    expect(icon).toBe("/favicon.svg");
  });

  it("alternates the title about every second while the page is hidden, with the alert favicon", () => {
    const f = createTitleFlasher(env);
    f.update({ critical: true, attentive: false });
    expect(f.flashing()).toBe(true);
    expect(title).toBe(ALERT_TITLE);
    expect(icon).toBe(ALERT_FAVICON);
    vi.advanceTimersByTime(1000);
    expect(title).toBe("Terrarium");
    vi.advanceTimersByTime(1000);
    expect(title).toBe(ALERT_TITLE);
    // A second update does not start a second timer.
    f.update({ critical: true, attentive: false });
    vi.advanceTimersByTime(1000);
    expect(title).toBe("Terrarium");
    expect(vi.getTimerCount()).toBe(1);
  });

  it("stops and restores the title and the favicon on focus", () => {
    const f = createTitleFlasher(env);
    f.update({ critical: true, attentive: false });
    vi.advanceTimersByTime(3000);
    f.update({ critical: true, attentive: true });
    expect(f.flashing()).toBe(false);
    expect(title).toBe("Terrarium");
    expect(icon).toBe("/favicon.svg");
    expect(vi.getTimerCount()).toBe(0);
  });

  it("stops when the critical item closes, and on dispose", () => {
    const f = createTitleFlasher(env);
    f.update({ critical: true, attentive: false });
    f.update({ critical: false, attentive: false });
    expect(title).toBe("Terrarium");
    f.update({ critical: true, attentive: false });
    f.dispose();
    expect(title).toBe("Terrarium");
    expect(vi.getTimerCount()).toBe(0);
  });

  it("keeps a title that the page set meanwhile", () => {
    const f = createTitleFlasher(env);
    f.update({ critical: true, attentive: false });
    vi.advanceTimersByTime(1000);
    title = "Inbox — Terrarium";
    vi.advanceTimersByTime(1000);
    expect(title).toBe(ALERT_TITLE);
    vi.advanceTimersByTime(1000);
    expect(title).toBe("Inbox — Terrarium");
    f.update({ critical: false, attentive: true });
    expect(title).toBe("Inbox — Terrarium");
  });

  it("works with no favicon link", () => {
    icon = null;
    const f = createTitleFlasher(env);
    f.update({ critical: true, attentive: false });
    expect(icon).toBeNull();
    f.dispose();
    expect(icon).toBeNull();
  });
});

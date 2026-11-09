// @vitest-environment jsdom
// The Inbox page with the mock API (TER-42): a waiting ask shows under the
// Asked status tab and not under Open; when the agent answers, the ask
// leaves Asked, its reply shows on Open, and the ask is under Done.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import { ApiError } from "../../api/http";
import { waitFor } from "../../test/wait";
import type { InboxItem } from "../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", () => ({
  api,
  events: { on: () => () => {} },
  inbox: { refresh: async () => {} },
  projects: { projects: () => [] },
  agents: { state: { agents: [] } },
}));

const { default: InboxPage } = await import("./InboxPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

function mount(url: string) {
  dispose?.();
  document.body.innerHTML = "";
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  history.set({ value: url });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "*", component: InboxPage });
        },
      }),
    el,
  );
  return el;
}

const watchCards = (el: HTMLElement) => el.querySelectorAll("article[aria-label^='Watch ']");

describe("Inbox load (TER-302)", () => {
  for (const scale of [1, 10]) {
    it(`renders ${scale}x live mail counts`, async () => {
      const rows: InboxItem[] = Array.from({ length: 257 * scale }, (_, i) => ({
        id: `mail:perf-${i}`, kind: "mail", open: true, urgent: false, critical: false,
        project: "p", at: "2026-11-08T00:00:00Z",
        message: { id: `perf-${i}`, thread_id: `thread-${i}`, from: "p/lead", from_kind: "agent", from_name: "p/lead",
          to: "operator", to_kind: "operator", to_name: "operator", subject: `Performance ${i}`,
          body: "Representative message content. ".repeat(100), state: "queued", created_at: "2026-11-08T00:00:00Z" },
      }));
      const spy = vi.spyOn(api.inbox, "list").mockImplementation(async (q = {}) => {
        const sorted = [...rows].sort((a, b) => b.at.localeCompare(a.at) || b.id.localeCompare(a.id));
        const previous = q.cursor ? JSON.parse(q.cursor)[4] as string : undefined;
        const start = previous ? sorted.findIndex((i) => i.id === previous) + 1 : 0;
        return sorted.slice(start, start + (q.limit ?? 200));
      });
      try {
        const el = mount("/inbox");
        await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBeGreaterThan(0), 10000);
        // Bound work, not wall time on a shared build host. A 10x regression
        // in mounted cards or fetched pages must fail at either fixture size.
        expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBe(40);
        expect(spy).toHaveBeenCalledTimes(1);
        expect(spy).toHaveBeenLastCalledWith(expect.objectContaining({ limit: 200, cursor: undefined }));
        const more = [...el.querySelectorAll("button")].find((b) => b.textContent?.startsWith("Load more"))!;
        more.click();
        expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBe(80);
        expect(spy).toHaveBeenCalledTimes(1);
        const reply = el.querySelector("textarea")!;
        reply.value = "Keep my draft";
        reply.dispatchEvent(new Event("input", { bubbles: true }));
        [...el.querySelectorAll("button")].find((b) => b.textContent === "Refresh")!.click();
        await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));
        expect(el.querySelector("textarea")).toBe(reply);
        expect(reply.value).toBe("Keep my draft");
        const lastMounted = el.querySelector(".stack > div[id^='inbox-item']:last-child");
        rows.push({ ...rows[0]!, id: "mail:new-top", at: "2026-11-09T00:00:00Z", message: { ...rows[0]!.message!, id: "new-top" } });
        [...el.querySelectorAll("button")].find((b) => b.textContent === "Refresh")!.click();
        await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBe(81));
        expect(el.querySelector("textarea")).not.toBeNull();
        expect(reply.isConnected).toBe(true);
        expect(reply.value).toBe("Keep my draft");
        if (lastMounted) expect(lastMounted.isConnected).toBe(true);
        while (el.querySelectorAll("article[aria-label^='Mail ']").length < 240) {
          const before = el.querySelectorAll("article[aria-label^='Mail ']").length;
          [...el.querySelectorAll("button")].find((b) => b.textContent === "Load more")!.click();
          await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBeGreaterThan(before));
        }
        expect(spy.mock.calls.some(([q]) => !!q?.cursor)).toBe(true);
        // Deep links intentionally bypass the render bound. Check that behavior
        // on the 1x fixture only, not by mounting 2570 rich cards in jsdom.
        if (scale === 1) {
          vi.spyOn(window, "requestAnimationFrame").mockImplementation(() => 0);
          const deep = mount(`/inbox?item=${encodeURIComponent(rows[0]!.id)}`);
          await waitFor(() => expect(deep.querySelectorAll("article[aria-label^='Mail ']").length).toBe(rows.length), 10000);
        }
      } finally {
        spy.mockRestore();
        vi.restoreAllMocks();
      }
    }, 30000);
  }
});

describe("the Asked status tab (TER-42)", () => {
  it("shows a waiting ask only under Asked, and its reply on Open when the agent answers", async () => {
    const r = await api.inbox.ask({ to: "terrarium/lead", text: "Why did the runner fail?" });
    const thread = r.message.thread_id;
    // The status tabs: Open, Done, Asked; no Asked kind tab.
    let el = mount("/inbox?status=asked");
    await waitFor(() => expect(watchCards(el).length).toBe(1));
    expect(el.textContent).toContain("Waiting for terrarium/lead: Question from the operator: Why did the runner fail?");
    const tabs = [...el.querySelectorAll("[role=tab]")].map((t) => t.textContent?.trim());
    expect(tabs).toEqual(["Open", "Done", "Asked"]);
    // Open: not there.
    el = mount("/inbox");
    await waitFor(() => expect(el.textContent).not.toContain("Loading"));
    expect(watchCards(el).length).toBe(0);
    expect(el.textContent).not.toContain("Waiting for terrarium/lead");
    // An old link to the Asked kind tab opens the Asked status tab.
    el = mount("/inbox?tab=watch");
    await waitFor(() => expect(watchCards(el).length).toBe(1));
    // The agent answers.
    await api.mail.send({ from: "terrarium/lead", to: "operator", thread_id: thread, body: "A flaky test." });
    el = mount("/inbox");
    await waitFor(() => expect(el.querySelector("[data-testid=watch-answer]")).not.toBeNull());
    expect(el.textContent).toContain("answer to your question");
    expect(watchCards(el).length).toBe(0);
    el = mount("/inbox?status=asked");
    await waitFor(() => expect(el.textContent).toContain("You wait for no agent"));
    expect(watchCards(el).length).toBe(0);
    el = mount("/inbox?status=done");
    await waitFor(() => expect(watchCards(el).length).toBe(1));
    expect(el.textContent).toContain("Answered by terrarium/lead");
  });
});

describe("the ack of a mail that closed outside the Inbox (GitHub #25)", () => {
  it("treats a 409 on ack as success: no error banner, the card leaves Open", async () => {
    // The mail was read or replied outside the Inbox (the CLI, the Mail
    // page): the daemon answers the ack with 409, and the card closes here.
    const spy = vi.spyOn(api.inbox, "ackMail").mockImplementation(async () => {
      throw new ApiError(409, "mail is acknowledged already (read)", undefined, "conflict");
    });
    try {
      const el = mount("/inbox?tab=mail");
      await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBeGreaterThan(0));
      const before = el.querySelectorAll("article[aria-label^='Mail ']").length;
      const card = [...el.querySelectorAll("article[aria-label^='Mail ']")].find((c) => c.textContent!.includes("0.1.0 ready for review"))!;
      card.querySelector<HTMLFormElement>("form[aria-label=Acknowledge]")!.requestSubmit();
      await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Mail ']").length).toBe(before - 1));
      expect(el.textContent).not.toContain("is acknowledged already");
      expect(spy).toHaveBeenCalledTimes(1);
    } finally {
      spy.mockRestore();
    }
  });
});

describe("the Tracking tab of the Threads (TER-145)", () => {
  it("shows Tracking between Open and Done on the Threads tab only, and lists only the tracking threads", async () => {
    let el = mount("/inbox?tab=thread");
    await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Thread ']").length).toBeGreaterThan(0));
    const tabs = () => [...el.querySelectorAll("[role=tab]")].map((t) => t.textContent?.trim());
    expect(tabs()).toEqual(["All", "Questions", "Notices", "Mail to you", "System", "Threads", "Open", "Tracking", "Done", "Asked"]);
    // The mock has no tracking thread: the Tracking list is empty.
    el = mount("/inbox?tab=thread&status=tracking");
    await waitFor(() => expect(el.textContent).toContain("No thread tracks work"));
    expect(el.querySelectorAll("article[aria-label^='Thread ']").length).toBe(0);
    // Another kind tab has no Tracking tab, and ?status=tracking opens Open there.
    el = mount("/inbox?tab=question&status=tracking");
    await waitFor(() => expect(el.textContent).not.toContain("Loading"));
    expect(tabs()).not.toContain("Tracking");
    expect([...el.querySelectorAll("[role=tab][aria-selected=true]")].map((t) => t.textContent?.trim())).toEqual(["Questions", "Open"]);
  });
});

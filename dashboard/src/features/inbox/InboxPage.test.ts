// @vitest-environment jsdom
// The Inbox page with the mock API (TER-42): a waiting ask shows under the
// Asked status tab and not under Open; when the agent answers, the ask
// leaves Asked, its reply shows on Open, and the ask is under Done.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import { waitFor } from "../../test/wait";

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

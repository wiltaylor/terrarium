// @vitest-environment jsdom
// Inbox threads in the dashboard (TER-94, docs/api.md §6.13), with the mock
// API: the Threads tab (open threads first, waiting-on-you marked), the New
// thread dialog, and the chat page: reply, answer an inline question, send
// the response, and close with the confirmation.
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
  projects: { projects: () => api.world.projects },
  agents: { state: { agents: api.world.agents } },
}));

const { default: InboxPage } = await import("./InboxPage");
const { default: ThreadPage } = await import("./ThreadPage");

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
          return [createComponent(Route, { path: "/inbox/threads/:id", component: ThreadPage }), createComponent(Route, { path: "/inbox", component: InboxPage })];
        },
      }),
    el,
  );
  return { el, history };
}

const button = (text: string, root: ParentNode = document) => [...root.querySelectorAll("button")].find((b) => b.textContent?.trim() === text);
const setValue = (sel: string, value: string, ev = "input", root: ParentNode = document) => {
  const e = root.querySelector(sel) as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;
  e.value = value;
  e.dispatchEvent(new Event(ev, { bubbles: true }));
};
const status = (el: HTMLElement) => el.querySelector("[aria-label='Thread details'] [data-testid=thread-status]")?.textContent?.trim();

describe("the Threads tab of the Inbox", () => {
  it("lists the open threads, the one that waits on you first and marked; Done lists the closed ones", async () => {
    let { el } = mount("/inbox?tab=thread");
    await waitFor(() => expect(el.querySelectorAll("article[aria-label^='Thread ']").length).toBe(2));
    const rows = [...el.querySelectorAll("article[aria-label^='Thread ']")];
    expect(rows[0]!.classList.contains("waits")).toBe(true);
    expect(rows[0]!.querySelector("[data-testid=thread-status]")!.textContent).toBe("waiting on you");
    expect(rows[0]!.textContent).toContain("2 open questions");
    expect(rows[1]!.querySelector("[data-testid=thread-status]")!.textContent).toBe("waiting on the agent");
    expect(rows[0]!.querySelector("a")!.getAttribute("href")).toMatch(/^\/inbox\/threads\//);
    ({ el } = mount("/inbox?tab=thread&status=done"));
    await waitFor(() => expect(el.textContent).toContain("Rename the runner crate"));
    expect(el.querySelectorAll("article[aria-label^='Thread ']").length).toBe(1);
    // The All tab shows the waiting thread as an Inbox item, not its questions.
    ({ el } = mount("/inbox"));
    await waitFor(() => expect(el.textContent).toContain("Split the event store by project?"));
    expect(el.textContent).not.toContain("Keep the audit rows when we prune?");
  });

  it("follows the project filter: only the threads of that scope, no global ones", async () => {
    // A global thread (the mock has no lead of `meta`, so it cannot open one).
    const seeded = api.world.threads!.find((t) => t.status === "waiting_agent")!;
    api.world.threads!.push({ ...seeded, id: "01JBTHRDGLOBAL0000000000001", topic: "A global question", scope: "global", messages: [] });
    let { el } = mount("/inbox?tab=thread&project=webshop");
    await waitFor(() => expect(el.textContent).toContain("Checkout retries"));
    expect(el.querySelectorAll("article[aria-label^='Thread ']").length).toBe(1);
    expect(el.textContent).not.toContain("A global question");
    expect(el.textContent).not.toContain("Split the event store by project?");
    ({ el } = mount("/inbox?tab=thread"));
    await waitFor(() => expect(el.textContent).toContain("A global question"));
    api.world.threads!.splice(api.world.threads!.findIndex((t) => t.id === "01JBTHRDGLOBAL0000000000001"), 1);
  });

  it("opens a thread from the New thread dialog and shows its chat page", async () => {
    const { el, history } = mount("/inbox?tab=thread");
    await waitFor(() => expect(button("New thread")).toBeTruthy());
    button("New thread")!.click();
    await waitFor(() => expect(document.querySelector("form[aria-label='New thread']")).not.toBeNull());
    button("Open the thread")!.click();
    await waitFor(() => expect(document.body.textContent).toContain("Write the topic."));
    setValue("select[aria-label=Scope]", "terrarium", "change");
    setValue("select[aria-label=Agent]", "terrarium/lead", "change");
    setValue("input[aria-label=Topic]", "Move the runner to a crate of its own?");
    setValue("textarea[aria-label='First message']", "What would **break**?");
    button("Open the thread")!.click();
    await waitFor(() => expect(history.get()).toMatch(/^\/inbox\/threads\//));
    await waitFor(() => expect(el.querySelector("h1")?.textContent).toBe("Move the runner to a crate of its own?"));
    expect(status(el)).toBe("waiting on the agent");
    expect(el.querySelector("[aria-label=Conversation] strong")!.textContent).toBe("You");
    expect(el.querySelector("[aria-label=Conversation] .prose strong")!.textContent).toBe("break");
    expect(el.textContent).toContain("reports to terrarium/lead");
  });
});

describe("the chat page of a thread", () => {
  it("replies, answers the inline questions, sends the response, and closes with the confirmation", async () => {
    const t = (await api.threads.list())[0]!;
    expect(t.status).toBe("waiting_operator");
    const { el } = mount(`/inbox/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("h1")?.textContent).toBe(t.topic));
    expect(status(el)).toBe("waiting on you");
    // The questions are inline, after the messages.
    const log = el.querySelector("[aria-label=Conversation]")!;
    expect([...log.children].map((c) => (c.matches("article[aria-label^='Message ']") ? "m" : "q"))).toEqual(["m", "m", "q", "q"]);

    // Answer the first question with an option, the second in own words.
    const [q1, q2] = [...log.querySelectorAll("article[aria-label^='Question ']")] as HTMLElement[];
    button("Prune after 90 days", q1)!.click();
    button("Send answer", q1)!.click();
    await waitFor(() => expect(q1!.textContent).toContain("Chosen"));
    expect(status(el)).toBe("waiting on you");
    setValue("textarea[aria-label='Answer text']", "Keep them for a year.", "input", q2);
    button("Send answer", q2)!.click();
    await waitFor(() => expect(status(el)).toBe("waiting on the agent"));

    // The agent answers; the operator replies.
    api.sim.threadPost(t.id, "Then I prune after 90 days and keep the audit rows.");
    setValue("textarea[aria-label='Message text']", "Good. Also <img src=x onerror=\"window.__xss=1\"> keep a backup.");
    button("Send")!.click();
    await waitFor(() => expect(log.textContent).toContain("Then I prune after 90 days"));
    await waitFor(() => expect(log.textContent).toContain("keep a backup."));
    expect((window as unknown as { __xss?: number }).__xss).toBeUndefined();
    expect(log.querySelector("img[onerror]")).toBeNull();
    expect((el.querySelector("textarea[aria-label='Message text']") as HTMLTextAreaElement).value).toBe("");

    // The overall response: the approval record.
    setValue("textarea[aria-label='Response text']", "Approved: prune after 90 days, keep audit rows.");
    button("Send response")!.click();
    await waitFor(() => expect(el.querySelector("section[aria-label='Your response']")?.textContent).toContain("Approved: prune after 90 days"));
    expect(log.querySelector("article.response .chip")!.textContent).toBe("response");

    // Close: a confirmation with the handoff; Cancel keeps it open.
    button("Close the thread")!.click();
    await waitFor(() => expect(document.querySelector("form[aria-label='Close the thread']")).not.toBeNull());
    expect(document.querySelector("[data-testid=close-retracts]")).toBeNull();
    button("Cancel", document.querySelector("form[aria-label='Close the thread']")!)!.click();
    await waitFor(() => expect(document.querySelector("form[aria-label='Close the thread']")).toBeNull());
    expect(status(el)).not.toBe("closed");
    button("Close the thread")!.click();
    await waitFor(() => expect(document.querySelector("form[aria-label='Close the thread']")).not.toBeNull());
    setValue("textarea[aria-label=Summary]", "Pruning lands in TER-99.");
    setValue("textarea[aria-label=Links]", "card TER-99\nhttps://example.com/plan\njavascript:alert(1)");
    button("Yes, close the thread")!.click();
    await waitFor(() => expect(status(el)).toBe("closed"));
    const handoff = el.querySelector("section[aria-label=Handoff]")!;
    expect(handoff.textContent).toContain("Pruning lands in TER-99.");
    expect([...handoff.querySelectorAll("a")].map((a) => a.getAttribute("href"))).toEqual(["https://example.com/plan"]);
    expect(handoff.textContent).toContain("javascript:alert(1)");
    expect(el.querySelector("form[aria-label=Message]")).toBeNull();
    expect(el.querySelector("form[aria-label='Send response']")).toBeNull();
    expect(button("Close the thread")).toBeUndefined();
    expect((await api.threads.get(t.id)).handoff!.links).toEqual([
      { kind: "card", ref: "TER-99" },
      { kind: "url", ref: "https://example.com/plan" },
      { kind: "other", ref: "javascript:alert(1)" },
    ]);
  });

  it("says that a close retracts the open questions", async () => {
    const t = await api.threads.open({ topic: "One question", scope: "terrarium", agent: "terrarium/lead" });
    api.sim.threadAsk(t.id, { text: "Yes or no?", options: ["Yes", "No"] });
    const { el } = mount(`/inbox/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("article[aria-label^='Question ']")).not.toBeNull());
    button("Close the thread")!.click();
    await waitFor(() => expect(document.querySelector("[data-testid=close-retracts]")?.textContent).toContain("1 open question is retracted"));
    button("Yes, close the thread")!.click();
    await waitFor(() => expect(status(el)).toBe("closed"));
    expect(el.querySelector("article[aria-label^='Question ']")!.textContent).toContain("retracted");
  });

  it("drops a javascript: Markdown link in a message", async () => {
    const t = await api.threads.open({ topic: "Links", scope: "terrarium", agent: "terrarium/lead" });
    api.sim.threadPost(t.id, "See [the plan](javascript:alert(1)) and [the board](https://example.com/b).");
    const { el } = mount(`/inbox/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("[aria-label=Conversation]")?.textContent).toContain("the plan"));
    const hrefs = [...el.querySelectorAll("[aria-label=Conversation] a")].map((a) => a.getAttribute("href") ?? "");
    expect(hrefs).toContain("https://example.com/b");
    expect(hrefs.some((h) => h.toLowerCase().includes("javascript"))).toBe(false);
  });

  it("reads the thread again when a write gets a 409, also without events", async () => {
    const t = await api.threads.open({ topic: "Closed elsewhere", scope: "terrarium", agent: "terrarium/lead" });
    const { el } = mount(`/inbox/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("h1")?.textContent).toBe("Closed elsewhere"));
    await api.threads.close(t.id, { summary: "Closed by the CLI." });
    setValue("textarea[aria-label='Message text']", "Still there?");
    button("Send")!.click();
    await waitFor(() => expect(status(el)).toBe("closed"));
    expect(el.querySelector("section[aria-label=Handoff]")!.textContent).toContain("Closed by the CLI.");
    expect(el.querySelector("form[aria-label=Message]")).toBeNull();
  });

  it("shows a missing thread", async () => {
    const { el } = mount("/inbox/threads/01JBNOPE");
    await waitFor(() => expect(el.textContent).toContain("No such thread."));
  });
});

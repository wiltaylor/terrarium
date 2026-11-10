// @vitest-environment jsdom
// Inbox threads in the dashboard (TER-94, docs/api.md §6.13), with the mock
// API: the Threads screen (TER-466: a forum table, open threads first, waiting-on-you marked), the New
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
const { default: ThreadsPage } = await import("./ThreadsPage");

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
          return [
            createComponent(Route, { path: "/threads/:id", component: ThreadPage }),
            createComponent(Route, { path: "/threads", component: ThreadsPage }),
            createComponent(Route, { path: "/inbox", component: InboxPage }),
          ];
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
/** The rows of the Threads table. */
const rows = (el: ParentNode) => [...el.querySelectorAll<HTMLElement>("tbody[aria-label^='Thread ']")];
/** The status icon of a row, and its tooltip. */
const icon = (r: HTMLElement) => r.querySelector<HTMLElement>("[data-testid=thread-status]")!;
const status = (el: HTMLElement) => el.querySelector("[aria-label='Thread details'] [data-testid=thread-status]")?.textContent?.trim();

describe("the Threads screen (TER-466)", () => {
  it("lists the open threads in a table, the one that waits on you first with the person icon; Archived lists the closed ones", async () => {
    let { el } = mount("/threads");
    await waitFor(() => expect(rows(el).length).toBe(2));
    const [first, second] = rows(el);
    expect(first!.classList.contains("waits")).toBe(true);
    expect(icon(first!).dataset.icon).toBe("you");
    expect(icon(first!).getAttribute("title")).toBe("Waiting on you: 2 open questions");
    expect(icon(first!).querySelector("svg")).not.toBeNull();
    // The columns: icon, topic (with scope, agent, report-to), messages, open questions, cards, last activity.
    expect([...el.querySelectorAll("thead th")].map((th) => th.textContent?.trim())).toEqual(["Status", "Topic", "Messages", "Open questions", "Cards", "Last activity"]);
    expect(first!.querySelector("td[data-label='Open questions']")!.textContent).toBe("2");
    expect(first!.querySelector(".thread-meta")!.textContent).toContain("reports to");
    expect(icon(second!).dataset.icon).toBe("agent");
    expect(icon(second!).getAttribute("title")).toBe("Waiting on the agent");
    expect(first!.querySelector("a")!.getAttribute("href")).toMatch(/^\/threads\//);
    ({ el } = mount("/threads?status=archived"));
    await waitFor(() => expect(el.textContent).toContain("Rename the runner crate"));
    expect(rows(el).length).toBe(1);
    expect(icon(rows(el)[0]!).dataset.icon).toBe("closed");
    expect([...el.querySelectorAll("[role=tab][aria-selected=true]")].map((t) => t.textContent?.trim())).toEqual(["Archived"]);
    // The Inbox shows neither the thread nor its questions.
    ({ el } = mount("/inbox"));
    await waitFor(() => expect(el.textContent).not.toContain("Loading"));
    expect(el.textContent).not.toContain("Split the event store by project?");
    expect(el.textContent).not.toContain("Keep the audit rows when we prune?");
  });

  it("follows the project filter: only the threads of that scope, no global ones", async () => {
    // A global thread (the mock has no lead of `meta`, so it cannot open one).
    const seeded = api.world.threads!.find((t) => t.status === "waiting_agent")!;
    api.world.threads!.push({ ...seeded, id: "01JBTHRDGLOBAL0000000000001", topic: "A global question", scope: "global", messages: [] });
    let { el } = mount("/threads?project=webshop");
    await waitFor(() => expect(el.textContent).toContain("Checkout retries"));
    expect(rows(el).length).toBe(1);
    expect(el.textContent).not.toContain("A global question");
    expect(el.textContent).not.toContain("Split the event store by project?");
    ({ el } = mount("/threads"));
    await waitFor(() => expect(el.textContent).toContain("A global question"));
    api.world.threads!.splice(api.world.threads!.findIndex((t) => t.id === "01JBTHRDGLOBAL0000000000001"), 1);
  });

  it("lists a tracking thread under Open with the tracking icon", async () => {
    const t = api.world.threads!.find((x) => x.topic === "Checkout retries")!;
    const old = t.status;
    t.status = "tracking";
    try {
      const { el } = mount("/threads");
      await waitFor(() => expect(rows(el).find((r) => r.textContent?.includes("Checkout retries"))).toBeTruthy());
      const r = rows(el).find((x) => x.textContent?.includes("Checkout retries"))!;
      expect(icon(r).dataset.icon).toBe("tracking");
      expect(r.classList.contains("waits")).toBe(false);
    } finally {
      t.status = old;
    }
  });

  it("opens a thread from the New thread dialog and shows its chat page", async () => {
    const { el, history } = mount("/threads");
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
    await waitFor(() => expect(history.get()).toMatch(/^\/threads\//));
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
    const { el } = mount(`/threads/${t.id}`);
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

  it("shows that an ended thread agent cannot answer", async () => {
    const t = await api.threads.open({ topic: "Agent ended", scope: "terrarium", agent: "terrarium/lead" });
    const stored = api.world.threads!.find((th) => th.id === t.id)!;
    stored.agent_end_outcome = "failed";
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(el.textContent).toContain("agent ended: failed. Nobody will answer in this thread."));
  });

  it("says that a close retracts the open questions", async () => {
    const t = await api.threads.open({ topic: "One question", scope: "terrarium", agent: "terrarium/lead" });
    api.sim.threadAsk(t.id, { text: "Yes or no?", options: ["Yes", "No"] });
    const { el } = mount(`/threads/${t.id}`);
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
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("[aria-label=Conversation]")?.textContent).toContain("the plan"));
    const hrefs = [...el.querySelectorAll("[aria-label=Conversation] a")].map((a) => a.getAttribute("href") ?? "");
    expect(hrefs).toContain("https://example.com/b");
    expect(hrefs.some((h) => h.toLowerCase().includes("javascript"))).toBe(false);
  });

  it("reads the thread again when a write gets a 409, also without events", async () => {
    const t = await api.threads.open({ topic: "Closed elsewhere", scope: "terrarium", agent: "terrarium/lead" });
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("h1")?.textContent).toBe("Closed elsewhere"));
    await api.threads.close(t.id, { summary: "Closed by the CLI." });
    setValue("textarea[aria-label='Message text']", "Still there?");
    button("Send")!.click();
    await waitFor(() => expect(status(el)).toBe("closed"));
    expect(el.querySelector("section[aria-label=Handoff]")!.textContent).toContain("Closed by the CLI.");
    expect(el.querySelector("form[aria-label=Message]")).toBeNull();
  });

  it("shows a missing thread", async () => {
    const { el } = mount("/threads/01JBNOPE");
    await waitFor(() => expect(el.textContent).toContain("No such thread."));
  });
});

describe("the agent's close proposal (TER-114)", () => {
  const proposal = (root: ParentNode = document) => root.querySelector("section[aria-label='Close proposal']") as HTMLElement | null;
  const row = (el: HTMLElement, topic: string) => rows(el).find((r) => r.textContent?.includes(topic));

  it("shows the proposal on the thread page; Decline is a message, and the thread stays open", async () => {
    const t = await api.threads.open({ topic: "Proposal declined", scope: "terrarium", agent: "terrarium/lead", text: "Go." });
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("h1")?.textContent).toBe("Proposal declined"));
    expect(proposal(el)).toBeNull();
    api.sim.threadPropose(t.id, { summary: "TER-99 has the plan.\n<img src=x onerror=\"window.__xss=2\">", links: [{ kind: "card", ref: "TER-99" }, { kind: "url", ref: "https://example.com/p" }, { kind: "url", ref: "javascript:alert(1)" }] });
    // The events are off in this test: mount the page again to read the proposal.
    mount(`/threads/${t.id}`);
    await waitFor(() => expect(proposal()).not.toBeNull());
    const p = proposal()!;
    expect(status(document.body)).toBe("close proposed");
    expect(p.textContent).toContain("terrarium/lead proposes to close the thread");
    expect(p.querySelector("[data-testid=proposal-summary]")!.textContent).toContain("<img src=x");
    expect(p.querySelector("img")).toBeNull();
    expect([...p.querySelectorAll("a")].map((a) => a.getAttribute("href"))).toEqual(["https://example.com/p"]);
    expect(p.textContent).toContain("javascript:alert(1)");

    button("Decline", p)!.click();
    await waitFor(() => expect(p.querySelector("form[aria-label='Decline the close']")).not.toBeNull());
    button("Send and decline", p)!.click();
    await waitFor(() => expect(p.textContent).toContain("Write the message."));
    setValue("textarea[aria-label='Decline text']", "Not yet: add the rollback to TER-99.", "input", p);
    button("Send and decline", p)!.click();
    await waitFor(() => expect(proposal()).toBeNull());
    expect(status(document.body)).toBe("waiting on the agent");
    expect(document.querySelector("[aria-label=Conversation]")!.textContent).toContain("Not yet: add the rollback to TER-99.");
    const after = await api.threads.get(t.id);
    expect(after).toMatchObject({ status: "waiting_agent", close_proposal: null });
    expect(after.messages!.at(-1)).toMatchObject({ author: "operator", kind: "message", text: "Not yet: add the rollback to TER-99." });
  });

  it("confirms the proposal on the thread page: the thread closes with the agent's handoff", async () => {
    const t = await api.threads.open({ topic: "Proposal confirmed", scope: "terrarium", agent: "terrarium/lead", text: "Go." });
    api.sim.threadAsk(t.id, { text: "Anything more?", options: ["Yes", "No"] });
    api.sim.threadPropose(t.id, { summary: "All in TER-100.", links: [{ kind: "card", ref: "TER-100" }] });
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(proposal(el)).not.toBeNull());
    expect(proposal(el)!.querySelector("[data-testid=proposal-retracts]")!.textContent).toBe("A confirm retracts 1 open question.");
    button("Confirm", proposal(el)!)!.click();
    await waitFor(() => expect(status(el)).toBe("closed"));
    expect(proposal(el)).toBeNull();
    const handoff = el.querySelector("section[aria-label=Handoff]")!;
    expect(handoff.textContent).toContain("You confirmed the close that terrarium/lead proposed");
    expect(handoff.textContent).toContain("All in TER-100.");
    expect(handoff.textContent).toContain("TER-100");
    expect(el.querySelector("article[aria-label^='Question ']")!.textContent).toContain("retracted");
    expect(await api.threads.get(t.id)).toMatchObject({ close_reason: "confirmed", closed_by: "operator" });
  });

  it("shows the proposal in the Threads list, with Confirm and Decline", async () => {
    const a = await api.threads.open({ topic: "Row confirm", scope: "terrarium", agent: "terrarium/lead", text: "Go." });
    const b = await api.threads.open({ topic: "Row decline", scope: "terrarium", agent: "terrarium/lead", text: "Go." });
    api.sim.threadPropose(a.id, { summary: "Row A is done." });
    api.sim.threadPropose(b.id, { summary: "Row B is done.", links: [{ kind: "wiki", ref: "plan" }] });
    let { el } = mount("/threads");
    await waitFor(() => expect(row(el, "Row confirm")).toBeTruthy());
    const ra = row(el, "Row confirm")!;
    expect(ra.classList.contains("waits")).toBe(true);
    expect(icon(ra).dataset.icon).toBe("you");
    expect(icon(ra).getAttribute("title")).toBe("Waiting on you: a close proposal");
    expect(proposal(ra)!.textContent).toContain("Row A is done.");
    expect(proposal(row(el, "Row decline")!)!.textContent).toContain("plan");

    button("Confirm", proposal(ra)!)!.click();
    await waitFor(() => expect(row(el, "Row confirm")).toBeUndefined());
    expect((await api.threads.get(a.id)).close_reason).toBe("confirmed");

    const rb = row(el, "Row decline")!;
    button("Decline", proposal(rb)!)!.click();
    setValue("textarea[aria-label='Decline text']", "Link the card too.", "input", rb);
    button("Send and decline", rb)!.click();
    await waitFor(() => expect(row(el, "Row decline")?.querySelector("section[aria-label='Close proposal']")).toBeNull());
    expect(icon(row(el, "Row decline")!).dataset.icon).toBe("agent");
    expect((await api.threads.get(b.id)).close_proposal).toBeNull();

    // The confirmed thread is under Archived.
    ({ el } = mount("/threads?status=archived"));
    await waitFor(() => expect(el.textContent).toContain("Row confirm"));
  });

  it("counts a question moved into the thread (in_thread) in both close warnings (TER-495)", async () => {
    const q = (await api.questions.ofAgent("terrarium/lead")).find((x) => x.kind === "question" && x.status === "open" && !x.thread)!;
    const { thread: t } = await api.questions.toThread(q.id);
    api.sim.threadPropose(t.id, { summary: "Settled in the thread." });
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(proposal(el)).not.toBeNull());
    expect(proposal(el)!.querySelector("[data-testid=proposal-retracts]")!.textContent).toBe("A confirm retracts 1 open question.");
    button("Close the thread")!.click();
    await waitFor(() => expect(document.querySelector("[data-testid=close-retracts]")?.textContent).toContain("1 open question is retracted"));
    button("Yes, close the thread")!.click();
    await waitFor(() => expect(status(el)).toBe("closed"));
    expect((await api.questions.get(q.id)).status).toBe("retracted");
  });

  it("shows the question that opened the thread, with no Open in thread (GitHub #32)", async () => {
    const q = (await api.questions.ofAgent("terrarium/lead")).find((x) => x.kind === "question" && x.status === "open" && !x.thread)!;
    const { thread: t } = await api.questions.toThread(q.id);
    expect(t.from_question).toBe(q.id);
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector(`article[aria-label='Question ${q.id}']`)).not.toBeNull());
    const card = el.querySelector(`article[aria-label='Question ${q.id}']`)!;
    expect([...card.querySelectorAll("a, button")].some((x) => x.textContent?.trim() === "Open in thread")).toBe(false);
    expect([...card.querySelectorAll("a")].some((x) => x.textContent?.trim() === "Show mail")).toBe(true);
  });
});

describe("the linked cards of a thread (TER-147)", () => {
  /** The seeded thread with linked cards: WEB-4 (in progress), WEB-1 (done), TER-16 (review). */
  const linked = () => api.world.threads!.find((t) => t.topic === "Checkout retries")!;
  const panel = (el: HTMLElement) => el.querySelector("details[aria-label='Linked cards']") as HTMLDetailsElement | null;
  const tile = (el: HTMLElement, id: string) => el.querySelector(`a.tracked-tile[data-card='${id}']`) as HTMLAnchorElement | null;
  const withStatus = async (status: "tracking" | "waiting_agent", fn: () => Promise<void>) => {
    const t = linked();
    const old = t.status;
    t.status = status;
    try {
      await fn();
    } finally {
      t.status = old;
    }
  };

  it("shows a mini kanban, one row per project with its own board columns, folded while the thread does not track", async () => {
    const { el } = mount(`/threads/${linked().id}`);
    await waitFor(() => expect(tile(el, "WEB-4")).not.toBeNull());
    const p = panel(el)!;
    expect(p.open).toBe(false);
    expect(p.dataset.open).toBe("folded");
    expect(p.querySelector("summary")!.textContent).toContain("3 cards in 2 projects");
    expect(p.querySelector("summary [data-testid=thread-progress]")!.textContent).toBe("2/3");

    // M2: one row per project, in the order of the first link, each with the columns of its own board in board order.
    const rows = [...p.querySelectorAll<HTMLElement>("section.tracked-row")];
    expect(rows.map((r) => r.dataset.project)).toEqual(["webshop", "terrarium"]);
    const cols = (r: HTMLElement) => [...r.querySelectorAll<HTMLElement>(".tracked-col")];
    const board = await api.board.get("webshop");
    expect(cols(rows[0]!).map((c) => c.dataset.column)).toEqual(board.columns.map((c) => c.name));
    // An empty column is narrow; a column with a linked card is not.
    const webCol = (name: string) => cols(rows[0]!).find((c) => c.dataset.column === name)!;
    expect(webCol("backlog").classList.contains("empty")).toBe(true);
    expect(webCol("in_progress").classList.contains("empty")).toBe(false);
    expect(webCol("done").querySelector("[data-card='WEB-1']")).not.toBeNull();
    // Only the linked cards: WEB-5 (blocked) is on the board but not linked.
    expect(webCol("blocked").querySelector(".tracked-tile")).toBeNull();
    expect(cols(rows[1]!).find((c) => c.dataset.column === "review")!.querySelector("[data-card='TER-16']")).not.toBeNull();

    // M1: project:ID, title, assignee, ETA, and the state chip; a click opens the card on its board.
    const t4 = tile(el, "WEB-4")!;
    expect(t4.textContent).toContain("webshop:WEB-4");
    expect(t4.querySelector(".title")!.textContent).toBe("Checkout flow");
    expect(t4.textContent).toContain("shop-lead");
    expect(t4.querySelector("[role=timer]")!.textContent).toMatch(/left$/);
    expect(t4.getAttribute("href")).toBe("/p/webshop/board?card=WEB-4");
    expect(tile(el, "TER-16")!.querySelector(".card-state")!.textContent).toBe("awaiting review");
    expect(tile(el, "TER-16")!.getAttribute("href")).toBe("/p/terrarium/board?card=TER-16");
  });

  it("is open at the top while the thread tracks", async () => {
    await withStatus("tracking", async () => {
      const { el } = mount(`/threads/${linked().id}`);
      await waitFor(() => expect(tile(el, "WEB-4")).not.toBeNull());
      const p = panel(el)!;
      expect(p.open).toBe(true);
      expect(p.dataset.open).toBe("tracking");
      // Above the conversation.
      const log = el.querySelector("[aria-label=Conversation]")!;
      expect(p.compareDocumentPosition(log) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });
  });

  it("shows no panel on a thread with no linked card", async () => {
    const t = api.world.threads!.find((x) => x.topic === "Split the event store by project?")!;
    const { el } = mount(`/threads/${t.id}`);
    await waitFor(() => expect(el.querySelector("h1")?.textContent).toBe(t.topic));
    expect(panel(el)).toBeNull();
  });

  it("shows the n/m chip in the Threads list, read live from the boards", async () => {
    const rowOf = (el: HTMLElement, topic: string) => rows(el).find((r) => r.textContent?.includes(topic));
    await withStatus("tracking", async () => {
      let { el } = mount("/threads");
      await waitFor(() => expect(rowOf(el, "Checkout retries")?.querySelector("[data-testid=thread-progress]")?.textContent).toBe("2/3"));
      const chip = rowOf(el, "Checkout retries")!.querySelector("[data-testid=thread-progress]")!;
      expect(chip.getAttribute("title")).toBe("2 of 3 linked cards are in review or later");
      expect(chip.classList.contains("ok")).toBe(false);

      // WEB-4 reaches review: the next read of the boards counts it.
      const web4 = api.world.cards.find((c) => c.id === "WEB-4")!;
      const old = web4.column;
      web4.column = "review";
      try {
        ({ el } = mount("/threads"));
        await waitFor(() => expect(rowOf(el, "Checkout retries")?.querySelector("[data-testid=thread-progress]")?.textContent).toBe("3/3"));
        expect(rowOf(el, "Checkout retries")!.querySelector("[data-testid=thread-progress]")!.classList.contains("ok")).toBe(true);
      } finally {
        web4.column = old;
      }
    });
    // A thread with no linked card has no chip.
    await api.threads.open({ topic: "No cards here", scope: "terrarium", agent: "terrarium/lead", text: "Go." });
    const { el } = mount("/threads");
    await waitFor(() => expect(rowOf(el, "No cards here")).toBeTruthy());
    expect(rowOf(el, "No cards here")!.querySelector("[data-testid=thread-progress]")).toBeNull();
  });
});

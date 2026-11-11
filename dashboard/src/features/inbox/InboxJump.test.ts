// @vitest-environment jsdom
// TER-40: new Inbox items from the event stream do not reset the card that
// the operator works on. The typed answer, the chosen option, and the focus
// stay; the card keeps its DOM.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentEvent } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { waitFor } from "../../test/wait";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners: ((e: AgentEvent) => void)[] = [];
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  api,
  events: { on: (fn: (e: AgentEvent) => void) => (listeners.push(fn), () => {}) },
  inbox: { refresh: async () => {} },
  projects: { projects: () => [] },
  agents: { state: { agents: [] } },
})); });

const { default: InboxPage } = await import("./InboxPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

function mount(url: string) {
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

describe("the Inbox keeps the card in work (TER-40)", () => {
  it("groups a card's question blocks and keeps its draft when a sibling is added", async () => {
    const q = api.world.questions!.find((x) => x.kind === "question" && x.status === "open")!;
    q.card = "TER-703";
    api.world.questions!.push({ ...q, id: "sibling-703", text: "Another decision?" });
    const el = mount("/inbox");
    const selector = `article[aria-label='Question ${q.id}']`;
    await waitFor(() => expect(el.querySelector(selector)).not.toBeNull());
    const group = el.querySelector("section[aria-label='Questions for TER-703']")!;
    expect(group.querySelectorAll("article").length).toBe(2);
    const first = el.querySelector(selector)!;
    const box = first.querySelector("textarea")!;
    box.value = "Unfinished answer";
    box.dispatchEvent(new Event("input", { bubbles: true }));
    api.world.questions!.push({ ...q, id: "third-703", text: "Third decision?" });
    [...el.querySelectorAll("button")].find(b => b.textContent === "Refresh")!.click();
    await waitFor(() => expect(group.querySelectorAll("article").length).toBe(3));
    expect(el.querySelector(selector)).toBe(first);
    expect(box.value).toBe("Unfinished answer");
  });
  it("keeps the draft, the chosen option, the focus, and the card's DOM when new items come", async () => {
    const q = api.world.questions!.find((x) => x.kind === "question" && x.status === "open" && x.options.length >= 2)!;
    const el = mount("/inbox");
    const card = () => el.querySelector(`article[aria-label='Question ${q.id}']`) as HTMLElement | null;
    await waitFor(() => expect(card()).not.toBeNull());
    const first = card()!;
    const box = first.querySelector("textarea[aria-label='Answer text']") as HTMLTextAreaElement;
    const option = [...first.querySelectorAll("[role=group][aria-label=Options] button")][1] as HTMLButtonElement;
    option.click();
    box.focus();
    box.value = "half of my answer";
    box.dispatchEvent(new Event("input", { bubbles: true }));
    const before = el.querySelectorAll("article").length;
    // New actionable items, then the inbox events.
    for (const body of ["one", "two"]) {
      api.world.questions!.push({ ...q, id: `new-${body}`, text: `new ${body}`, options: [] });
    }
    for (const fn of listeners) fn({ kind: "inbox", data: { item: "question:new-two", kind: "question", change: "opened" } } as unknown as AgentEvent);
    await waitFor(() => expect(el.querySelectorAll("article").length).toBe(before + 2), 3000);
    // The same card, with its state.
    expect(card()).toBe(first);
    expect(first.querySelector("textarea[aria-label='Answer text']")).toBe(box);
    expect(box.value).toBe("half of my answer");
    expect(option.getAttribute("aria-pressed")).toBe("true");
    expect(document.activeElement).toBe(box);
    expect(el.textContent).toContain("new two");
  });
});

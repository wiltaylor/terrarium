// @vitest-environment jsdom
import { MemoryRouter, Route } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { Card, Question } from "../../api/types";

vi.mock("../../stores", () => ({
  api: { questions: { list: vi.fn(), answer: vi.fn() } },
  events: { on: () => () => {} },
  inbox: { refresh: vi.fn() },
  projects: { projects: () => [] },
}));
const { api, inbox } = await import("../../stores");
const { CardQuestions } = await import("./CardQuestions");
let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

it("shows the card's question and answers it with the same control as the Inbox", async () => {
  const q = {
    id: "Q1", kind: "question", status: "open", priority: "normal",
    agent: "A1", agent_name: "lead", project: "p", card: "P-1",
    text: "Proceed with the work?", options: [], revision: 1, thread_id: "MT1",
    created_at: "2026-11-10T00:00:00Z", updated_at: "2026-11-10T00:00:00Z",
  } as Question;
  vi.mocked(api.questions.list).mockResolvedValue([q]);
  vi.mocked(api.questions.answer).mockResolvedValue({ ...q, status: "answered", answer: "Proceed" });
  const changed = vi.fn();
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(MemoryRouter, {
    get children() {
      return createComponent(Route, { path: "*", component: () => createComponent(CardQuestions, {
        card: { id: "P-1", project: "p" } as Card, linkable: [], onChanged: changed,
      }) });
    },
  }), el);
  await vi.waitFor(() => expect(el.textContent).toContain(q.text));
  expect(api.questions.list).toHaveBeenCalledWith({ card: "P-1", status: "all" });
  const input = el.querySelector("textarea")!;
  input.value = "Proceed";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  el.querySelector("form[aria-label='Answer']")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(changed).toHaveBeenCalledOnce());
  expect(api.questions.answer).toHaveBeenCalledWith("Q1", { answer: "Proceed" });
  expect(inbox.refresh).toHaveBeenCalledOnce();
  expect(el.querySelector("form[aria-label='Answer']")).toBeNull();
  expect(el.textContent).toContain("Answered (1)");
});

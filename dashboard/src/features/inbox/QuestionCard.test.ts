// @vitest-environment jsdom
// The thread actions of a question card (GitHub #32): "Open in thread" opens
// the linked Inbox thread, or creates one for an open question with none;
// "Show mail" opens the mail thread of the question.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Question } from "../../api/types";

vi.mock("../../stores", () => ({ api: { questions: {} }, projects: { projects: () => [] } }));
const { QuestionCard } = await import("./QuestionCard");
const { BundleCard } = await import("./BundleCard");
const { api } = await import("../../stores");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

const question = (over: Partial<Question> = {}): Question =>
  ({
    id: "Q1",
    kind: "question",
    status: "open",
    priority: "normal",
    agent: "A1",
    agent_name: "lead",
    project: "p",
    text: "Keep the logs?",
    options: ["Yes", "No"],
    revision: 1,
    thread_id: "MT1",
    created_at: "2026-11-10T00:00:00Z",
    updated_at: "2026-11-10T00:00:00Z",
    ...over,
  }) as Question;

function mount(q: Question, inThread = false) {
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  history.set({ value: "/inbox" });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "*", component: () => q.kind === "bundle" ? createComponent(BundleCard, { question: q, now: Date.parse("2026-11-10T00:01:00Z"), onChange: () => {} }) : createComponent(QuestionCard, { question: q, now: Date.parse("2026-11-10T00:01:00Z"), inThread, onChange: () => {} }) });
        },
      }),
    el,
  );
  return el;
}

const links = (el: HTMLElement, text: string) => [...el.querySelectorAll("a")].filter((a) => a.textContent?.trim() === text);
const buttons = (el: HTMLElement, text: string) => [...el.querySelectorAll("button")].filter((b) => b.textContent?.trim() === text);

it.each([[], [0], [0, 1]])("renders and submits checkbox selections %j", async (selected) => {
  const q = question({ multi_select: true });
  const answer = vi.fn().mockResolvedValue({ ...q, status: "answered" });
  api.questions.answer = answer;
  const el = mount(q, true);
  const checks = [...el.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')];
  expect(checks).toHaveLength(2);
  expect(el.querySelector('textarea[aria-label="Answer text"]')).not.toBeNull();
  for (const i of selected) checks[i]!.click();
  expect(buttons(el, "Send answer")[0]!.disabled).toBe(false);
  el.querySelector('form[aria-label="Answer"]')!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await Promise.resolve();
  expect(answer).toHaveBeenCalledWith("Q1", { options: selected });
});

it.each([[], [0], [0, 1]])("renders and submits checkbox bundle selections %j", async (selected) => {
  sessionStorage.clear();
  const q = question({ kind: "bundle", items: [{ text: "Pick", options: ["A", "B"], multi_select: true }, { text: "Other", options: [], multi_select: true }] });
  const answer = vi.fn().mockResolvedValue({ ...q, status: "answered" });
  api.questions.answer = answer;
  const el = mount(q);
  const checks = [...el.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')];
  expect(checks).toHaveLength(2);
  expect(el.querySelectorAll("textarea")).toHaveLength(2);
  for (const i of selected) checks[i]!.click();
  expect(buttons(el, "Submit all answers")[0]!.disabled).toBe(false);
  el.querySelector('form[aria-label="Answer all items"]')!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await Promise.resolve();
  expect(answer).toHaveBeenCalledWith("Q1", { revision: 1, items: [{ options: selected }, { options: [] }] });
});

describe("Open in thread and Show mail (GitHub #32)", () => {
  it("offers to create the thread for an open question with no thread, and links its mail thread as Show mail", () => {
    const el = mount(question());
    expect(buttons(el, "Open in thread")).toHaveLength(1);
    expect(links(el, "Open in thread")).toHaveLength(0);
    expect(links(el, "Show mail").map((a) => a.getAttribute("href"))).toEqual(["/logs/mail?thread=MT1&open=MT1"]);
  });

  it("opens the linked thread of a question in a thread, and creates none", () => {
    const el = mount(question({ status: "in_thread", thread: "TH1" }));
    expect(links(el, "Open in thread").map((a) => a.getAttribute("href"))).toEqual(["/threads/TH1"]);
    expect(buttons(el, "Open in thread")).toHaveLength(0);
    expect(el.querySelector("input[aria-label='Thread message']")).toBeNull();
    expect(links(el, "Show mail")).toHaveLength(1);
  });

  it("keeps the link to the thread after the answer", () => {
    const el = mount(question({ status: "answered", thread: "TH1", answer: "Yes" }));
    expect(links(el, "Open in thread").map((a) => a.getAttribute("href"))).toEqual(["/threads/TH1"]);
  });

  it("offers no thread for a question that is not open and has no thread", () => {
    const el = mount(question({ status: "answered", answer: "Yes" }));
    expect(buttons(el, "Open in thread")).toHaveLength(0);
    expect(links(el, "Open in thread")).toHaveLength(0);
  });

  it("has no Open in thread on the page of its own thread", () => {
    const el = mount(question({ status: "in_thread", thread: "TH1" }), true);
    expect(links(el, "Open in thread")).toHaveLength(0);
    expect(buttons(el, "Open in thread")).toHaveLength(0);
    expect(links(el, "Show mail")).toHaveLength(1);
  });
});

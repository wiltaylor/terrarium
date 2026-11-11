// @vitest-environment jsdom
// Ask and forward in the Inbox (GitHub #24): the Ask dialog, the Forward
// dialog, the answer line of a reply with "Use as answer", and Cancel on a
// watch.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { InboxWatch, Message, Question } from "../../api/types";

const ask = vi.fn();
const openRequest = vi.fn(async () => ({ id: "R1" }));
const cancelWatch = vi.fn();
const preferences = vi.fn(async () => ({ default_agent: null as string | null }));
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  api: { account: { preferences }, threads: { open: openRequest }, inbox: { ask, cancelWatch, ackMail: vi.fn() } },
  agents: { state: { agents: [{ id: "A2", project: "demo", name: "w2", roles: ["project_admin"] }, { id: "A1", project: "demo", name: "w1", roles: ["project_admin"] }, { id: "E1", project: "demo", name: "tmp", roles: [], ephemeral: true }] } },
})); });
vi.mock("@solidjs/router", () => ({ A: (p: { href: string; children?: unknown }) => p.children }));

const { AskDialog } = await import("./AskDialog");
const { MailItemCard, WatchCard } = await import("./cards");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

function mount(fn: () => unknown) {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(fn as never, el);
  const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent?.trim().startsWith(text));
  return { el, button };
}

const settle = () => new Promise((r) => setTimeout(r, 0));
const setValue = (sel: string, value: string, ev = "input") => {
  const e = document.querySelector(sel) as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;
  e.value = value;
  e.dispatchEvent(new Event(ev, { bubbles: true }));
};

const watch = (over: Partial<InboxWatch> = {}): InboxWatch => ({ thread_id: "T1", agent: "A1", agent_name: "demo/w1", message_id: "M1", subject: "Question from the operator: why?", created_at: new Date().toISOString(), waiting: true, replies: 0, ...over });
const question = { id: "Q1", kind: "question", agent_name: "demo/lead", text: "Ship it?", options: ["yes", "no"], status: "open" } as unknown as Question;

describe("the Ask dialog", () => {
  it("preselects the server default and lets the operator override it", async () => {
    preferences.mockResolvedValueOnce({ default_agent: "demo/w2" });
    mount(() => createComponent(AskDialog, { onClose: () => {}, onAsked: () => {} }));
    await settle();
    expect((document.querySelector("select") as HTMLSelectElement).value).toBe("demo/w2");
    setValue("select", "demo/w1", "change");
    expect((document.querySelector("select") as HTMLSelectElement).value).toBe("demo/w1");
  });
  it("lists the agents without the ephemeral ones, checks the input, and asks", async () => {
    ask.mockResolvedValue({ message: { id: "M1" }, watch: watch() });
    const onRequested = vi.fn();
    const { button } = mount(() => createComponent(AskDialog, { onClose: () => {}, onAsked: vi.fn(), onRequested }));
    const options = [...document.querySelectorAll("select[aria-label=Agent] option")].map((o) => o.textContent);
    expect(options).toEqual(["Pick an agent…", "demo/w1", "demo/w2"]);
    button("Ask")!.click();
    await settle();
    expect(ask).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("Pick an agent.");
    setValue("select[aria-label=Agent]", "demo/w1", "change");
    setValue("textarea[aria-label='Question text']", "  Why did it fail?  ");
    button("Ask")!.click();
    await settle();
    expect(openRequest).toHaveBeenCalledWith({ report_to: "demo/w1", topic: "Why did it fail?", text: "Why did it fail?" });
    expect(onRequested).toHaveBeenCalledWith({ id: "R1" });
  });

  it("forwards a question with an optional note", async () => {
    ask.mockResolvedValue({ message: { id: "M2" }, watch: watch({ question_id: "Q1" }) });
    const { button } = mount(() => createComponent(AskDialog, { question, onClose: () => {}, onAsked: () => {} }));
    expect(document.querySelector("[role=dialog]")!.getAttribute("aria-label")).toBe("Forward to an agent");
    expect(document.body.textContent).toContain("Ship it?");
    setValue("select[aria-label=Agent]", "demo/w2", "change");
    button("Forward")!.click();
    await settle();
    expect(ask).toHaveBeenCalledWith({ to: "demo/w2", question: "Q1" });
  });
});

describe("the answer of a watched thread", () => {
  const reply = { id: "M9", thread_id: "T1", from: "A1", from_kind: "agent", from_name: "demo/w1", subject: "Re: Forwarded", body: "Yes, ship it.", state: "delivered", created_at: new Date().toISOString() } as unknown as Message;

  it("links the forwarded question and fills its answer with Use as answer", () => {
    const onOpenQuestion = vi.fn();
    const onUseAsAnswer = vi.fn();
    const { el, button } = mount(() => createComponent(MailItemCard, { message: reply, open: true, now: Date.now(), onAck: () => {}, watch: watch({ question_id: "Q1", waiting: false, replies: 1 }), onOpenQuestion, onUseAsAnswer }));
    expect(el.querySelector("[data-testid=watch-answer]")!.textContent).toContain("answer to your forward of Q1");
    button("answer to your forward")!.click();
    expect(onOpenQuestion).toHaveBeenCalledWith("Q1");
    button("Use as answer")!.click();
    expect(onUseAsAnswer).toHaveBeenCalledWith("Q1", "Yes, ship it.");
  });

  it("names the question of an ask, with no Use as answer", () => {
    const { el, button } = mount(() => createComponent(MailItemCard, { message: reply, open: true, now: Date.now(), onAck: () => {}, watch: watch({ waiting: false, replies: 1 }) }));
    expect(el.querySelector("[data-testid=watch-answer]")!.textContent).toContain('answer to your question "Question from the operator: why?"');
    expect(button("Use as answer")).toBeUndefined();
  });
});

describe("a watch", () => {
  it("shows that it waits, and Cancel stops it", async () => {
    cancelWatch.mockResolvedValue(watch({ cancelled_at: "now" }));
    const onChange = vi.fn();
    const { el, button } = mount(() => createComponent(WatchCard, { watch: watch(), now: Date.now(), onChange }));
    expect(el.textContent).toContain("Waiting for demo/w1: Question from the operator: why?");
    button("Cancel")!.click();
    button("Yes, stop waiting")!.click();
    await settle();
    expect(cancelWatch).toHaveBeenCalledWith("T1");
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ cancelled_at: "now" }));
  });
});

// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, createSignal, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { Message } from "../api/types";

vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  return instanceTestStores({ projects: { projects: () => [{ name: "terrarium", card_prefix: "TER" }] }, api: {} });
});
const { MessageView } = await import("../features/mail/parts");
const { CommentPost } = await import("./CommentPost");
const { PostEditor } = await import("./PostEditor");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); });
function mount(view: () => JSX.Element) {
  const root = document.createElement("div"); document.body.append(root);
  const history = createMemoryHistory(); history.set({ value: "/logs/mail" });
  dispose = render(() => createComponent(MemoryRouter, { history, get children() { return createComponent(Route, { path: "*", component: view }); } }), root);
  return root;
}
it("mail uses numbered forum posts, safe Markdown, a permalink and Quote", () => {
  const quote = vi.fn();
  const message = { id: "M1", thread_id: "T1", from_name: "lead", to_name: "worker", body: "**hello**\n<script>bad()</script>", created_at: "2026-11-11T00:00:00Z", state: "read" } as Message;
  const el = mount(() => createComponent(MessageView, { message, number: 2, onQuote: quote }));
  expect(el.querySelector(".thread-post-header strong")?.textContent).toBe("lead");
  expect(el.querySelector(".prose strong")?.textContent).toBe("hello");
  expect(el.querySelector("script")).toBeNull();
  expect(el.querySelector('[aria-label="Permalink to post 2"]')?.getAttribute("href")).toBe("/logs/mail?open=M1#mail-M1");
  [...el.querySelectorAll("button")].find((b) => b.textContent === "Quote")!.click();
  expect(quote).toHaveBeenCalledWith("> **hello**\n> <script>bad()</script>\n\n");
});
it("card comments preserve Markdown links and session metadata and quote a selection", () => {
  const quote = vi.fn();
  const el = mount(() => createComponent(CommentPost, { card: "TER-1", project: "terrarium", index: 0, onQuote: quote, comment: { author: "lead", body: "**first** and TER-2", created_at: "2026-11-11T00:00:00Z", session: { id: "S1", harness: "codex" } as never } }));
  expect(el.querySelector(".thread-post-body strong")?.textContent).toBe("first");
  expect(el.querySelector('a.wikilink-card')?.getAttribute("href")).toBe("/p/terrarium/board?card=TER-2");
  expect(el.textContent).toContain("View session");
  expect(el.querySelector('[aria-label="Permalink to post 1"]')?.getAttribute("href")).toBe("/p/terrarium/board?card=TER-1#comment-TER-1-1");
  const range = document.createRange(); range.selectNodeContents(el.querySelector(".thread-post-body strong")!);
  window.getSelection()!.removeAllRanges(); window.getSelection()!.addRange(range);
  [...el.querySelectorAll("button")].find((b) => b.textContent === "Quote")!.click();
  expect(quote).toHaveBeenCalledWith("> first\n\n");
});
it("Write and Preview retain the draft and sanitize the preview", () => {
  const el = mount(() => {
    const [text, setText] = createSignal("**draft** <img src=x onerror=bad()>");
    const textarea = document.createElement("textarea");
    textarea.value = text();
    textarea.oninput = () => setText(textarea.value);
    return createComponent(PostEditor, { label: "Reply", project: "terrarium", get text() { return text(); }, children: textarea });
  });
  [...el.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent === "Preview")!.click();
  expect(el.querySelector('[aria-label="Reply preview"] strong')?.textContent).toBe("draft");
  expect(el.querySelector("img")).toBeNull();
  [...el.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent === "Write")!.click();
  expect(el.querySelector("textarea")!.value).toContain("**draft**");
});

// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import { waitFor } from "../../test/wait";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api, events: { on: () => () => {} }, projects: { projects: () => [], prefixes: () => ({}) } });
});
const { default: MailPage } = await import("./MailPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.restoreAllMocks(); });

it("opens mail in time order with an unread boundary, quotes and previews the reply", async () => {
  const first = await api.mail.send({ to: "terrarium/lead", body: "**earlier**", subject: "Forum regression" });
  await api.mail.reply(first.id, "**later**");
  vi.spyOn(api.readPositions, "visit").mockResolvedValue({ previous: first.created_at, read_at: new Date().toISOString() });
  // Keep the fixture timestamps distinct even on a fast mock clock.
  const list = api.mail.list.bind(api.mail);
  vi.spyOn(api.mail, "list").mockImplementation(async (q) => {
    const result = await list(q);
    return { ...result, items: result.items.map((m) => m.thread_id === first.thread_id && m.id !== first.id ? { ...m, created_at: new Date(Date.parse(first.created_at) + 1000).toISOString() } : m) };
  });
  const history = createMemoryHistory(); history.set({ value: `/logs/mail?open=${first.id}` });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => createComponent(MemoryRouter, { history, get children() { return createComponent(Route, { path: "/logs/mail", component: MailPage }); } }), root);
  await waitFor(() => expect(root.querySelectorAll(".log-thread .thread-post")).toHaveLength(2));
  expect([...root.querySelectorAll(".thread-post-body strong")].map((e) => e.textContent)).toEqual(["earlier", "later"]);
  await waitFor(() => expect(root.querySelectorAll('[role="separator"]')).toHaveLength(1));
  const posts = root.querySelectorAll(".thread-post");
  expect(posts[1]!.previousElementSibling?.getAttribute("role")).toBe("separator");
  [...posts[0]!.querySelectorAll("button")].find((b) => b.textContent === "Quote")!.click();
  expect(root.querySelector<HTMLTextAreaElement>('[aria-label="Reply"]')!.value).toBe("> **earlier**\n\n");
  [...root.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent === "Preview")!.click();
  expect(root.querySelector('[aria-label="Reply preview"] blockquote strong')?.textContent).toBe("earlier");
});

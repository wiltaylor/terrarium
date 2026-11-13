import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent } from "../../api/types";
import { waitFor } from "../../test/wait";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(event: AgentEvent) => void>();
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api, projects: { projects: () => [], prefixes: () => ({}) }, events: {
    on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
  } });
});
const { MailPanel } = await import("./MailPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); document.documentElement.scrollTop = 0; vi.restoreAllMocks(); });

it("keeps duplicate filters and open rows local and retains a reply draft through refresh", async () => {
  const message = await api.mail.send({ to: "terrarium/lead", subject: "Panel retention", body: "hello" });
  await api.mail.send({ to: "terrarium/lead", subject: "Above retention", body: "newer row" });
  const history = createMemoryHistory(); history.set({ value: "/host?open=foreign&q=foreign" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><MailPanel project="terrarium" /></section><section><MailPanel project="terrarium" /></section>
  </>} /></MemoryRouter>, root);
  const sections = root.querySelectorAll("section");
  const row = () => [...sections[0]!.querySelectorAll<HTMLTableRowElement>("tr.log-row")].find((r) => r.querySelector(".subject")?.textContent === "Panel retention")!;
  await waitFor(() => expect(row()).toBeTruthy());
  expect(root.querySelector('[aria-label="Filter by project"]')).toBeNull();
  expect(root.querySelector(".log-thread")).toBeNull();
  const names = [...root.querySelectorAll("datalist")].map((el) => el.id);
  expect(new Set(names).size).toBe(2);
  row().click();
  await waitFor(() => expect(sections[0]!.querySelector('[aria-label="Reply"]')).toBeTruthy());
  expect(sections[1]!.querySelector('[aria-label="Reply"]')).toBeNull();
  const reply = sections[0]!.querySelector<HTMLTextAreaElement>('[aria-label="Reply"]')!;
  reply.value = "unsaved reply"; reply.dispatchEvent(new Event("input", { bubbles: true })); reply.focus();
  const originalRow = row();
  // Exercise the production keeper on the document, with deterministic layout
  // for jsdom. Only the first panel is in the viewport in this fixture.
  const nativeRect = Element.prototype.getBoundingClientRect;
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this.closest("section") === sections[0] && this.matches(".mail-log tbody, .log-row, .log-thread-row")) {
      const index = this.matches("tbody") ? 0 : [...this.parentElement!.children].indexOf(this);
      return new DOMRect(0, index * 40 - document.documentElement.scrollTop, 500, 40);
    }
    return nativeRect.call(this);
  });
  const scrollBy = vi.spyOn(window, "scrollBy").mockImplementation((options, y) => {
    document.documentElement.scrollTop += typeof options === "number" ? y ?? 0 : options?.top ?? 0;
  });
  document.documentElement.scrollTop = 50;
  window.dispatchEvent(new Event("scroll"));
  const top = originalRow.getBoundingClientRect().top;
  const indexBefore = [...originalRow.parentElement!.children].indexOf(originalRow);
  await api.mail.reply(message.id, "new response");
  const list = vi.spyOn(api.mail, "list");
  for (const fn of listeners) fn({ id: "e", seq: 1, at: "2026-11-13T00:00:00Z", kind: "mail", project: "terrarium", data: { thread_id: message.thread_id }, summary: "reply" });
  await waitFor(() => expect(list).toHaveBeenCalled());
  await waitFor(() => expect(sections[0]!.textContent).toContain("new response"));
  expect(row()).toBe(originalRow);
  expect(sections[0]!.querySelector('[aria-label="Reply"]')).toBe(reply);
  expect(reply.value).toBe("unsaved reply");
  expect(document.activeElement).toBe(reply);
  await waitFor(() => expect(scrollBy).toHaveBeenCalled());
  const insertedHeight = ([...originalRow.parentElement!.children].indexOf(originalRow) - indexBefore) * 40;
  expect(insertedHeight).toBeGreaterThan(0);
  expect(document.documentElement.scrollTop).toBe(50 + insertedHeight);
  expect(originalRow.getBoundingClientRect().top).toBe(top);
  const search = sections[1]!.querySelector<HTMLInputElement>('[aria-label="Search"]')!;
  search.value = "nothing matches"; search.dispatchEvent(new Event("input", { bubbles: true }));
  expect(sections[0]!.querySelector<HTMLInputElement>('[aria-label="Search"]')!.value).toBe("");
  expect(history.get()).toBe("/host?open=foreign&q=foreign");
  expect(listeners.size).toBe(2);
  dispose!(); dispose = undefined;
  expect(listeners.size).toBe(0);
});

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
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); });

it("keeps duplicate filters and open rows local and retains a reply draft through refresh", async () => {
  const message = await api.mail.send({ to: "terrarium/lead", subject: "Panel retention", body: "hello" });
  const history = createMemoryHistory(); history.set({ value: "/host?open=foreign&q=foreign" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><MailPanel project="terrarium" /></section><section><MailPanel project="terrarium" /></section>
  </>} /></MemoryRouter>, root);
  const sections = root.querySelectorAll("section");
  const row = () => [...sections[0]!.querySelectorAll<HTMLTableRowElement>("tr.log-row")].find((r) => r.textContent?.includes("Panel retention"))!;
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
  const originalRow = row(); root.scrollTop = 51;
  await api.mail.reply(message.id, "new response");
  const list = vi.spyOn(api.mail, "list");
  for (const fn of listeners) fn({ id: "e", seq: 1, at: "2026-11-13T00:00:00Z", kind: "mail", project: "terrarium", data: { thread_id: message.thread_id }, summary: "reply" });
  await waitFor(() => expect(list).toHaveBeenCalled());
  await waitFor(() => expect(sections[0]!.textContent).toContain("new response"));
  expect(row()).toBe(originalRow);
  expect(sections[0]!.querySelector('[aria-label="Reply"]')).toBe(reply);
  expect(reply.value).toBe("unsaved reply");
  expect(document.activeElement).toBe(reply);
  expect(root.scrollTop).toBe(51);
  const search = sections[1]!.querySelector<HTMLInputElement>('[aria-label="Search"]')!;
  search.value = "nothing matches"; search.dispatchEvent(new Event("input", { bubbles: true }));
  expect(sections[0]!.querySelector<HTMLInputElement>('[aria-label="Search"]')!.value).toBe("");
  expect(history.get()).toBe("/host?open=foreign&q=foreign");
  expect(listeners.size).toBe(2);
  dispose!(); dispose = undefined;
  expect(listeners.size).toBe(0);
});

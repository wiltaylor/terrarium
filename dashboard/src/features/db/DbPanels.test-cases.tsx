// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { AgentEvent } from "../../api/types";
import { waitFor } from "../../test/wait";

const listeners = new Set<(event: AgentEvent) => void>();
const type = vi.fn(async (project: string, name: string) => ({ project, name, fields: [], records: 0, description: "", updated_at: "2026-11-13T00:00:00Z" }));
const types = vi.fn(async (project: string) => [await type(project, "items")]);
const record = vi.fn(async (project: string, id: string) => ({ project, id, type: "items", data: {}, revision: 1, updated_at: "2026-11-13T00:00:00Z" }));
const query = vi.fn(async (project: string, q: string) => ({ type: "items", query: q, view: "table", fields: [], records: [await record(project, "REC-1"), await record(project, "REC-2")], count: 2, labels: {} }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { db: { type, types, query, record } }, events: {
    on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
  } });
});
const { DbRecordsPanel } = await import("./DbRecordsPage");
const { DbTypesPanel } = await import("./DbTypesPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); document.documentElement.scrollTop = 0; vi.restoreAllMocks(); vi.clearAllMocks(); });

it("requires a local type, isolates drafts, retains focused inputs on a DB event and cleans up", async () => {
  const [project, setProject] = createSignal("alpha");
  const history = createMemoryHistory(); history.set({ value: "/host?q=foreign&id=foreign" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><DbRecordsPanel project={project()} /></section><section><DbRecordsPanel project="beta" /></section>
  </>} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelectorAll('option[value="items"]')).toHaveLength(2));
  expect(query).not.toHaveBeenCalled();
  const selects = root.querySelectorAll<HTMLSelectElement>('[aria-label="Record type"]');
  for (const select of selects) { select.value = "items"; select.dispatchEvent(new Event("change", { bubbles: true })); }
  await waitFor(() => expect(query).toHaveBeenCalledTimes(2));
  await waitFor(() => expect(root.querySelectorAll(".db-table tbody tr")).toHaveLength(4));
  const inputs = root.querySelectorAll<HTMLInputElement>('[aria-label="Query"]');
  inputs[0]!.value = "type:items unsaved"; inputs[0]!.dispatchEvent(new Event("input", { bubbles: true })); inputs[0]!.focus();
  const row = root.querySelectorAll(".db-table tbody tr")[1]!;
  // DB has no scroll keeper: a same-size refresh must retain the row and the
  // document offset rather than replacing the table or scrolling to the top.
  const nativeRect = Element.prototype.getBoundingClientRect;
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this.matches(".db-table tbody tr")) return new DOMRect(0, [...this.parentElement!.children].indexOf(this) * 40 - document.documentElement.scrollTop, 500, 40);
    return nativeRect.call(this);
  });
  document.documentElement.scrollTop = 50;
  window.dispatchEvent(new Event("scroll"));
  const top = row.getBoundingClientRect().top;
  expect(inputs[1]!.value).toBe("type:items");
  for (const fn of listeners) fn({ id: "e", seq: 1, at: "2026-11-13T00:00:00Z", kind: "db.record", project: "alpha", data: { type: "items" }, summary: "changed" });
  await waitFor(() => expect(query).toHaveBeenCalledTimes(3));
  expect(root.querySelector('[aria-label="Query"]')).toBe(inputs[0]);
  expect(inputs[0]!.value).toBe("type:items unsaved");
  expect(document.activeElement).toBe(inputs[0]);
  expect(root.querySelectorAll(".db-table tbody tr")[1]).toBe(row);
  expect(document.documentElement.scrollTop).toBe(50);
  expect(row.getBoundingClientRect().top).toBe(top);
  expect(history.get()).toBe("/host?q=foreign&id=foreign");
  setProject("gamma");
  await waitFor(() => expect(types).toHaveBeenCalledWith("gamma"));
  expect(root.querySelectorAll('[aria-label="Query"]')).toHaveLength(1);
  expect(listeners.size).toBe(1);
  dispose!(); dispose = undefined;
  expect(listeners.size).toBe(0);
});

it("opening and closing a record preserves the applied query, draft and request count", async () => {
  const history = createMemoryHistory(); history.set({ value: "/host" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <DbRecordsPanel project="alpha" />} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelector('option[value="items"]')).toBeTruthy());
  const select = root.querySelector<HTMLSelectElement>('[aria-label="Record type"]')!;
  select.value = "items"; select.dispatchEvent(new Event("change", { bubbles: true }));
  await waitFor(() => expect(root.querySelectorAll(".db-table tbody tr")).toHaveLength(2));
  const input = root.querySelector<HTMLInputElement>('[aria-label="Query"]')!;
  input.value = "type:items status=open"; input.dispatchEvent(new Event("input", { bubbles: true }));
  input.form!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await waitFor(() => expect(query).toHaveBeenLastCalledWith("alpha", "type:items status=open", { cursor: undefined }));
  input.value = "type:items unsaved"; input.dispatchEvent(new Event("input", { bubbles: true }));
  const calls = query.mock.calls.length;
  const row = root.querySelector<HTMLTableRowElement>(".db-table tbody tr")!;
  row.click();
  await waitFor(() => expect(document.querySelector('[role="dialog"]')).toBeTruthy());
  expect(input.value).toBe("type:items unsaved");
  expect(query).toHaveBeenCalledTimes(calls);
  document.querySelector<HTMLButtonElement>('[role="dialog"] [aria-label="Close"]')!.click();
  await waitFor(() => expect(document.querySelector('[role="dialog"]')).toBeNull());
  expect(input.value).toBe("type:items unsaved");
  expect(query).toHaveBeenCalledTimes(calls);
  expect(root.querySelector(".db-table tbody tr")).toBe(row);
  expect(root.querySelectorAll(".db-table tbody tr")).toHaveLength(2);
  // A later live refresh must still use the applied query, not the unsaved draft.
  for (const fn of listeners) fn({ id: "e", seq: 1, at: "2026-11-13T00:00:00Z", kind: "db.record", project: "alpha", data: { type: "items" }, summary: "changed" });
  await waitFor(() => expect(query).toHaveBeenCalledTimes(calls + 1));
  expect(query).toHaveBeenLastCalledWith("alpha", "type:items status=open", { cursor: undefined });
  expect(input.value).toBe("type:items unsaved");
});

it("keeps type links project-local and removes the type subscription", async () => {
  const history = createMemoryHistory(); history.set({ value: "/host" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <DbTypesPanel project="alpha" />} /></MemoryRouter>, root);
  await waitFor(() => expect(root.querySelector('a[href="/p/alpha/knowledge/db/items"]')).toBeTruthy());
  expect(listeners.size).toBe(1);
  dispose!(); dispose = undefined;
  expect(listeners.size).toBe(0);
});

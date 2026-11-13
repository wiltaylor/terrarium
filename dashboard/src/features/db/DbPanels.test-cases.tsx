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
const query = vi.fn(async (_project: string, q: string) => ({ type: "items", query: q, view: "table", fields: [], records: [], count: 0, labels: {} }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { db: { type, types, query } }, events: {
    on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
  } });
});
const { DbRecordsPanel } = await import("./DbRecordsPage");
const { DbTypesPanel } = await import("./DbTypesPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); vi.clearAllMocks(); });

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
  const inputs = root.querySelectorAll<HTMLInputElement>('[aria-label="Query"]');
  inputs[0]!.value = "type:items unsaved"; inputs[0]!.dispatchEvent(new Event("input", { bubbles: true })); inputs[0]!.focus();
  root.scrollTop = 37;
  expect(inputs[1]!.value).toBe("type:items");
  for (const fn of listeners) fn({ id: "e", seq: 1, at: "2026-11-13T00:00:00Z", kind: "db.record", project: "alpha", data: { type: "items" }, summary: "changed" });
  await waitFor(() => expect(query).toHaveBeenCalledTimes(3));
  expect(root.querySelector('[aria-label="Query"]')).toBe(inputs[0]);
  expect(inputs[0]!.value).toBe("type:items unsaved");
  expect(document.activeElement).toBe(inputs[0]);
  expect(root.scrollTop).toBe(37);
  expect(history.get()).toBe("/host?q=foreign&id=foreign");
  setProject("gamma");
  await waitFor(() => expect(types).toHaveBeenCalledWith("gamma"));
  expect(root.querySelectorAll('[aria-label="Query"]')).toHaveLength(1);
  expect(listeners.size).toBe(1);
  dispose!(); dispose = undefined;
  expect(listeners.size).toBe(0);
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

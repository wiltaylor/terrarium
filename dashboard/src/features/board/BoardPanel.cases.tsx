// @vitest-environment jsdom
import { createSignal, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent, Board } from "../../api/types";
import { waitFor } from "../../test/wait";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../instance/navigation", () => ({ A: (p: JSX.AnchorHTMLAttributes<HTMLAnchorElement>) => <a {...p} />, useNavigate: () => vi.fn() }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({
    api, events: { on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } },
    agents: { inProject: () => [], byId: () => undefined },
    inbox: { refresh: async () => {} }, projects: { projects: () => [] },
    cardPeek: { open: () => {}, close: () => {} },
  });
});
const { BoardPanel } = await import("./BoardPanel");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.useRealTimers(); });
function host() { const el = document.createElement("div"); document.body.append(el); return el; }

it("keeps duplicate queries, filters and new-card drafts independent with stable unique IDs", async () => {
  const el = host();
  const first = vi.fn();
  const second = vi.fn();
  const [query, setQuery] = createSignal({});
  dispose = render(() => <>
    <section data-panel="a"><BoardPanel project="terrarium" query={query()} onQueryChange={first} onToggleDone={() => {}} /></section>
    <section data-panel="b"><BoardPanel project="terrarium" query={{}} onQueryChange={second} onToggleDone={() => {}} /></section>
  </>, el);
  await waitFor(() => expect(el.querySelectorAll(".column-add").length).toBeGreaterThan(1));
  const panels = el.querySelectorAll<HTMLElement>("[data-panel]");
  const ids = [...el.querySelectorAll(".board-filters")].map(e => e.id);
  expect(new Set(ids).size).toBe(2);
  const group = panels[0]!.querySelector<HTMLInputElement>(".board-group input")!;
  group.click();
  expect(first).toHaveBeenCalledWith({ group: "version" }, undefined);
  expect(second).not.toHaveBeenCalled();
  setQuery({ group: "version" });
  expect([...el.querySelectorAll(".board-filters")].map(e => e.id)).toEqual(ids);
  panels[0]!.querySelector<HTMLButtonElement>(".column-add")!.click();
  const title = document.querySelector<HTMLInputElement>(".new-card input")!;
  title.value = "Only first";
  title.dispatchEvent(new Event("input", { bubbles: true }));
  expect(panels[1]!.querySelector(".new-card")).toBeNull();
  expect(title.value).toBe("Only first");
});

it("discards old-project board replies and removes subscriptions", async () => {
  const el = host();
  const board = await api.board.get("terrarium");
  let resolve!: (b: Board) => void;
  vi.spyOn(api.board, "get").mockImplementation(p => p === "old" ? new Promise(r => { resolve = r; }) : Promise.resolve({ ...board, project: p, cards: [] }));
  const [project, setProject] = createSignal("old");
  dispose = render(() => <BoardPanel project={project()} query={{}} onQueryChange={() => {}} onToggleDone={() => {}} />, el);
  await waitFor(() => expect(resolve).toBeTypeOf("function"));
  setProject("new");
  await waitFor(() => expect(el.querySelector(".column-add")).not.toBeNull());
  resolve(board);
  await Promise.resolve();
  expect(el.querySelector("[data-card]")).toBeNull();
  expect(listeners.size).toBe(1);
  dispose(); dispose = undefined;
  expect(listeners.size).toBe(0);
});

it("defers live reload during drag and cancels pending work at unmount", async () => {
  const el = host();
  const get = vi.spyOn(api.board, "get");
  dispose = render(() => <BoardPanel project="terrarium" query={{}} onQueryChange={() => {}} onToggleDone={() => {}} />, el);
  await waitFor(() => expect(el.querySelector("[data-card]")).not.toBeNull());
  vi.useFakeTimers();
  const card = el.querySelector<HTMLElement>("[data-card]")!;
  const drag = new Event("dragstart", { bubbles: true });
  Object.defineProperty(drag, "dataTransfer", { value: { setData: () => {}, effectAllowed: "" } });
  card.dispatchEvent(drag);
  get.mockClear();
  const event = { kind: "board", project: "terrarium", data: {} } as AgentEvent;
  listeners.forEach(fn => fn(event));
  await vi.advanceTimersByTimeAsync(5000);
  expect(get).not.toHaveBeenCalled();
  card.dispatchEvent(new Event("dragend", { bubbles: true }));
  await vi.advanceTimersByTimeAsync(5000);
  expect(get).toHaveBeenCalledTimes(1);
  get.mockClear();
  listeners.forEach(fn => fn(event));
  dispose(); dispose = undefined;
  await vi.advanceTimersByTimeAsync(5000);
  expect(get).not.toHaveBeenCalled();
  expect(listeners.size).toBe(0);
});

it("puts a column cog on each column head, also in the read-only page widget (TER-1116)", async () => {
  const el = host();
  dispose = render(() => <BoardPanel project="terrarium" query={{}} onQueryChange={() => {}} onToggleDone={() => {}} canWriteCards={false} canEditColumns={false} />, el);
  const board = await api.board.get("terrarium");
  await waitFor(() => expect(el.querySelectorAll("[data-column-head] [data-column-cog]").length).toBeGreaterThan(0));
  const cogs = [...el.querySelectorAll<HTMLElement>("[data-column-head] [data-column-cog]")].map(b => b.dataset.columnCog);
  expect(cogs).toEqual(board.columns.map(c => c.name));
  el.querySelector<HTMLButtonElement>('[data-column-head="review"] [data-column-cog]')!.click();
  await waitFor(() => expect(document.querySelector('[role="dialog"] .column-options')).not.toBeNull());
});

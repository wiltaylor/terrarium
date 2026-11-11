// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createMockApi } from "../../api/mock";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: import("../../api/types").AgentEvent) => void>();
const onEvent = (e: import("../../api/types").AgentEvent) => listeners.forEach(fn => fn(e));
vi.mock("../../stores", () => ({
  api,
  events: { on: (fn: typeof onEvent) => { listeners.add(fn); return () => listeners.delete(fn); } },
  inbox: { refresh: async () => {} },
  agents: { inProject: () => [], byId: () => undefined },
  // The card body and comments link card keys (TER-571).
  projects: { projects: () => [] },
  cardPeek: { open: () => {}, close: () => {} },
}));
const { BoardPage } = await import("./BoardPage");
const { loadBoards } = await import("../inbox/TrackedCards");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; vi.restoreAllMocks(); });

it("fetches only missing linked Done cards for Inbox progress", async () => {
  const done = await api.cards.create("terrarium", { title: "Linked completion", column: "done" });
  const get = vi.spyOn(api.cards, "get");
  const boards = await loadBoards(["terrarium"], [
    { project: "terrarium", card: done.id, linked_by: "operator", linked_at: "" },
    { project: "terrarium", card: "TER-12", linked_by: "operator", linked_at: "" },
  ]);
  expect(get).toHaveBeenCalledExactlyOnceWith(done.id);
  expect(boards.terrarium!.cards.find(c => c.id === done.id)?.column).toBe("done");
  expect(boards.terrarium!.cards.some(c => c.id === "TER-11")).toBe(false);
});

it("opens full details for a Done card absent from the active board", async () => {
  const done = await api.cards.create("terrarium", { title: "Completed regression", column: "done", body: "Full detail only" });
  await api.cards.comment(done.id, "Full comment only");
  const get = vi.spyOn(api.cards, "get");
  const history = createMemoryHistory();
  history.set({ value: `/p/terrarium/board?card=${done.id}` });
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => createComponent(MemoryRouter, {
    history,
    get children() { return createComponent(Route, { path: "/p/:project/board", component: BoardPage }); },
  }), host);
  await waitFor(() => expect(get).toHaveBeenCalledWith(done.id));
  await waitFor(() => expect(document.body.textContent).toContain("Full comment only"));
  expect(document.body.textContent).toContain("Full detail only");
  expect((await api.board.get("terrarium")).cards.some(c => c.id === done.id)).toBe(false);
  get.mockClear();
  const event = { kind: "card", project: "terrarium", card: "TER-other", data: {} } as import("../../api/types").AgentEvent;
  onEvent(event);
  await new Promise(resolve => setTimeout(resolve, 400));
  expect(get).not.toHaveBeenCalled();
  onEvent({ ...event, card: done.id });
  onEvent(event);
  await waitFor(() => expect(get).toHaveBeenCalledExactlyOnceWith(done.id));
});

it("does not fetch full cards when the board has no open drawer", async () => {
  const get = vi.spyOn(api.cards, "get");
  const history = createMemoryHistory();
  history.set({ value: "/p/terrarium/board" });
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => createComponent(MemoryRouter, {
    history,
    get children() { return createComponent(Route, { path: "/p/:project/board", component: BoardPage }); },
  }), host);
  await waitFor(() => expect(host.querySelectorAll("[data-card]").length).toBeGreaterThan(0));
  expect(get).not.toHaveBeenCalled();
  const chip = host.querySelector("[data-card='TER-12'] .test-chip");
  expect(chip?.getAttribute("title")).toContain("Latest test run");
  expect(chip?.getAttribute("href")).toContain("/p/terrarium/tests/");
});

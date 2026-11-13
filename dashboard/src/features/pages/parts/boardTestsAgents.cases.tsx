// @vitest-environment jsdom
import { createSignal, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../../api/mock";
import { ApiError } from "../../../api/http";
import type { Agent, AgentEvent, Board, PageWidget, TestRunListQuery } from "../../../api/types";
import { waitFor } from "../../../test/wait";
import { PROJECT_WIDGET_VIEWS } from "../widgetRegistry";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
const agent = (id: string, extra: Partial<Agent> = {}): Agent => ({
  id, name: id, project: "terrarium", harness: "codex", status: "stopped", paused: false, roles: [], ...extra,
  workspace: "/workspace", created_at: "2026-11-12T00:00:00Z",
});
const ordinary = [agent("paused", { paused: true }), agent("working", { status: "working" })];
const { navigate } = vi.hoisted(() => ({ navigate: vi.fn() }));
vi.mock("../../../instance/navigation", () => ({ A: (p: JSX.AnchorHTMLAttributes<HTMLAnchorElement>) => <a {...p} />, useNavigate: () => navigate }));
vi.mock("../../../components/Leases", () => ({ LeaseChips: () => null }));
vi.mock("../../../components/HoldBadge", () => ({ HoldBadge: () => null }));
vi.mock("../../../stores", async () => {
  const { instanceTestStores } = await import("../../../test/instance");
  return instanceTestStores({
    api, events: { on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } },
    agents: { inProject: (p: string) => ordinary.filter(a => a.project === p), byId: () => undefined, state: { loading: false, error: null }, refresh: async () => {} },
    leases: { loadAll: async () => {} }, pins: { isPinned: () => false, toggle: async () => {} },
    inbox: { refresh: async () => {} }, projects: { projects: () => [] },
    cardPeek: { open: () => {}, close: () => {} },
  });
});
const { CardAgentsPanel } = await import("../../agents/AgentsPanel");
const { AGENTS_PARTS, BOARD_PARTS, BOARD_TESTS_AGENTS_PARTS, TESTS_PARTS, boardTestsAgentsPart } = await import("./boardTestsAgents");

let dispose: (() => void) | undefined;
const startUrl = "/p/terrarium/board?card=TER-1&col=review";
beforeEach(() => window.history.replaceState(null, "", startUrl));
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); vi.useRealTimers(); listeners.clear(); navigate.mockClear(); });
const admin = { canWriteCards: true, canEditColumns: true, canCreateAgent: true, canPinAgents: true };
const buttonText = (el: Element, text: string) => [...el.querySelectorAll("button")].find(b => b.textContent?.trim() === text);
function host() { const el = document.createElement("div"); document.body.append(el); return el; }
const widget = <K extends PageWidget["kind"]>(id: string, kind: K, options?: PageWidget["options"]) => ({ id, kind, options });
const emit = (event: Partial<AgentEvent>) => listeners.forEach(fn => fn({ data: {}, ...event } as AgentEvent));
function change(el: HTMLInputElement | HTMLSelectElement, value: string, event = "change") {
  el.value = value; el.dispatchEvent(new Event(event, { bubbles: true }));
}

it("has one renderer for each registry view of Board, Tests and Agents, and none for an unknown view", () => {
  for (const kind of ["board", "tests", "agents"] as const) {
    expect(Object.keys(BOARD_TESTS_AGENTS_PARTS[kind]).sort()).toEqual([...PROJECT_WIDGET_VIEWS[kind]].sort());
    for (const view of PROJECT_WIDGET_VIEWS[kind]) expect(boardTestsAgentsPart(kind, { view })).toBe((BOARD_TESTS_AGENTS_PARTS[kind] as Record<string, unknown>)[view]);
  }
  expect(boardTestsAgentsPart("board", undefined)).toBe(BOARD_PARTS.lanes);
  expect(boardTestsAgentsPart("agents", {})).toBe(AGENTS_PARTS.agents);
  expect(boardTestsAgentsPart("board", { view: "runs" as never })).toBeUndefined();
});

it("keeps duplicate lanes widgets independent of each other and of the URL, also on a phone", async () => {
  vi.stubGlobal("matchMedia", (query: string) => ({ matches: true, media: query, addEventListener: () => {}, removeEventListener: () => {} }));
  window.requestAnimationFrame ??= (fn => setTimeout(fn, 0)) as typeof requestAnimationFrame;
  window.cancelAnimationFrame ??= (id => clearTimeout(id)) as typeof cancelAnimationFrame;
  // jsdom has no element scrolling. The phone board scrolls its column chips.
  const scrollTo = vi.fn();
  Object.defineProperty(HTMLElement.prototype, "scrollTo", { value: scrollTo, configurable: true });
  const el = host();
  const Lanes = BOARD_PARTS.lanes;
  dispose = render(() => <>
    <section data-w="a"><Lanes project="terrarium" widget={widget("a", "board")} /></section>
    <section data-w="b"><Lanes project="terrarium" widget={widget("b", "board")} /></section>
  </>, el);
  await waitFor(() => expect(el.querySelectorAll(".board-colnav button").length).toBeGreaterThan(2));
  const [a, b] = [...el.querySelectorAll<HTMLElement>("[data-w]")];
  const chips = (s: HTMLElement) => [...s.querySelectorAll<HTMLButtonElement>(".board-colnav button")];
  const pressed = (s: HTMLElement) => chips(s).filter(c => c.getAttribute("aria-pressed") === "true").map(c => c.dataset.column);
  // The URL ?col=review and ?card=TER-1 belong to the route page, not to a widget.
  const before = pressed(b!);
  const target = chips(a!).find(c => !before.includes(c.dataset.column))!;
  target.click();
  await waitFor(() => expect(pressed(a!)).toEqual([target.dataset.column]));
  expect(pressed(b!)).toEqual(before);
  expect(new Set([...el.querySelectorAll(".board-filters")].map(e => e.id)).size).toBe(2);
  expect(document.querySelector(".drawer")).toBeNull();
  expect(window.location.pathname + window.location.search).toBe(startUrl);
  dispose(); dispose = undefined;
  await new Promise(r => setTimeout(r, 20));
  delete (HTMLElement.prototype as { scrollTo?: unknown }).scrollTo;
  vi.unstubAllGlobals();
});

it("toggles a done widget locally without navigation", async () => {
  const el = host();
  const Done = BOARD_PARTS.done;
  dispose = render(() => <Done project="terrarium" widget={widget("d", "board", { view: "done" })} />, el);
  await waitFor(() => expect(el.querySelector("h2")?.textContent).toBe("Done cards"));
  [...el.querySelectorAll("button")].find(b => b.textContent === "Back to cards")!.click();
  await waitFor(() => expect(el.querySelector("h2")?.textContent).toBe("Cards"));
  expect(window.location.pathname + window.location.search).toBe(startUrl);
});

it("keeps the columns draft over remote board events, ignores other projects and cleans up", async () => {
  const board = await api.board.get("terrarium");
  const changed: Board = { ...board, columns: board.columns.map((c, i) => i === 1 ? { ...c, title: "Renamed elsewhere" } : c) };
  const get = vi.spyOn(api.board, "get").mockResolvedValueOnce(board).mockResolvedValue(changed);
  const el = host();
  const Columns = BOARD_PARTS.columns;
  dispose = render(() => <Columns project="terrarium" widget={widget("c", "board", { view: "columns" })} grants={admin} />, el);
  await waitFor(() => expect(el.querySelectorAll(".columns-editor-row").length).toBe(board.columns.length));
  expect(el.querySelector(".modal")).toBeNull();
  const title = el.querySelectorAll<HTMLInputElement>(".columns-editor-row")[1]!.querySelectorAll("input")[1]!;
  change(title, "My draft", "input");
  vi.useFakeTimers();
  emit({ kind: "board", project: "other" });
  await vi.advanceTimersByTimeAsync(1000);
  expect(get).toHaveBeenCalledTimes(1);
  emit({ kind: "board", project: "terrarium" });
  emit({ kind: "card", project: "terrarium" });
  await vi.advanceTimersByTimeAsync(1000);
  expect(get).toHaveBeenCalledTimes(2);
  vi.useRealTimers();
  await waitFor(() => expect(el.querySelector("[role=status]")?.textContent).toContain("changed on the server"));
  expect(el.querySelectorAll<HTMLInputElement>(".columns-editor-row")[1]!.querySelectorAll("input")[1]!.value).toBe("My draft");
  // Cancel loads the server columns.
  [...el.querySelectorAll("button")].find(b => b.textContent?.trim() === "Cancel")!.click();
  await waitFor(() => expect(el.querySelectorAll<HTMLInputElement>(".columns-editor-row")[1]!.querySelectorAll("input")[1]!.value).toBe("Renamed elsewhere"));
  expect(el.querySelector("[role=status]")).toBeNull();
  vi.useFakeTimers();
  emit({ kind: "board", project: "terrarium" });
  dispose(); dispose = undefined;
  await vi.advanceTimersByTimeAsync(1000);
  expect(get).toHaveBeenCalledTimes(2);
  expect(listeners.size).toBe(0);
});

it("shows a refused columns save and keeps the draft, and remounts after a saved change", async () => {
  const board = await api.board.get("terrarium");
  vi.spyOn(api.board, "get").mockResolvedValue(board);
  const update = vi.spyOn(api.board, "update").mockRejectedValueOnce(new ApiError(403, "project_admin required", undefined, "forbidden"));
  const el = host();
  const Columns = BOARD_PARTS.columns;
  dispose = render(() => <Columns project="terrarium" widget={widget("c", "board", { view: "columns" })} grants={admin} />, el);
  await waitFor(() => expect(el.querySelectorAll(".columns-editor-row").length).toBe(board.columns.length));
  const titleInput = () => el.querySelectorAll<HTMLInputElement>(".columns-editor-row")[1]!.querySelectorAll("input")[1]!;
  change(titleInput(), "Draft title", "input");
  el.querySelector<HTMLFormElement>("form.columns-editor")!.requestSubmit();
  await waitFor(() => expect(el.textContent).toContain("project_admin required"));
  expect(titleInput().value).toBe("Draft title");
  const saved: Board = { ...board, columns: board.columns.map((c, i) => i === 1 ? { ...c, title: "Draft title" } : c) };
  update.mockResolvedValueOnce(saved);
  el.querySelector<HTMLFormElement>("form.columns-editor")!.requestSubmit();
  await waitFor(() => expect(update).toHaveBeenCalledTimes(2));
  await waitFor(() => expect(el.textContent).not.toContain("project_admin required"));
  expect(titleInput().value).toBe("Draft title");
  expect(update.mock.calls[1]![0]).toBe("terrarium");
});

it("seeds the test runs name from the saved search and keeps duplicate filters local", async () => {
  const list = vi.spyOn(api.testRuns, "list").mockResolvedValue({ items: [], next: null });
  const el = host();
  const Runs = TESTS_PARTS.runs;
  dispose = render(() => <>
    <section data-w="a"><Runs project="terrarium" widget={widget("a", "tests", { view: "runs", search: " unit " })} /></section>
    <section data-w="b"><Runs project="terrarium" widget={widget("b", "tests")} /></section>
  </>, el);
  await waitFor(() => expect(list).toHaveBeenCalledTimes(2));
  const queries = () => list.mock.calls.map(c => c[1] as TestRunListQuery);
  expect(queries().map(q => q.name).sort()).toEqual(["unit", undefined]);
  const [a, b] = [...el.querySelectorAll<HTMLElement>("[data-w]")];
  expect(a!.querySelector<HTMLInputElement>("[aria-label='Run name']")!.value).toBe("unit");
  expect(b!.querySelector<HTMLInputElement>("[aria-label='Run name']")!.value).toBe("");
  list.mockClear();
  change(b!.querySelector("select")!, "failed");
  await waitFor(() => expect(list).toHaveBeenCalledTimes(1));
  expect(queries()[0]).toMatchObject({ status: "failed", name: undefined });
  expect(a!.querySelector("select")!.value).toBe("");
  // A name draft in one widget stays there until Enter or blur.
  change(a!.querySelector<HTMLInputElement>("[aria-label='Run name']")!, "lint", "input");
  expect(b!.querySelector<HTMLInputElement>("[aria-label='Run name']")!.value).toBe("");
  expect(window.location.pathname + window.location.search).toBe(startUrl);
});

it("renders the agents and card agents parts with local filters and an empty state", async () => {
  vi.spyOn(api.agents, "list").mockResolvedValue([]);
  const el = host();
  const Agents = AGENTS_PARTS.agents;
  const CardAgents = AGENTS_PARTS.card_agents;
  dispose = render(() => <>
    <section data-w="a"><Agents project="terrarium" widget={widget("a", "agents")} /></section>
    <section data-w="b"><Agents project="terrarium" widget={widget("b", "agents")} /></section>
    <section data-w="c"><CardAgents project="terrarium" widget={widget("c", "agents", { view: "card_agents" })} /></section>
  </>, el);
  const [a, b, c] = [...el.querySelectorAll<HTMLElement>("[data-w]")];
  change(a!.querySelector("select")!, "paused");
  expect(a!.querySelectorAll(".agent-card")).toHaveLength(1);
  expect(b!.querySelectorAll(".agent-card")).toHaveLength(2);
  await waitFor(() => expect(c!.textContent).toContain("No card agents."));
  expect(api.agents.list).toHaveBeenCalledWith("terrarium", { card: true });
});

it("defaults a board widget to read-only and opens a card on its detail route", async () => {
  const get = vi.spyOn(api.cards, "get");
  const update = vi.spyOn(api.cards, "update");
  const el = host();
  const Lanes = BOARD_PARTS.lanes;
  dispose = render(() => <Lanes project="terrarium" widget={widget("r", "board")} />, el);
  await waitFor(() => expect(el.querySelector("[data-card]")).not.toBeNull());
  expect(el.querySelector(".column-add")).toBeNull();
  expect(buttonText(el, "Edit columns")).toBeUndefined();
  expect([...el.querySelectorAll<HTMLElement>("[data-card]")].every(c => c.getAttribute("draggable") !== "true")).toBe(true);
  const card = el.querySelector<HTMLElement>("[data-card]")!;
  card.click();
  expect(navigate).toHaveBeenCalledWith(`/p/terrarium/board?card=${encodeURIComponent(card.dataset.card!)}`);
  await new Promise(r => setTimeout(r, 20));
  expect(document.querySelector(".drawer")).toBeNull();
  expect(get).not.toHaveBeenCalled();
  expect(update).not.toHaveBeenCalled();
});

it("shows board writes only with the grants, and a revoked grant removes them and closes the drawer", async () => {
  const el = host();
  const Lanes = BOARD_PARTS.lanes;
  const [grants, setGrants] = createSignal<typeof admin | { canWriteCards?: boolean; canEditColumns?: boolean }>({ canWriteCards: true });
  dispose = render(() => <Lanes project="terrarium" widget={widget("g", "board")} grants={grants()} />, el);
  await waitFor(() => expect(el.querySelector(".column-add")).not.toBeNull());
  expect(buttonText(el, "Edit columns")).toBeUndefined();
  expect(el.querySelector<HTMLElement>("[data-card][draggable=true]")).not.toBeNull();
  setGrants(admin);
  expect(buttonText(el, "Edit columns")).not.toBeUndefined();
  el.querySelector<HTMLElement>("[data-card]")!.click();
  await waitFor(() => expect(document.querySelector(".drawer")).not.toBeNull());
  expect(navigate).not.toHaveBeenCalled();
  setGrants({});
  await waitFor(() => expect(document.querySelector(".drawer")).toBeNull());
  expect(el.querySelector(".column-add")).toBeNull();
  expect(buttonText(el, "Edit columns")).toBeUndefined();
  expect(el.querySelector<HTMLElement>("[data-card][draggable=true]")).toBeNull();
});

it("shows read-only columns without a grant and the editor with one", async () => {
  const board = await api.board.get("terrarium");
  vi.spyOn(api.board, "get").mockResolvedValue(board);
  const update = vi.spyOn(api.board, "update");
  const el = host();
  const Columns = BOARD_PARTS.columns;
  const [grants, setGrants] = createSignal<{ canEditColumns?: boolean } | undefined>(undefined);
  dispose = render(() => <Columns project="terrarium" widget={widget("c", "board", { view: "columns" })} grants={grants()} />, el);
  await waitFor(() => expect(el.querySelectorAll("table [data-column-row]").length).toBe(board.columns.length));
  expect(el.querySelector("form, input, select, textarea")).toBeNull();
  expect(el.querySelector("button")).toBeNull();
  // A write right of another domain is not a columns grant.
  setGrants({ canEditColumns: false });
  expect(el.querySelector("form")).toBeNull();
  setGrants({ canEditColumns: true });
  await waitFor(() => expect(el.querySelectorAll(".columns-editor-row").length).toBe(board.columns.length));
  setGrants(undefined);
  expect(el.querySelector("form")).toBeNull();
  expect(update).not.toHaveBeenCalled();
});

it("shows a card agents load error on the Agents tab and in the widget", async () => {
  vi.spyOn(api.agents, "list").mockRejectedValue(new ApiError(500, "agents unavailable"));
  const el = host();
  const CardAgents = AGENTS_PARTS.card_agents;
  dispose = render(() => <>
    <section data-w="tab"><CardAgentsPanel project="terrarium" /></section>
    <section data-w="widget"><CardAgents project="terrarium" widget={widget("c", "agents", { view: "card_agents" })} /></section>
  </>, el);
  const [tab, part] = [...el.querySelectorAll<HTMLElement>("[data-w]")];
  await waitFor(() => expect(tab!.querySelector("[role=alert]")).not.toBeNull());
  await waitFor(() => expect(part!.querySelector("[role=alert]")).not.toBeNull());
  expect(part!.textContent).not.toContain("No card agents.");
});

it("hides New agent and pins without grants and shows them with grants", () => {
  const el = host();
  const Agents = AGENTS_PARTS.agents;
  const [grants, setGrants] = createSignal<{ canCreateAgent?: boolean; canPinAgents?: boolean } | undefined>(undefined);
  dispose = render(() => <Agents project="terrarium" widget={widget("a", "agents")} grants={grants()} />, el);
  expect(el.querySelectorAll(".agent-card")).toHaveLength(2);
  expect(buttonText(el, "New agent")).toBeUndefined();
  expect(buttonText(el, "Pin")).toBeUndefined();
  setGrants({ canCreateAgent: true });
  expect(buttonText(el, "New agent")).not.toBeUndefined();
  expect(buttonText(el, "Pin")).toBeUndefined();
  setGrants({ canPinAgents: true });
  expect(buttonText(el, "New agent")).toBeUndefined();
  expect(el.querySelectorAll("button[aria-pressed]")).toHaveLength(2);
});

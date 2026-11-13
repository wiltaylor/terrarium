// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { allHome, type HomeCard } from "../../api/home";

const home = { cards: vi.fn(), monitors: vi.fn(), pipelines: vi.fn() };
const open = vi.fn();
let refresh: (e: { kind: string }) => void;
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({ api: { home }, cardPeek: { open }, projects: { projects: () => [{ name: "one" }, { name: "two", home_board_columns: { in_progress: ["working"], review: ["review", "merge_queue"], accepted: [], blocked: ["blocked"] } }] }, events: { on: (fn: typeof refresh) => { refresh = fn; return () => {}; } } })); });
vi.mock("@solidjs/router", () => ({ A: (p: { children: unknown }) => p.children }));
const { HomeBoard } = await import("./HomeBoard");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.resetAllMocks(); vi.useRealTimers(); });
const settle = () => new Promise(r => setTimeout(r, 0));
function mount() {
  home.cards.mockResolvedValue({ items: [], next: null });
  const el = document.createElement("div"); document.body.append(el);
  dispose = render(() => createComponent(HomeBoard, { now: Date.now() }), el);
}
it("follows an empty continuation page", async () => {
  const page = vi.fn().mockResolvedValueOnce({ items: [], next: "after" }).mockResolvedValueOnce({ items: [1], next: null });
  expect(await allHome(page)).toEqual([1]);
  expect(page).toHaveBeenLastCalledWith("after");
});
it("does not announce nominal before both lists finish", async () => {
  let finish!: (v: { items: never[]; next: null }) => void;
  home.monitors.mockResolvedValue({ items: [], next: null });
  home.pipelines.mockReturnValue(new Promise(r => { finish = r; }));
  mount(); await settle();
  expect(document.body.textContent).not.toContain("All Systems Nominal");
  finish({ items: [], next: null }); await settle();
  expect(document.body.textContent).toContain("All Systems Nominal");
});
it.each([["same", "same"], ["a", "b", "a"]])("rejects repeated cursors %j", async (...cursors) => {
  const page = vi.fn();
  for (const next of cursors) page.mockResolvedValueOnce({ items: [], next });
  await expect(allHome(page)).rejects.toThrow("repeated cursor");
  expect(page).toHaveBeenCalledTimes(cursors.length);
});
it("stops after 500 pages with distinct continuation cursors", async () => {
  let calls = 0;
  const page = vi.fn(async () => ({ items: [], next: String(++calls) }));
  await expect(allHome(page)).rejects.toThrow("500-page limit");
  expect(page).toHaveBeenCalledTimes(500);
});
it("allows a terminal 500th page", async () => {
  let calls = 0;
  const page = vi.fn(async () => ({ items: [1], next: ++calls === 500 ? null : String(calls) }));
  expect(await allHome(page)).toHaveLength(500);
  expect(page).toHaveBeenCalledTimes(500);
});
it("shows pagination errors on the widget", async () => {
  home.monitors.mockResolvedValue({ items: [], next: "same" });
  home.pipelines.mockResolvedValue({ items: [], next: null });
  mount(); await settle();
  expect(document.body.textContent).toContain("repeated cursor");
  expect(document.body.textContent).not.toContain("All Systems Nominal");
  expect(home.monitors).toHaveBeenCalledTimes(2);
});
it("does not announce nominal on an error", async () => {
  home.monitors.mockRejectedValue(new Error("offline"));
  home.pipelines.mockResolvedValue({ items: [], next: null });
  mount(); await settle();
  expect(document.body.textContent).toContain("offline");
  expect(document.body.textContent).not.toContain("All Systems Nominal");
});
it("shows status widgets independently of layout membership", async () => {
  home.monitors.mockResolvedValue({ items: [{ id: "m", name: "No result", project: "one", status: "unknown", link: "/p/one/dashboard" }], next: null });
  home.pipelines.mockResolvedValue({ items: [{ id: "p", name: "Build", project: "two", status: "failed", link: "/p/two/pipelines" }], next: null });
  mount(); await settle();
  expect(document.body.textContent).toContain("No result");
  expect(document.body.textContent).toContain("Build");
  expect(document.body.textContent).toContain("unknown");
  expect(document.body.textContent).toContain("failed");
  expect(document.body.textContent).not.toContain("All Systems Nominal");
  expect(document.body.textContent).not.toContain("Needs attention");
});
it("keeps nominal visible during a timer refresh without a heading", async () => {
  home.monitors.mockResolvedValue({ items: [], next: null });
  home.pipelines.mockResolvedValue({ items: [], next: null });
  mount(); await settle();
  const nominal = document.querySelector(".nominal");
  home.monitors.mockReturnValue(new Promise(() => {}));
  vi.useFakeTimers(); await vi.advanceTimersByTimeAsync(60_000);
  expect(document.querySelector(".nominal")).toBe(nominal);
  expect(document.body.textContent).not.toContain("Needs attention");
  expect(document.body.textContent).not.toContain("Loading");
});
it("retains and orders tiles and widgets through paginated reloads, replacing only a changed tile", async () => {
  const a = { id: "ONE-1", project: "one", title: "First", labels: [], home_column: "in_progress" };
  const b = { ...a, id: "ONE-2", title: "Second" };
  const m = { id: "m", project: "one", name: "Monitor", status: "warn", link: "/" };
  home.monitors.mockResolvedValue({ items: [m], next: null });
  home.pipelines.mockResolvedValue({ items: [], next: null });
  mount();
  home.cards.mockImplementation(async q => ({ items: q.column === "in_progress" ? [b, a] : [], next: null }));
  await settle();
  vi.useFakeTimers();
  refresh({ kind: "card" }); await vi.advanceTimersByTimeAsync(2000);
  const tiles = [...document.querySelectorAll(".home-card")];
  const widget = document.querySelector(".home-status");
  expect(tiles.map(t => t.textContent)).toEqual([expect.stringContaining("First"), expect.stringContaining("Second")]);
  const mutations: MutationRecord[] = [];
  const observer = new MutationObserver(records => mutations.push(...records));
  observer.observe(document.querySelector(".home-kanban")!, { childList: true, subtree: true });
  let finish!: (v: unknown) => void;
  home.cards.mockImplementation(async q => q.column !== "in_progress" ? { items: [], next: null } : q.cursor ? new Promise(r => { finish = r; }) : { items: [{ ...a }], next: "page2" });
  home.monitors.mockResolvedValue({ items: [{ ...m }], next: null });
  refresh({ kind: "card" }); await vi.advanceTimersByTimeAsync(2000);
  expect([...document.querySelectorAll(".home-card")]).toEqual(tiles);
  finish({ items: [{ ...b }], next: null }); await vi.advanceTimersByTimeAsync(0);
  expect([...document.querySelectorAll(".home-card")]).toEqual(tiles);
  expect(document.querySelector(".home-status")).toBe(widget);
  expect(mutations.flatMap(m => [...m.removedNodes, ...m.addedNodes]).filter(n => n instanceof Element && n.matches(".home-card"))).toHaveLength(0);
  home.cards.mockImplementation(async q => ({ items: q.column === "in_progress" ? [{ ...b, title: "Changed" }, { ...a }] : [], next: null }));
  refresh({ kind: "card" }); await vi.advanceTimersByTimeAsync(2000);
  expect(document.querySelectorAll(".home-card")[0]).toBe(tiles[0]);
  expect(document.querySelectorAll(".home-card")[1]).not.toBe(tiles[1]);
  expect(document.querySelector(".home-status")).toBe(widget);
  observer.disconnect();
});
it("shows labels, project, agent, ETA and blocked reason, and opens the sidebar", async () => {
  home.monitors.mockResolvedValue({ items: [], next: null }); home.pipelines.mockResolvedValue({ items: [], next: null });
  mount();
  const card: HomeCard = { id: "ONE-1", title: "Fix", project: "one", column: "blocked", home_column: "blocked", labels: ["type:bug", "feature"], assignee: "a", assignee_name: "worker", assignee_status: "blocked", eta_at: new Date(Date.now() - 60_000).toISOString(), blocked_reason: "Need access\nmore" };
  home.cards.mockImplementation(async q => ({ items: q.column === "blocked" ? [card] : [], next: null }));
  const select = document.querySelector("select")!;
  select.value = "one"; select.dispatchEvent(new Event("change", { bubbles: true })); await settle();
  const tile = document.querySelector<HTMLButtonElement>(".home-card")!;
  expect(tile.textContent).toContain("type:bug"); expect(tile.textContent).toContain("worker"); expect(tile.textContent).toContain("overdue");
  expect(tile.textContent).toContain("Need access"); expect(tile.textContent).not.toContain("more");
  tile.click(); expect(open).toHaveBeenCalledWith({ id: "ONE-1", project: "one" });
  expect(home.cards).toHaveBeenCalledWith(expect.objectContaining({ project: "one" }));
});
it("shows a column cog for each column of a lane only when one project is chosen (TER-1116)", async () => {
  home.monitors.mockResolvedValue({ items: [], next: null });
  home.pipelines.mockResolvedValue({ items: [], next: null });
  mount(); await settle();
  expect(document.querySelector("[data-column-cog]")).toBeNull();
  const select = document.querySelector<HTMLSelectElement>('select[aria-label="Project"]')!;
  select.value = "two"; select.dispatchEvent(new Event("change", { bubbles: true })); await settle();
  const lane = (title: string) => [...document.querySelector(`section[aria-label="${title}"]`)!.querySelectorAll<HTMLElement>("[data-column-cog]")].map(b => b.dataset.columnCog);
  expect(lane("In progress")).toEqual(["working"]);
  expect(lane("Review")).toEqual(["review", "merge_queue"]);
  expect(lane("Accepted")).toEqual([]);
  expect(document.querySelector('[data-column-cog="merge_queue"]')?.getAttribute("aria-label")).toBe("Options of merge queue (two)");
});

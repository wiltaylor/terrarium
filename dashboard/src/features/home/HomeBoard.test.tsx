// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { allHome, type HomeCard } from "../../api/home";

const home = { cards: vi.fn(), monitors: vi.fn(), pipelines: vi.fn() };
const open = vi.fn();
vi.mock("../../stores", () => ({ api: { home }, cardPeek: { open }, projects: { projects: () => [{ name: "one" }, { name: "two" }] }, events: { on: () => () => {} } }));
vi.mock("@solidjs/router", () => ({ A: (p: { href: string; children: unknown }) => <a href={p.href}>{p.children as string}</a> }));
const { HomeBoard } = await import("./HomeBoard");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; vi.resetAllMocks(); });
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
  expect(document.querySelectorAll(".home-status")).toHaveLength(2);
  expect(document.body.textContent).not.toContain("All Systems Nominal");
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

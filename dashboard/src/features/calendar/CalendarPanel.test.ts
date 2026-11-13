// @vitest-environment jsdom
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent, CalendarView } from "../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(event: AgentEvent) => void>();
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api, events: { on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } } });
});
vi.mock("../../instance/navigation", () => ({ InstanceAnchor: (props: { children: unknown }) => props.children }));
const { CalendarPanel } = await import("./CalendarPage");
const disposers: (() => void)[] = [];
const empty = (from: string, to: string): CalendarView => ({ from, to, occurrences: [], events: [], truncated: false, can_write: false });
beforeEach(() => {
  vi.spyOn(api.calendar, "project").mockImplementation(async (_project, range) => empty(range?.from ?? "", range?.to ?? ""));
  vi.spyOn(api.calendar, "watchers").mockResolvedValue([]);
});
afterEach(() => { disposers.splice(0).forEach((dispose) => dispose()); expect(listeners.size).toBe(0); document.body.innerHTML = ""; vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const tick = async (ms = 0) => { await new Promise((resolve) => setTimeout(resolve, ms)); for (let i = 0; i < 8; i++) await new Promise((resolve) => setTimeout(resolve, 0)); };
function mount(props: Parameters<typeof CalendarPanel>[0]) {
  const root = document.createElement("div"); document.body.append(root);
  disposers.push(render(() => createComponent(CalendarPanel, props), root)); return root;
}

it.each(["month", "week", "agenda"] as const)("retains saved %s on a phone and reports local view changes", async (view) => {
  vi.stubGlobal("matchMedia", () => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() }));
  const changed = vi.fn();
  const root = mount({ project: "terrarium", view, onViewChange: changed }); await tick();
  expect(!!root.querySelector(".cal-month")).toBe(view === "month");
  expect(!!root.querySelector(".cal-week")).toBe(view === "week");
  const month = [...root.querySelectorAll("button")].find((button) => button.textContent?.trim() === "Month")!;
  month.click(); await tick();
  expect(root.querySelector(".cal-month")).not.toBeNull();
  expect(changed).toHaveBeenCalledWith("month");
  expect(root.textContent).not.toContain("New event");
});

it("defaults to month and isolates duplicate view and anchor", async () => {
  const first = mount({ project: "terrarium" });
  const second = mount({ project: "terrarium", view: "week" }); await tick();
  const title = first.querySelector("[role=grid]")!.getAttribute("aria-label");
  second.querySelector<HTMLButtonElement>("button[aria-label=On]")!.click(); await tick();
  expect(first.querySelector("[role=grid]")!.getAttribute("aria-label")).toBe(title);
  expect(second.querySelector(".cal-week")).not.toBeNull();
});

it("retains grid DOM, focus, scroll and anchor through project live refresh", async () => {
  const root = mount({ project: "terrarium" }); await tick();
  const cell = root.querySelector<HTMLElement>("[role=gridcell]")!;
  const title = root.querySelector("[role=grid]")!.getAttribute("aria-label");
  cell.tabIndex = 0; cell.focus(); root.scrollTop = 90;
  listeners.forEach((fn) => fn({ seq: 1, id: "calendar-refresh", at: "", kind: "calendar.updated", project: "terrarium", summary: "", data: {} }));
  await tick(450);
  expect(root.querySelector("[role=gridcell]")).toBe(cell);
  expect(document.activeElement).toBe(cell);
  expect(root.scrollTop).toBe(90);
  expect(root.querySelector("[role=grid]")!.getAttribute("aria-label")).toBe(title);
});

it("does not show an old project's late write grant and cleans up subscriptions", async () => {
  let resolve!: (value: CalendarView) => void;
  vi.mocked(api.calendar.project).mockImplementation((project, range) => project === "old" ? new Promise((done) => { resolve = done; }) : Promise.resolve(empty(range?.from ?? "", range?.to ?? "")));
  const [project, setProject] = createSignal("old");
  const root = mount({ get project() { return project(); } }); await tick();
  setProject("new"); await tick();
  resolve({ ...empty("", ""), can_write: true }); await tick();
  expect(root.textContent).not.toContain("New event");
  expect(listeners.size).toBe(1);
});

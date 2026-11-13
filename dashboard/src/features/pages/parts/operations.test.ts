// @vitest-environment jsdom
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../../api/mock";
import type { AgentEvent, CalendarView, PageWidget, ProjectWidgetViews } from "../../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(event: AgentEvent) => void>();
vi.mock("../../../stores", async () => {
  const { instanceTestStores } = await import("../../../test/instance");
  return instanceTestStores({ api, events: { on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } } });
});
vi.mock("../../../instance/navigation", () => ({ A: (props: { children: unknown }) => props.children, InstanceAnchor: (props: { children: unknown }) => props.children }));
const { OPERATIONS_WIDGET_RENDERERS, OperationsWidget, operationsRenderer } = await import("./operations");
const { PROJECT_WIDGET_VIEWS, featureWidget } = await import("../widgetRegistry");
type Props = Parameters<typeof OperationsWidget>[0];

const disposers: (() => void)[] = [];
const soon = () => new Date(Date.now() + 3_600_000).toISOString();
const calendar = (titles: string[], canWrite = false): CalendarView => ({
  from: "", to: "", truncated: false, can_write: canWrite, events: [],
  occurrences: titles.map((title, i) => ({ event_id: `ev${i}`, start: soon(), end: soon(), all_day: false, title, kind: "task", project: "terrarium", global: false, recurring: false, assignees: [] })),
});
beforeEach(() => {
  vi.spyOn(api.calendar, "project").mockResolvedValue(calendar(["Patch window", "Release freeze"]));
  vi.spyOn(api.calendar, "watchers").mockResolvedValue([]);
});
afterEach(() => { disposers.splice(0).forEach((dispose) => dispose()); expect(listeners.size).toBe(0); document.body.innerHTML = ""; vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const tick = async (ms = 0) => { await new Promise((resolve) => setTimeout(resolve, ms)); for (let i = 0; i < 8; i++) await new Promise((resolve) => setTimeout(resolve, 0)); };
function mount(props: Props) {
  const root = document.createElement("div"); document.body.append(root);
  disposers.push(render(() => createComponent(OperationsWidget, props), root)); return root;
}
const widget = <K extends "schedules" | "rules" | "pipelines" | "calendar">(kind: K, view: ProjectWidgetViews[K], search?: string, id = `${kind}-${view}`) => featureWidget(kind, view, id, 0, search);
const labels = (root: HTMLElement) => [...root.querySelectorAll("button")].map((b) => b.textContent?.trim()).join("|");
const PART_SELECTOR: Record<string, string> = {
  "schedules:schedules": "table[aria-label='Schedules']", "schedules:triggers": "table[aria-label='Triggers']", "schedules:event_sources": "table[aria-label='Event sources']",
  "rules:flow": "[aria-label='Board flow']", "rules:lint": "[aria-label='Lint']", "rules:table": "table[aria-label='Rules']", "rules:last_runs": "table[aria-label='Rule runs']",
  "pipelines:state": "[aria-label='Pipelines by state']", "pipelines:runs": ".pl-runs",
  "calendar:month": ".cal-month", "calendar:week": ".cal-week", "calendar:agenda": ".cal-agenda",
};

it("maps every registry view of the four kinds and nothing else", () => {
  for (const kind of ["schedules", "rules", "pipelines", "calendar"] as const) {
    expect(Object.keys(OPERATIONS_WIDGET_RENDERERS[kind]).sort()).toEqual([...PROJECT_WIDGET_VIEWS[kind]].sort());
  }
  expect(operationsRenderer({ kind: "board", options: { view: "lanes" } })).toBeUndefined();
  expect(operationsRenderer({ kind: "markdown" })).toBeUndefined();
  expect(operationsRenderer({ kind: "rules", options: { view: "lanes" as never } })).toBeUndefined();
  expect(operationsRenderer({ kind: "rules" })).toBe(OPERATIONS_WIDGET_RENDERERS.rules.flow);
  expect(operationsRenderer({ kind: "schedules" })).toBe(OPERATIONS_WIDGET_RENDERERS.schedules.schedules);
  expect(operationsRenderer({ kind: "calendar" })).toBe(OPERATIONS_WIDGET_RENDERERS.calendar.month);
});

it.each(Object.keys(PART_SELECTOR))("renders only the %s part, read-only without grants", async (part) => {
  const [kind, view] = part.split(":") as ["rules", ProjectWidgetViews["rules"]];
  const root = mount({ project: "terrarium", widget: widget(kind, view) }); await tick();
  for (const [other, selector] of Object.entries(PART_SELECTOR)) {
    if (other.startsWith(`${kind}:`)) expect(!!root.querySelector(selector), other).toBe(other === part);
  }
  expect(labels(root)).not.toMatch(/New schedule|New trigger|New event source|New pipeline|New event|Plan the migration|Edit|Delete|Rotate/);
  expect(root.querySelector("input[type=checkbox]")).toBeNull();
});

it("shows Not Found for an unknown view and fetches nothing", async () => {
  const rules = vi.spyOn(api.rules, "effective");
  const bad: PageWidget = { id: "bad", kind: "rules", x: 0, y: 0, w: 12, h: 3, options: { view: "graph" as never } };
  const root = mount({ project: "terrarium", widget: bad }); await tick();
  expect(root.textContent).toBe("Not Found");
  expect(rules).not.toHaveBeenCalled();
});

it("grants schedule creation and event-source creation separately", async () => {
  const schedulesOnly = { createSchedule: true };
  const a = mount({ project: "terrarium", widget: widget("schedules", "schedules"), grants: schedulesOnly });
  const b = mount({ project: "terrarium", widget: widget("schedules", "triggers"), grants: schedulesOnly });
  const c = mount({ project: "terrarium", widget: widget("schedules", "event_sources"), grants: schedulesOnly });
  const d = mount({ project: "terrarium", widget: widget("schedules", "event_sources", undefined, "src2"), grants: { createEventSource: true } });
  const e = mount({ project: "terrarium", widget: widget("schedules", "schedules", undefined, "s2"), grants: { createEventSource: true } });
  await tick();
  expect(labels(a)).toContain("New schedule");
  expect(labels(b)).toContain("New trigger");
  expect(labels(c)).not.toContain("New event source");
  expect(labels(d)).toContain("New event source");
  expect(labels(e)).not.toContain("New schedule");
  // Create grants do not grant row management.
  expect(labels(d)).not.toMatch(/Rotate|Delete/);
  expect(a.querySelector("input[type=checkbox]")).toBeNull();
});

it("applies per-resource manage grants to schedules and event sources", async () => {
  const root = mount({ project: "terrarium", widget: widget("schedules", "event_sources"), grants: { manageEventSource: (s) => s.project === "terrarium" } });
  await tick();
  expect(root.querySelector("input[aria-label='Enable gh']")).not.toBeNull();
  expect(root.querySelector("input[aria-label='Enable nightly']")).toBeNull();
});

it("shows rules and pipelines controls only with their admin grants", async () => {
  const rules = mount({ project: "terrarium", widget: widget("rules", "flow"), grants: { manageRules: true } });
  const pipelines = mount({ project: "terrarium", widget: widget("pipelines", "state"), grants: { managePipelines: true } });
  const wrong = mount({ project: "terrarium", widget: widget("pipelines", "state", undefined, "p2"), grants: { manageRules: true, createSchedule: true } });
  await tick();
  expect(labels(rules)).toContain("Plan the migration");
  expect(labels(pipelines)).toContain("New pipeline");
  expect(labels(wrong)).not.toContain("New pipeline");
});

it("applies the saved search option to each part", async () => {
  const schedules = mount({ project: "terrarium", widget: widget("schedules", "schedules", "standup") });
  const sources = mount({ project: "terrarium", widget: widget("schedules", "event_sources", "night") });
  const pipelines = mount({ project: "terrarium", widget: widget("pipelines", "runs", "octopus") });
  const agenda = mount({ project: "terrarium", widget: widget("calendar", "agenda", "patch") });
  await tick();
  expect(schedules.textContent).toContain("standup");
  expect(schedules.textContent).not.toContain("deploy-staging");
  expect(sources.textContent).toContain("nightly");
  expect(sources.textContent).not.toContain("/hooks/terrarium/gh");
  expect(pipelines.textContent).toContain("octopus-deploy");
  expect(agenda.textContent).toContain("Patch window");
  expect(agenda.textContent).not.toContain("Release freeze");
});

it("keeps duplicate instances of one part independent", async () => {
  const first = mount({ project: "terrarium", widget: widget("schedules", "schedules", undefined, "one") });
  const second = mount({ project: "terrarium", widget: widget("schedules", "schedules", "tests", "two") });
  const month = mount({ project: "terrarium", widget: widget("calendar", "month", undefined, "m") });
  const week = mount({ project: "terrarium", widget: widget("calendar", "week", undefined, "w") });
  await tick();
  first.querySelector<HTMLButtonElement>("button[aria-expanded]")!.click(); await tick();
  expect(second.querySelector("button[aria-expanded=true]")).toBeNull();
  expect(second.textContent).not.toContain("standup");
  const title = month.querySelector("[role=grid]")!.getAttribute("aria-label");
  week.querySelector<HTMLButtonElement>("button[aria-label=On]")!.click(); await tick();
  expect(month.querySelector("[role=grid]")!.getAttribute("aria-label")).toBe(title);
  expect(week.querySelector(".cal-week")).not.toBeNull();
});

it.each(["month", "week", "agenda"] as const)("keeps a saved %s calendar on a phone with all three modes selectable", async (view) => {
  vi.stubGlobal("matchMedia", () => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() }));
  const root = mount({ project: "terrarium", widget: widget("calendar", view) }); await tick();
  expect(root.querySelector(PART_SELECTOR[`calendar:${view}`]!)).not.toBeNull();
  for (const mode of ["Month", "Week", "Agenda"]) expect(labels(root).split("|")).toContain(mode);
});

it("defaults a calendar without a saved view to month, also on a phone", async () => {
  vi.stubGlobal("matchMedia", () => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() }));
  const root = mount({ project: "terrarium", widget: { id: "c", kind: "calendar", x: 0, y: 0, w: 12, h: 3 } }); await tick();
  expect(root.querySelector(".cal-month")).not.toBeNull();
});

it("shows calendar write controls only from the server can_write", async () => {
  vi.mocked(api.calendar.project).mockResolvedValue(calendar([], true));
  const root = mount({ project: "terrarium", widget: widget("calendar", "month") }); await tick();
  expect(labels(root)).toContain("New event");
});

it("follows a changed saved view: calendar keeps its anchor, other kinds mount the new part", async () => {
  const [cal, setCal] = createSignal(widget("calendar", "month"));
  const [rules, setRules] = createSignal(widget("rules", "flow"));
  const c = mount({ project: "terrarium", get widget() { return cal(); } });
  const r = mount({ project: "terrarium", get widget() { return rules(); } });
  await tick();
  c.querySelector<HTMLButtonElement>("button[aria-label=On]")!.click(); await tick();
  const moved = c.querySelector("[role=grid]")!.getAttribute("aria-label");
  setCal(widget("calendar", "week")); setRules(widget("rules", "lint")); await tick();
  expect(c.querySelector(".cal-week")).not.toBeNull();
  setCal(widget("calendar", "month")); await tick();
  expect(c.querySelector("[role=grid]")!.getAttribute("aria-label")).toBe(moved);
  expect(r.querySelector("[aria-label='Lint']")).not.toBeNull();
  expect(r.querySelector("[aria-label='Board flow']")).toBeNull();
});

it("retains row DOM, focus and scroll through a live event", async () => {
  const root = mount({ project: "terrarium", widget: widget("pipelines", "state") }); await tick();
  const button = root.querySelector<HTMLButtonElement>("button.link")!;
  button.focus(); root.scrollTop = 60;
  listeners.forEach((fn) => fn({ seq: 1, id: "p", at: "", kind: "pipeline.status", project: "terrarium", summary: "", data: {} }));
  await tick(450);
  expect(root.querySelector("button.link")).toBe(button);
  expect(document.activeElement).toBe(button);
  expect(root.scrollTop).toBe(60);
});

it("drops a late reply of the previous project and keeps one subscription", async () => {
  const schedules = await api.schedules.list({ project: "terrarium", kind: "time" });
  let resolve!: (value: typeof schedules) => void;
  vi.spyOn(api.schedules, "list").mockImplementation((query) => query?.project === "old" ? new Promise((done) => { resolve = done; }) : Promise.resolve([]));
  const [project, setProject] = createSignal("old");
  const root = mount({ get project() { return project(); }, widget: widget("schedules", "schedules") });
  await tick(); setProject("new"); await tick(); resolve(schedules); await tick();
  expect(root.textContent).not.toContain("standup");
  expect(listeners.size).toBe(1);
});

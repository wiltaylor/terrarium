// @vitest-environment jsdom
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent } from "../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(event: AgentEvent) => void>();
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api, events: { on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } }, projects: { projects: () => [{ name: "terrarium" }] }, agents: { state: { agents: [] } }, subscriptions: { list: () => [] } });
});
vi.mock("../../instance/navigation", () => ({ A: (props: { children: unknown }) => props.children }));
const { SchedulesPanel } = await import("./SchedulesPage");
const disposers: (() => void)[] = [];
afterEach(() => { disposers.splice(0).forEach((dispose) => dispose()); expect(listeners.size).toBe(0); document.body.innerHTML = ""; vi.restoreAllMocks(); });
const tick = async () => { for (let i = 0; i < 8; i++) await new Promise((resolve) => setTimeout(resolve, 0)); };
function mount(props: Parameters<typeof SchedulesPanel>[0]) {
  const root = document.createElement("div"); document.body.append(root);
  disposers.push(render(() => createComponent(SchedulesPanel, props), root)); return root;
}

it.each(["schedules", "triggers", "event_sources"] as const)("renders just %s, with no reader mutation controls", async (part) => {
  const root = mount({ project: "terrarium", part }); await tick();
  expect(!!root.querySelector("table[aria-label='Schedules']")).toBe(part === "schedules");
  expect(!!root.querySelector("table[aria-label='Triggers']")).toBe(part === "triggers");
  expect(!!root.querySelector("table[aria-label='Event sources']")).toBe(part === "event_sources");
  expect(root.querySelector("input[type=checkbox]")).toBeNull();
  expect([...root.querySelectorAll("button")].map((button) => button.textContent?.trim()).join("|")).not.toMatch(/New schedule|New trigger|New event source|Edit|Delete|Rotate/);
});

it("isolates duplicate scope, selections, and drafts", async () => {
  const selected = vi.fn();
  const first = mount({ project: "terrarium", part: "schedules", canCreate: true, onSelectionChange: selected });
  const second = mount({ project: "terrarium", part: "schedules", scope: "global" });
  await tick();
  const disclosure = first.querySelector<HTMLButtonElement>("button[aria-expanded]")!;
  disclosure.click(); await tick();
  expect(selected).toHaveBeenCalledTimes(1);
  expect(second.querySelector("button[aria-expanded=true]")).toBeNull();
  [...first.querySelectorAll("button")].find((button) => button.textContent?.trim() === "New schedule")!.click();
  await tick();
  expect(document.querySelectorAll("[role=dialog]").length).toBe(1);
  expect(second.querySelector("button[aria-expanded=true]")).toBeNull();
});

it("drops a delayed previous-project list and removes its subscription", async () => {
  const schedules = await api.schedules.list({ project: "terrarium", kind: "time" });
  let resolve!: (value: typeof schedules) => void;
  vi.spyOn(api.schedules, "list").mockImplementation((query) => query?.project === "old" ? new Promise((done) => { resolve = done; }) : Promise.resolve([]));
  const [project, setProject] = createSignal("old");
  const root = mount({ get project() { return project(); }, part: "schedules" });
  await tick(); setProject("new"); await tick(); resolve(schedules); await tick();
  expect(root.querySelector("button[aria-expanded]")).toBeNull();
  expect(listeners.size).toBe(1);
});

it("keeps a selected row, focus and scroll through a live event", async () => {
  const root = mount({ project: "terrarium", part: "schedules" }); await tick();
  const button = root.querySelector<HTMLButtonElement>("button[aria-expanded]")!;
  button.click(); button.focus(); root.scrollTop = 60;
  // A known schedule event updates the keyed row without replacing it.
  const schedule = (await api.schedules.list({ project: "terrarium", kind: "time" }))[0];
  listeners.forEach((fn) => fn({ kind: "schedule", project: "terrarium", data: { schedule, change: "updated" } } as unknown as AgentEvent));
  await tick();
  expect(root.querySelector("button[aria-expanded]")).toBe(button);
  expect(button.getAttribute("aria-expanded")).toBe("true");
  expect(document.activeElement).toBe(button);
  expect(root.scrollTop).toBe(60);
});

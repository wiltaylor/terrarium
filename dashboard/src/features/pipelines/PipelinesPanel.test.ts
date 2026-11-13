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
  return instanceTestStores({ api, events: { on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } } });
});
vi.mock("../../instance/navigation", () => ({ InstanceAnchor: (props: { children: unknown }) => props.children }));
const { PipelinesPanel } = await import("./PipelinesPage");
const disposers: (() => void)[] = [];
afterEach(() => { disposers.splice(0).forEach((dispose) => dispose()); expect(listeners.size).toBe(0); document.body.innerHTML = ""; vi.restoreAllMocks(); });
const tick = async (ms = 0) => { await new Promise((resolve) => setTimeout(resolve, ms)); for (let i = 0; i < 8; i++) await new Promise((resolve) => setTimeout(resolve, 0)); };
function mount(props: Parameters<typeof PipelinesPanel>[0]) {
  const root = document.createElement("div"); document.body.append(root);
  disposers.push(render(() => createComponent(PipelinesPanel, props), root)); return root;
}

it("makes runs independently discoverable and keeps state separate", async () => {
  const runs = mount({ project: "terrarium", part: "runs" });
  const state = mount({ project: "terrarium", part: "state" });
  await tick();
  expect(runs.querySelector(".pl-runs")).not.toBeNull();
  expect(state.querySelector(".pl-runs")).toBeNull();
  expect(state.querySelector("[aria-label='Pipelines by state']")).not.toBeNull();
  expect(runs.querySelector("[aria-label='Pipelines by state']")).toBeNull();
  expect(runs.textContent).not.toContain("New pipeline");
  expect(runs.textContent).not.toContain("Delete");
  expect(runs.querySelector("input[type=checkbox]")).toBeNull();
});

it("keeps duplicate searches independent and retains row focus and scroll on live refresh", async () => {
  const first = mount({ project: "terrarium", part: "state", search: "github" });
  const second = mount({ project: "terrarium", part: "runs", search: "octopus" });
  await tick();
  expect(first.textContent).not.toContain("octopus-deploy");
  expect(second.textContent).toContain("octopus-deploy");
  const button = first.querySelector<HTMLButtonElement>("button.link")!;
  button.focus(); first.scrollTop = 75;
  listeners.forEach((fn) => fn({ kind: "pipeline.updated", project: "terrarium" } as AgentEvent));
  await tick(450);
  expect(first.querySelector("button.link")).toBe(button);
  expect(document.activeElement).toBe(button);
  expect(first.scrollTop).toBe(75);
});

it("drops old selections and late replies when project changes", async () => {
  const pipelines = await api.pipelines.list("terrarium");
  let resolve!: (value: typeof pipelines) => void;
  vi.spyOn(api.pipelines, "list").mockImplementation((project) => project === "old" ? new Promise((done) => { resolve = done; }) : Promise.resolve([]));
  const [project, setProject] = createSignal("old");
  const root = mount({ get project() { return project(); }, part: "runs" });
  await tick(); setProject("new"); await tick(); resolve(pipelines); await tick();
  expect(root.querySelector(".pl-runs")).toBeNull();
  expect(listeners.size).toBe(1);
});

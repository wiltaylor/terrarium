// @vitest-environment jsdom
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api });
});
const { RulesPanel } = await import("./RulesPage");
const disposers: (() => void)[] = [];
afterEach(() => { disposers.splice(0).forEach((dispose) => dispose()); document.body.innerHTML = ""; vi.restoreAllMocks(); });
const tick = async () => { for (let i = 0; i < 8; i++) await new Promise((resolve) => setTimeout(resolve, 0)); };
function mount(props: Parameters<typeof RulesPanel>[0]) {
  const root = document.createElement("div");
  document.body.append(root);
  disposers.push(render(() => createComponent(RulesPanel, props), root));
  return root;
}

it.each(["flow", "lint", "table", "last_runs"] as const)("renders only the %s part without route state", async (part) => {
  const root = mount({ project: "terrarium", part });
  await tick();
  expect(!!root.querySelector("[aria-label='Board flow']")).toBe(part === "flow");
  expect(!!root.querySelector("[aria-label='Lint']")).toBe(part === "lint");
  expect(!!root.querySelector("table[aria-label='Rules']")).toBe(part === "table");
  expect(!!root.querySelector("table[aria-label='Rule runs']")).toBe(part === "last_runs");
  expect(root.textContent).not.toContain("Plan the migration");
  expect(root.querySelector("input[type=checkbox]")).toBeNull();
});

it("isolates duplicate migration drafts and retains a draft through refresh", async () => {
  const first = mount({ project: "terrarium", part: "flow", canManage: true });
  const second = mount({ project: "terrarium", part: "flow", canManage: true });
  await tick();
  const buttons = (root: HTMLElement, label: string) => [...root.querySelectorAll("button")].find((b) => b.textContent?.trim() === label)!;
  buttons(first, "Plan the migration").click();
  await tick();
  const plan = first.querySelector("[aria-label='Migration plan']");
  expect(plan).not.toBeNull();
  expect(second.querySelector("[aria-label='Migration plan']")).toBeNull();
  buttons(first, "Refresh").click();
  await tick();
  expect(first.querySelector("[aria-label='Migration plan']")).toBe(plan);
});

it("discards a delayed old-project response", async () => {
  const effective = await api.rules.effective("terrarium");
  let resolve!: (value: typeof effective) => void;
  vi.spyOn(api.rules, "effective").mockImplementation((project) => project === "old" ? new Promise((done) => { resolve = done; }) : Promise.resolve(effective));
  const [project, setProject] = createSignal("old");
  const root = mount({ get project() { return project(); }, part: "flow" });
  await tick();
  setProject("terrarium");
  await tick();
  resolve({ ...effective, board_flow: "rules" });
  await tick();
  expect(root.textContent).toContain("board_flow code");
});

// @vitest-environment jsdom
// The Rules tab (docs/api.md §19.12, TER-171): the flow line, the lint, the
// rules with a switch each, the runs, and the migration (a plan, then Apply
// after a confirm). It runs against the mock daemon, whose board for
// project `terrarium` has no validating and no accepted column.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", () => ({ api }));
vi.mock("@solidjs/router", () => ({ useParams: () => ({ project: "terrarium" }) }));

const { default: RulesPage } = await import("./RulesPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

const tick = async (n = 5) => {
  for (let i = 0; i < n; i++) await new Promise((r) => setTimeout(r, 0));
};

function mount() {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(RulesPage, {}), el);
  return el;
}

const button = (root: ParentNode, text: string) => [...root.querySelectorAll("button")].find((b) => b.textContent?.trim() === text);

describe("RulesPage", () => {
  // Runs first: the mock is shared, and the next test migrates the project.
  it("Apply sends the plan's hash: a change of rules.wcl after the plan is a conflict", async () => {
    const el = mount();
    await tick();
    button(el, "Plan the migration")!.click();
    await tick();
    expect(el.querySelector("[aria-label='Migration plan']")).not.toBeNull();
    // rules.wcl changes after the plan (another user, the CLI).
    await api.rules.disable("terrarium", "job-cancelled");
    const spy = vi.spyOn(api.rules, "migrate");
    button(el, "Apply")!.click();
    await tick();
    button(el, "Switch to rules")!.click();
    await tick(10);
    expect(spy).toHaveBeenCalledWith("terrarium", true, "");
    spy.mockRestore();
    expect(document.body.textContent).toMatch(/changed: read them again/);
    expect((await api.rules.effective("terrarium")).board_flow).toBe("code");
    // Back to the seed for the next test: no rules.wcl.
    await api.rules.enable("terrarium", "job-cancelled");
  });

  it("plans the migration, applies it after a confirm, then lists the pack with its switches", async () => {
    const el = mount();
    await tick();
    expect(el.querySelector("[aria-label='Board flow']")?.textContent).toMatch(/board_flow code/);
    expect(el.querySelector("[aria-label=Lint]")?.textContent).toMatch(/No findings/);
    // The coded flow runs no pack rule: the table is empty; the seeded runs show.
    expect(el.querySelector("table[aria-label=Rules]")?.textContent).toMatch(/No rules run/);
    expect(el.querySelector("table[aria-label='Rule runs']")?.textContent).toMatch(/job-started/);

    button(el, "Plan the migration")!.click();
    await tick();
    const plan = el.querySelector("[aria-label='Migration plan']")!;
    expect(plan.textContent).toMatch(/mapped merge-pass: the board has no column `@accepted`/);
    expect(plan.textContent).toMatch(/mapped agent-park/);
    expect(plan.querySelector("pre")?.textContent).toMatch(/rule "merge-pass"/);
    // Apply asks first.
    button(el, "Apply")!.click();
    await tick();
    expect((await api.rules.effective("terrarium")).board_flow).toBe("code");
    button(el, "Switch to rules")!.click();
    await tick(10);
    expect(el.querySelector("[aria-label='Board flow']")?.textContent).toMatch(/board_flow rules/);
    expect(el.querySelector("[role=status]")?.textContent).toMatch(/terrarium runs board_flow rules: rules.wcl has the overrides/);
    // The migration section goes: nothing is missing now.
    expect(el.querySelector("[aria-label=Migration]")).toBeNull();
    const row = el.querySelector("tr[aria-label='Rule merge-pass']")!;
    expect(row.textContent).toMatch(/pack, changed in rules.wcl/);
    expect(row.textContent).toMatch(/card.comment/);
    expect(row.textContent).not.toMatch(/card.move/);

    // Switch a pack rule off: the checkbox calls disable, the row stays off.
    const box = el.querySelector<HTMLInputElement>("input[aria-label='Enable job-cancelled']")!;
    expect(box.checked).toBe(true);
    box.checked = false;
    box.dispatchEvent(new Event("change"));
    await tick(10);
    expect(el.querySelector("[role=status]")?.textContent).toMatch(/job-cancelled is off: rules.wcl changed/);
    expect(el.querySelector<HTMLInputElement>("input[aria-label='Enable job-cancelled']")!.checked).toBe(false);
    const eff = await api.rules.effective("terrarium");
    expect(eff.rules.find((r) => r.name === "job-cancelled")).toMatchObject({ enabled: false, origin: "override" });

    // Open a rule: its WCL text.
    button(el, "▸")!.click();
    await tick();
    expect(el.querySelector("table[aria-label=Rules] pre")?.textContent).toMatch(/^rule "/);
  });

});

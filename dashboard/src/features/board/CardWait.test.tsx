// @vitest-environment jsdom
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { CardWait, CardWaitSummary } from "../../api/types";
import { CardWaitDetails, WaitChip } from "./CardWait";

vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ projects: { projects: () => [] }, cardPeek: { open: () => {} } });
});
vi.mock("../../instance/navigation", () => ({ useNavigate: () => () => {} }));

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); });
const fixture = (over: Partial<CardWait> = {}): CardWait => ({
  mode: "all", dependencies: [
    { card: "TER-10", target: "done", met: true, hidden: false },
    { card: "TER-12", target: "review", met: false, hidden: false },
  ], action: { move_to: "ready", comment: "Review **ready**", wake: true }, wake_recipient: "terrarium/lead", ...over,
});
function mount(wait: CardWait | null) {
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => createComponent(CardWaitDetails, { wait, project: "terrarium" }), root);
  return root;
}
it("shows each condition, target, action and resolved wake recipient", () => {
  const root = mount(fixture());
  expect(root.textContent).toContain("All conditions");
  expect(root.querySelectorAll("li")).toHaveLength(2);
  expect(root.querySelectorAll("li")[0]!.textContent).toContain("met");
  expect(root.querySelectorAll("li")[1]!.textContent).toContain("not met");
  expect(root.textContent).toContain("Target: review");
  expect(root.textContent).toContain("ready");
  expect(root.querySelector("strong")?.textContent).toBe("ready");
  expect(root.textContent).toContain("terrarium/lead");
});
it("does not disclose any hidden dependency fields even when present", () => {
  const root = mount(fixture({ dependencies: [{ hidden: true, met: false, card: "SECRET-1", target: "private-target" }] }));
  expect(root.textContent).toContain("hidden");
  expect(root.textContent).not.toMatch(/SECRET|Secret|private/);
  expect(root.querySelector("a")).toBeNull();
});
it("sanitizes dependency text and action comments", () => {
  const hostile = '<img src=x onerror="alert(1)"><script>alert(2)</script>';
  const root = mount(fixture({ dependencies: [{ card: hostile, target: "done", met: false, hidden: false }], action: { comment: hostile, wake: false } }));
  expect(root.querySelector("script, [onerror], img")).toBeNull();
  expect(root.textContent).toContain("No wake requested");
  expect(root.textContent).toContain("No move");
});
it("keeps completion latched while showing current condition state", () => {
  const root = mount(fixture({ mode: "any", met_at: "2026-11-11T12:00:00Z" }));
  expect(root.textContent).toContain("Any condition");
  expect(root.textContent).toContain("Wait completed (latched)");
  expect(root.textContent).toContain("not met");
  expect(root.querySelector("time")?.getAttribute("datetime")).toBe("2026-11-11T12:00:00Z");
});
it("has no wait UI for old servers or cards without waits", () => {
  expect(mount(null).textContent).toBe("");
});
it("shows no chip without a wait and accepts a full mutation response", () => {
  const [wait, setWait] = createSignal<CardWait | null>(null);
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => createComponent(WaitChip, { get wait() { return wait(); } }), root);
  expect(root.textContent).toBe("");
  setWait(fixture());
  expect(root.textContent).toBe("waits all: 1/2");
  expect(root.textContent).not.toContain("TER-10");
  setWait(null);
  expect(root.textContent).toBe("");
});
it("updates the chip from pending any-of to latched completion", () => {
  const [wait, setWait] = createSignal<CardWaitSummary>({ mode: "any", met: false, deps_met: 1, deps_total: 2 });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => createComponent(WaitChip, { get wait() { return wait(); } }), root);
  expect(root.textContent).toBe("waits any: 1/2");
  setWait({ mode: "any", met: true, deps_met: 0, deps_total: 2 });
  expect(root.textContent).toBe("wait met");
  expect(root.querySelector(".warn")).toBeNull();
});

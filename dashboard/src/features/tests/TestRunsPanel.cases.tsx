// @vitest-environment jsdom
import { createSignal, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent, TestRunSummary } from "../../api/types";
import type { Page } from "../../api/http";
import { waitFor } from "../../test/wait";
import type { TestRunsQuery } from "./TestRunsPanel";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../instance/navigation", () => ({ A: (p: JSX.AnchorHTMLAttributes<HTMLAnchorElement>) => <a {...p} /> }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api, events: { on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } } });
});
const { TestRunsPanel } = await import("./TestRunsPanel");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.useRealTimers(); });
const row = (id: string): TestRunSummary => ({
  id, project: "my app", name: id, status: "passed", format: "junit", tool: null,
  counts: { total: 1, passed: 1, failed: 0, skipped: 0, errored: 0 },
  duration_ms: null, started_at: null, commit: null, branch: null, file_name: "unit.xml",
  raw_size: 1, notes: [], cards: ["TER-12"], labels: ["version:vnext"],
  created_by: "operator", created_by_name: "operator", created_at: "2026-11-12T00:00:00Z", url: "",
});
function host() { const el = document.createElement("div"); document.body.append(el); return el; }
function change(el: HTMLInputElement | HTMLSelectElement, value: string, event = "change") {
  el.value = value; el.dispatchEvent(new Event(event, { bubbles: true }));
}

it("owns duplicate drafts and applies all filters through instance-local patches", async () => {
  vi.spyOn(api.testRuns, "list").mockResolvedValue({ items: [], next: null });
  const el = host();
  const patch = vi.fn();
  const other = vi.fn();
  dispose = render(() => <>
    <section><TestRunsPanel project="my app" query={{}} onQueryChange={patch} /></section>
    <section><TestRunsPanel project="my app" query={{}} onQueryChange={other} /></section>
  </>, el);
  const sections = el.querySelectorAll("section");
  const input = sections[0]!.querySelector<HTMLInputElement>('[aria-label="Run name"]')!;
  change(input, " unit ", "input");
  expect(patch).not.toHaveBeenCalled();
  expect(sections[1]!.querySelector<HTMLInputElement>('[aria-label="Run name"]')!.value).toBe("");
  sections[0]!.querySelector("form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  expect(patch).toHaveBeenLastCalledWith({ name: "unit" });
  input.dispatchEvent(new Event("blur"));
  expect(patch).toHaveBeenLastCalledWith({ name: "unit" });
  change(sections[0]!.querySelector<HTMLSelectElement>("select")!, "failed");
  expect(patch).toHaveBeenLastCalledWith({ status: "failed" });
  change(sections[0]!.querySelector<HTMLInputElement>('[aria-label="Card ID"]')!, " ter-12 ");
  expect(patch).toHaveBeenLastCalledWith({ card: "TER-12" });
  change(sections[0]!.querySelector<HTMLInputElement>('[aria-label="Label"]')!, " version:vnext ");
  expect(patch).toHaveBeenLastCalledWith({ label: "version:vnext" });
  expect(other).not.toHaveBeenCalled();
});

it("deduplicates pagination, retains rows on errors and keeps detail/card/label links", async () => {
  const list = vi.spyOn(api.testRuns, "list")
    .mockResolvedValueOnce({ items: [row("R1")], next: "cursor" })
    .mockResolvedValueOnce({ items: [row("R1"), row("R2")], next: "next" })
    .mockRejectedValueOnce(new Error("failed page"));
  const el = host();
  dispose = render(() => <TestRunsPanel project="my app" query={{}} onQueryChange={() => {}} />, el);
  await waitFor(() => expect(el.querySelectorAll(".test-run-row")).toHaveLength(1));
  const more = () => [...el.querySelectorAll("button")].find(b => b.textContent?.includes("Load more"))!;
  more().click();
  await waitFor(() => expect(el.querySelectorAll(".test-run-row")).toHaveLength(2));
  expect(list).toHaveBeenLastCalledWith("my app", expect.objectContaining({ cursor: "cursor" }));
  more().click();
  await waitFor(() => expect(el.textContent).toContain("failed page"));
  expect(el.querySelectorAll(".test-run-row")).toHaveLength(2);
  const hrefs = [...el.querySelectorAll("a")].map(a => a.getAttribute("href"));
  expect(hrefs).toContain("/p/my%20app/tests/R1");
  expect(hrefs).toContain("/p/my%20app/board?card=TER-12");
  expect(hrefs).toContain("/p/my%20app/board?version=vnext&group=version");
});

it("ignores stale project/query replies and wrong-project events, cleans timers on disposal", async () => {
  let resolve!: (p: Page<TestRunSummary>) => void;
  const list = vi.spyOn(api.testRuns, "list").mockImplementation(p => p === "old" ? new Promise(r => { resolve = r; }) : Promise.resolve({ items: [row("new")], next: null }));
  const [project, setProject] = createSignal("old");
  const [query, setQuery] = createSignal<TestRunsQuery>({ name: "old", status: "invalid" });
  const el = host();
  dispose = render(() => <TestRunsPanel project={project()} query={query()} onQueryChange={() => {}} />, el);
  await waitFor(() => expect(resolve).toBeTypeOf("function"));
  expect(list).toHaveBeenCalledWith("old", expect.objectContaining({ status: undefined }));
  setProject("new"); setQuery({ name: "new", status: "passed", card: "TER-12", label: "version:vnext" });
  await waitFor(() => expect(el.querySelector(".test-run-name")?.textContent).toBe("new"));
  resolve({ items: [row("stale")], next: "obsolete" });
  await Promise.resolve();
  expect(el.querySelector(".test-run-name")?.textContent).toBe("new");
  expect(list).toHaveBeenLastCalledWith("new", expect.objectContaining({ name: "new", status: "passed", card: "TER-12", label: "version:vnext" }));
  vi.useFakeTimers(); list.mockClear();
  listeners.forEach(fn => fn({ kind: "test_run", project: "old", data: {} } as AgentEvent));
  await vi.advanceTimersByTimeAsync(400);
  expect(list).not.toHaveBeenCalled();
  listeners.forEach(fn => fn({ kind: "test_run", project: "new", data: {} } as AgentEvent));
  await vi.advanceTimersByTimeAsync(400);
  expect(list).toHaveBeenCalledTimes(1);
  list.mockClear();
  listeners.forEach(fn => fn({ kind: "test_run", project: "new", data: {} } as AgentEvent));
  dispose(); dispose = undefined;
  await vi.advanceTimersByTimeAsync(400);
  expect(list).not.toHaveBeenCalled();
  expect(listeners.size).toBe(0);
});

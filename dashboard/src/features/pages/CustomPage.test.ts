// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createMockApi } from "../../api/mock";
import type { AgentEvent, PageWidget } from "../../api/types";
import { ApiError } from "../../api/http";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
const pageEvent = (): AgentEvent => ({ seq: 1, id: "page-event", at: new Date().toISOString(), summary: "Page updated", kind: "page.updated", project: "terrarium", data: { slug: "overview" } });
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores({ api, events: {
  on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
} }); });
const { default: CustomPage } = await import("./CustomPage");

async function mount(widgets: PageWidget[] = []) {
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  await api.pages.save("terrarium", "overview", { title: "Overview", position: 0, widgets });
  const history = createMemoryHistory(); history.set({ value: "/p/terrarium/pages/overview" });
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, { history, get children() { return createComponent(Route, { path: "/p/:project/pages/:slug", component: CustomPage }); } }), host);
  await waitFor(() => expect(host.textContent).toContain("Overview"));
  return { host, dispose: () => { dispose(); host.remove(); vi.restoreAllMocks(); vi.unstubAllGlobals(); } };
}
function click(host: HTMLElement, text: string) {
  const button = [...host.querySelectorAll("button")].find(b => b.textContent === text);
  expect(button).toBeTruthy(); button!.click();
}

it("keeps the shipped picker and grid when capability is absent or false", async () => {
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
    click(host, "Edit page");
    expect([...host.querySelectorAll("select option")].map(option => option.textContent)).not.toContain("board:lanes");
    expect(host.querySelector(".snap-grid-document")).toBeNull();
  } finally { dispose(); }
});

it("adds independent feature parts only when enabled and preserves their options on save", async () => {
  const originalList = api.pages.list;
  vi.spyOn(api.pages, "list").mockImplementation(async project => ({ ...await originalList(project), capabilities: { project_pages_v2: true } }));
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
    click(host, "Edit page");
    const picker = host.querySelector("select")!;
    picker.value = "board:lanes"; picker.dispatchEvent(new Event("change", { bubbles: true }));
    click(host, "Add widget");
    click(host, "Add widget");
    const sections = [...host.querySelectorAll<HTMLElement>("section[data-widget-id]")];
    expect(sections.length).toBe(2);
    expect(sections[0]!.dataset.widgetId).not.toBe(sections[1]!.dataset.widgetId);
    const search = [...host.querySelectorAll("label")].find(label => label.textContent?.startsWith("Search"))!.querySelector("input")!;
    search.value = "second only"; search.dispatchEvent(new Event("input", { bubbles: true }));
    const previous = await api.pages.get("terrarium", "overview");
    const save = vi.spyOn(api.pages, "save").mockImplementation(async (_project, _slug, body) => ({ ...previous, ...body }));
    click(host, "Save page");
    await waitFor(() => expect(save).toHaveBeenCalled());
    const widgets = save.mock.calls[0]![2].widgets;
    expect(widgets.map(widget => widget.options)).toEqual([{ view: "lanes" }, { view: "lanes", search: "second only" }]);
  } finally { dispose(); }
});

it("keeps enabled frames and open editors through page and save rights refreshes", async () => {
  const originalList = api.pages.list;
  let initial = true;
  const pending: Array<(value: Awaited<ReturnType<typeof originalList>>) => void> = [];
  const list = vi.spyOn(api.pages, "list").mockImplementation(async project => {
    if (initial) {
      initial = false;
      return { ...await originalList(project), capabilities: { project_pages_v2: true } };
    }
    return new Promise(resolve => pending.push(value => resolve({ ...value, capabilities: { project_pages_v2: true } })));
  });
  const { host, dispose } = await mount();
  const expectEnabled = () => {
    expect(host.querySelector(".snap-grid-document")).not.toBeNull();
    expect(host.querySelector("section[data-widget-id]")?.getAttribute("data-frame")).toBe("viewport");
    expect([...host.querySelectorAll("select option")].map(option => option.textContent)).toContain("board:lanes");
  };
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
    click(host, "Edit page");
    const picker = host.querySelector("select")!;
    picker.value = "board:lanes"; picker.dispatchEvent(new Event("change", { bubbles: true }));
    click(host, "Add widget");
    const frame = [...host.querySelectorAll("label")].find(label => label.textContent?.startsWith("Frame"))!.querySelector("select")!;
    frame.value = "viewport"; frame.dispatchEvent(new Event("change", { bubbles: true }));
    expectEnabled();
    expect([...host.querySelectorAll("label")].some(label => label.textContent?.startsWith("Search"))).toBe(true);

    listeners.forEach(fn => fn(pageEvent()));
    await waitFor(() => expect(list).toHaveBeenCalledTimes(2));
    expectEnabled();
    expect([...host.querySelectorAll("label")].some(label => label.textContent?.startsWith("Search"))).toBe(true);
    const current = await originalList("terrarium");
    pending.shift()!(current);
    await waitFor(() => expect(pending).toHaveLength(0));

    const previous = await api.pages.get("terrarium", "overview");
    vi.spyOn(api.pages, "save").mockImplementation(async (_project, _slug, body) => ({ ...previous, ...body }));
    click(host, "Save page");
    await waitFor(() => expect(list).toHaveBeenCalledTimes(3));
    expectEnabled();
    pending.shift()!(current);
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
  } finally {
    pending.forEach(resolve => resolve({ items: [], can_edit: true, capabilities: { project_pages_v2: true } }));
    dispose();
  }
});

it("keeps oversized search text in the draft and reports its UTF-8 byte count", async () => {
  const originalList = api.pages.list;
  vi.spyOn(api.pages, "list").mockImplementation(async project => ({ ...await originalList(project), capabilities: { project_pages_v2: true } }));
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
    click(host, "Edit page");
    const picker = host.querySelector("select")!;
    picker.value = "board:lanes"; picker.dispatchEvent(new Event("change", { bubbles: true }));
    click(host, "Add widget");
    const search = [...host.querySelectorAll("label")].find(label => label.textContent?.startsWith("Search"))!.querySelector("input")!;
    const oversized = "é".repeat(129);
    search.value = oversized; search.dispatchEvent(new Event("input", { bubbles: true }));
    expect(search.value).toBe(oversized);
    expect(search.getAttribute("aria-invalid")).toBe("true");
    expect([...host.querySelectorAll("p")].find(p => p.textContent === "258 / 256 bytes")?.className).toBe("error");
    const previous = await api.pages.get("terrarium", "overview");
    const save = vi.spyOn(api.pages, "save").mockImplementation(async (_project, _slug, body) => ({ ...previous, ...body }));
    click(host, "Save page");
    await waitFor(() => expect(save).toHaveBeenCalled());
    expect(save.mock.calls[0]![2].widgets[0]?.options?.search).toBe(oversized);
  } finally { dispose(); }
});

it("renders an unresolved target-local monitor as Not Found without fetching source data", async () => {
  const originalList = api.pages.list;
  vi.spyOn(api.pages, "list").mockImplementation(async project => ({ ...await originalList(project), capabilities: { project_pages_v2: true } }));
  const series = vi.spyOn(api.monitors, "series");
  const { host, dispose } = await mount([{ id: "copied", kind: "chart", x: 0, y: 0, w: 12, h: 3, monitor: "missing-source-id" }]);
  try {
    await waitFor(() => expect(host.textContent).toContain("Not Found"));
    expect(series).not.toHaveBeenCalled();
  } finally { dispose(); }
});

it("keeps an edit draft when another editor emits page.updated", async () => {
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
    click(host, "Edit page");
    const input = host.querySelector("input")!; input.value = "My draft"; input.dispatchEvent(new Event("input", { bubbles: true }));
    await api.pages.save("terrarium", "overview", { title: "Other editor", position: 0, widgets: [] });
    listeners.forEach(fn => fn(pageEvent()));
    await waitFor(() => expect(host.querySelector("input")?.value).toBe("My draft"));
    click(host, "Save page");
    await waitFor(() => expect(host.textContent).toContain("My draft"));
    expect((await api.pages.get("terrarium", "overview")).title).toBe("My draft");
  } finally { dispose(); }
});

it("renders migrated system widgets and retains their DOM during a live reload", async () => {
  const { host, dispose } = await mount([
    { id: "summary", kind: "monitor_summary", x: 0, y: 0, w: 12, h: 2 },
    { id: "note", kind: "markdown", x: 0, y: 2, w: 3, h: 4, text: "Migrated note" },
  ]);
  try {
    await waitFor(() => expect(host.textContent).toContain("Migrated note"));
    const summary = host.querySelector(".dash-summary");
    expect(summary).not.toBeNull();
    const previous = await api.pages.get("terrarium", "overview");
    let resolve!: (value: typeof previous) => void;
    vi.spyOn(api.pages, "get").mockImplementation(() => new Promise(r => { resolve = r; }));
    listeners.forEach(fn => fn(pageEvent()));
    expect(host.querySelector(".dash-summary")).toBe(summary);
    expect(host.textContent).toContain("Migrated note");
    resolve(previous);
    await waitFor(() => expect(host.querySelector(".dash-summary")).toBe(summary));
  } finally { dispose(); }
});

it("hides write controls for a reader", async () => {
  vi.spyOn(api.pages, "list").mockResolvedValue({ items: [], can_edit: false });
  const { host, dispose } = await mount();
  try { await waitFor(() => expect(host.textContent).toContain("This page has no widgets")); expect(host.textContent).not.toContain("Edit page"); }
  finally { dispose(); }
});

it("shows daemon validation refusal and retains the draft", async () => {
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page")); click(host, "Edit page");
    vi.spyOn(api.pages, "save").mockRejectedValue(new ApiError(422, "widget exceeds 10000 rows"));
    click(host, "Save page");
    await waitFor(() => expect(host.querySelector('[role="alert"]')?.textContent).toContain("10000 rows"));
    expect(host.querySelector("input")?.value).toBe("Overview");
  } finally { dispose(); }
});

it("keeps the loaded page visible while a live refresh is pending", async () => {
  const { host, dispose } = await mount();
  try {
    const get = vi.spyOn(api.pages, "get");
    let resolve!: (value: Awaited<ReturnType<typeof api.pages.get>>) => void;
    const previous = await api.pages.get("terrarium", "overview");
    get.mockImplementation(() => new Promise(r => { resolve = r; }));
    listeners.forEach(fn => fn(pageEvent()));
    expect(host.textContent).toContain("Overview");
    expect(host.textContent).toContain("This page has no widgets");
    resolve(previous);
    await waitFor(() => expect(host.textContent).toContain("Overview"));
  } finally { dispose(); }
});

it("keeps widget input mounted and focused while typing and moving", async () => {
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page")); click(host, "Edit page"); click(host, "Add widget");
    const input = host.querySelector("textarea")!; input.focus();
    input.value = "First line"; input.dispatchEvent(new Event("input", { bubbles: true }));
    expect(host.querySelector("textarea")).toBe(input);
    expect(document.activeElement).toBe(input);
    input.value += " and more"; input.dispatchEvent(new Event("input", { bubbles: true }));
    host.querySelector('[aria-label^="Move "]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(host.querySelector("textarea")).toBe(input);
    expect(input.value).toBe("First line and more");
    click(host, "Save page");
    await waitFor(() => expect(host.textContent).not.toContain("Save page"));
    expect((await api.pages.get("terrarium", "overview")).widgets[0]?.text).toBe("First line and more");
  } finally { dispose(); }
});

it("preserves saved grid positions on reload and throughout a pending live refresh", async () => {
  const widgets: PageWidget[] = [{ id: "intro", kind: "markdown", text: "Content", x: 2, y: 3, w: 5, h: 2, auto_height: false }];
  let mounted = await mount(widgets);
  const position = () => mounted.host.querySelector<HTMLElement>('[data-widget-id="intro"]')?.style.cssText;
  try {
    await waitFor(() => expect(position()).toContain("grid-column"));
    const initial = position();
    const original = await api.pages.get("terrarium", "overview");
    let resolve!: (p: typeof original) => void;
    vi.spyOn(api.pages, "get").mockImplementation(() => new Promise(r => { resolve = r; }));
    const section = mounted.host.querySelector("section");
    listeners.forEach(fn => fn(pageEvent()));
    expect(position()).toBe(initial);
    expect(mounted.host.textContent).toContain("Content");
    resolve(structuredClone(original));
    await waitFor(() => expect(position()).toBe(initial));
    expect(mounted.host.querySelector("section")).toBe(section);
    mounted.dispose();
    mounted = await mount(widgets);
    await waitFor(() => expect(position()).toBe(initial));
  } finally { mounted.dispose(); }
});

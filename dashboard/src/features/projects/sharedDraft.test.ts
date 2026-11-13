// @vitest-environment jsdom
// TER-1073: one draft of all tabs, owned by the project shell.
import { MemoryRouter, Route, Router, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { ApiError } from "../../api/http";
import type { AgentEvent, PageWidget, ProjectLayoutInput, ProjectPage } from "../../api/types";

const server = vi.hoisted(() => ({
  revision: 3, canEdit: true, enabled: true,
  pages: [] as ProjectPage[],
  listeners: new Set<(e: AgentEvent) => void>(),
}));
const stored = (slug: string, position: number, widgets: PageWidget[] = []): ProjectPage => ({ project: "demo", slug, title: slug.toUpperCase(), position, widgets, updated_at: "", updated_by: "operator" });
const api = vi.hoisted(() => ({
  pages: {
    list: vi.fn(async () => ({ items: structuredClone(server.pages), can_edit: server.canEdit, ...(server.enabled ? { capabilities: { project_pages_v2: true } } : {}) })),
    get: vi.fn(async (_project: string, slug: string) => { const page = server.pages.find(p => p.slug === slug); if (!page) throw new ApiError(404, "page not found"); return structuredClone(page); }),
    save: vi.fn(), remove: vi.fn(), resetLayout: vi.fn(), copyLayout: vi.fn(),
    layout: vi.fn(async () => ({ revision: server.revision, pages: structuredClone(server.pages) })),
    saveLayout: vi.fn(async (_project: string, body: ProjectLayoutInput) => {
      if (body.base_revision !== server.revision) throw new ApiError(409, "conflict", undefined, "conflict", { revision: server.revision });
      server.revision += 1;
      server.pages = body.pages.map(p => ({ ...structuredClone(p), project: "demo", updated_at: "", updated_by: "operator" }));
      return { revision: server.revision, pages: structuredClone(server.pages) };
    }),
  },
  monitors: { list: async () => [] },
  projects: { counts: async () => ({ blocked_cards: 0, questions: { open: 0, urgent: 0, critical: 0 } }) },
}));
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores({
  api, agents: { inProject: () => [] },
  events: { on: (fn: (e: AgentEvent) => void) => { server.listeners.add(fn); return () => server.listeners.delete(fn); } },
}); });
const { default: ProjectScreen, historyTarget, LEAVE_DRAFT } = await import("./ProjectScreen");
const { default: CustomPage } = await import("../pages/CustomPage");

let cleanup: (() => void) | undefined;
afterEach(() => { cleanup?.(); cleanup = undefined; document.body.replaceChildren(); vi.unstubAllGlobals(); vi.clearAllMocks(); });

async function mount(path = "/p/demo/pages/a") {
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  const history = createMemoryHistory(); history.set({ value: path });
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, {
    history,
    root: props => [Object.assign(document.createElement("a"), { href: "/settings", textContent: "Leave project" }), props.children],
    get children() {
      return [
        createComponent(Route, { path: "/p/:project", component: ProjectScreen, get children() { return [
          createComponent(Route, { path: "/pages/:slug", component: CustomPage }),
          createComponent(Route, { path: "/board", component: () => "Board view" }),
        ]; } }),
        createComponent(Route, { path: "/settings", component: () => "Settings view" }),
      ];
    },
  }), host);
  cleanup = () => { dispose(); host.remove(); };
  await waitFor(() => expect(host.querySelector("nav")?.textContent).toContain("A"));
  return { host, history };
}
const button = (host: HTMLElement, text: string) => {
  const found = [...host.querySelectorAll<HTMLButtonElement>("button")].find(b => b.textContent === text || b.getAttribute("aria-label") === text);
  expect(found, text).toBeTruthy(); return found!;
};
const has = (host: HTMLElement, text: string) => [...host.querySelectorAll("button")].some(b => b.textContent === text || b.getAttribute("aria-label") === text);
const input = (element: HTMLInputElement | HTMLSelectElement, value: string, kind = "input") => { element.value = value; element.dispatchEvent(new Event(kind, { bubbles: true })); };
const tabOrder = (host: HTMLElement) => [...host.querySelectorAll<HTMLElement>(".project-tab-edit")].map(t => t.dataset.slug);
const link = (host: HTMLElement, href: string) => host.querySelector<HTMLAnchorElement>(`nav a[href="${href}"]`)!;
const emit = (data: Record<string, unknown>) => server.listeners.forEach(fn => fn({ seq: 1, id: "e", at: "", summary: "", kind: "page.updated", project: "demo", data }));

function seed() {
  server.revision = 3; server.canEdit = true; server.enabled = true;
  server.pages = [stored("a", 0), stored("b", 1, [{ id: "w1", kind: "board", x: 0, y: 0, w: 12, h: 3, options: { view: "lanes", search: "keep" } }])];
}

async function startEdit(host: HTMLElement) {
  await waitFor(() => expect(has(host, "Edit pages")).toBe(true));
  button(host, "Edit pages").click();
  await waitFor(() => expect(tabOrder(host)).toEqual(["a", "b"]));
}

it("edits two tabs and saves them with one layout write and no per-page controls", async () => {
  seed();
  const { host } = await mount();
  await waitFor(() => expect(has(host, "Edit pages")).toBe(true));
  expect(has(host, "Edit page")).toBe(false);
  expect(has(host, "Add page")).toBe(false);
  await startEdit(host);
  expect(has(host, "Save page") || has(host, "Delete page")).toBe(false);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Alpha");
  link(host, "/p/demo/pages/b").click();
  await waitFor(() => expect(host.querySelector("section[data-widget-id=w1]")).not.toBeNull());
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Alpha");
  const picker = [...host.querySelectorAll("label")].find(l => l.textContent?.startsWith("Widget kind"))!.querySelector("select")!;
  input(picker, "tests:runs", "change");
  button(host, "Add widget").click();
  expect(host.querySelectorAll("section[data-widget-id]")).toHaveLength(2);
  button(host, "Save pages").click();
  await waitFor(() => expect(has(host, "Edit pages")).toBe(true));
  expect(api.pages.saveLayout).toHaveBeenCalledTimes(1);
  expect(api.pages.save).not.toHaveBeenCalled();
  const body = api.pages.saveLayout.mock.calls[0]![1];
  expect(body.base_revision).toBe(3);
  expect(body.pages.map(p => [p.slug, p.title, p.position])).toEqual([["a", "Alpha", 0], ["b", "B", 1]]);
  expect(body.pages[1]!.widgets[0]).toEqual({ id: "w1", kind: "board", x: 0, y: 0, w: 12, h: 3, options: { view: "lanes", search: "keep" } });
  expect(body.pages[1]!.widgets[1]?.options).toEqual({ view: "runs" });
  await waitFor(() => expect(link(host, "/p/demo/pages/a").textContent).toBe("Alpha"));
  expect(host.querySelectorAll("section[data-widget-id]")).toHaveLength(2);
});

it("Cancel discards the changes of all tabs without a write", async () => {
  seed();
  vi.stubGlobal("confirm", () => true);
  const { host } = await mount();
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of b"]')!, "Changed");
  button(host, "Remove a").click();
  await waitFor(() => expect(tabOrder(host)).toEqual(["b"]));
  button(host, "Cancel").click();
  await waitFor(() => expect(link(host, "/p/demo/pages/a")?.textContent).toBe("A"));
  expect(link(host, "/p/demo/pages/b").textContent).toBe("B");
  expect(api.pages.saveLayout).not.toHaveBeenCalled();
  expect(api.pages.save).not.toHaveBeenCalled();
});

it("reorders tabs with buttons, Alt+arrows and drag, keeping focus and contiguous positions", async () => {
  seed();
  server.pages.push(stored("c", 2));
  const { host } = await mount();
  await waitFor(() => expect(has(host, "Edit pages")).toBe(true));
  button(host, "Edit pages").click();
  await waitFor(() => expect(tabOrder(host)).toEqual(["a", "b", "c"]));
  const right = button(host, "Move a right");
  right.focus(); right.click();
  expect(tabOrder(host)).toEqual(["b", "a", "c"]);
  expect(document.activeElement).toBe(right);
  const tab = link(host, "/p/demo/pages/c");
  tab.focus();
  tab.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", altKey: true, bubbles: true }));
  expect(tabOrder(host)).toEqual(["b", "c", "a"]);
  expect(document.activeElement).toBe(tab);
  const items = () => [...host.querySelectorAll<HTMLElement>(".project-tab-edit")];
  items()[2]!.dispatchEvent(new Event("dragstart", { bubbles: true }));
  items()[0]!.dispatchEvent(new Event("dragover", { bubbles: true, cancelable: true }));
  items()[0]!.dispatchEvent(new Event("drop", { bubbles: true, cancelable: true }));
  expect(tabOrder(host)).toEqual(["a", "b", "c"]);
  button(host, "Move c left").click();
  button(host, "Save pages").click();
  await waitFor(() => expect(api.pages.saveLayout).toHaveBeenCalledTimes(1));
  expect(api.pages.saveLayout.mock.calls[0]![1].pages.map(p => [p.slug, p.position])).toEqual([["a", 0], ["c", 1], ["b", 2]]);
});

it("keeps the draft through page.updated and a 409, then replaces only on request", async () => {
  seed();
  vi.stubGlobal("confirm", () => true);
  const { host } = await mount();
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Mine");
  server.revision = 4;
  emit({ project: "demo", revision: 4, operation: "replace" });
  await waitFor(() => expect(host.textContent).toContain("Someone changed these pages"));
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Mine");
  button(host, "Save pages").click();
  await waitFor(() => expect(host.querySelector('[role="alert"]')?.textContent).toContain("revision 4"));
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Mine");
  expect(server.pages[0]!.title).toBe("A");
  button(host, "Replace with my draft").click();
  await waitFor(() => expect(has(host, "Edit pages")).toBe(true));
  expect(api.pages.saveLayout.mock.calls.map(c => c[1].base_revision)).toEqual([3, 4]);
  expect(server.pages[0]!.title).toBe("Mine");
});

it("moves a widget to another tab with its ID and options", async () => {
  seed();
  const { host } = await mount("/p/demo/pages/b");
  await startEdit(host);
  await waitFor(() => expect(host.querySelector("section[data-widget-id=w1]")).not.toBeNull());
  const move = [...host.querySelectorAll("label")].find(l => l.textContent?.startsWith("Move to page"))!.querySelector("select")!;
  input(move, "a", "change");
  expect(host.querySelector("section[data-widget-id=w1]")).toBeNull();
  link(host, "/p/demo/pages/a").click();
  await waitFor(() => expect(host.querySelector("section[data-widget-id=w1]")).not.toBeNull());
  button(host, "Save pages").click();
  await waitFor(() => expect(api.pages.saveLayout).toHaveBeenCalledTimes(1));
  const pages = api.pages.saveLayout.mock.calls[0]![1].pages;
  expect(pages[0]!.widgets).toEqual([{ id: "w1", kind: "board", x: 0, y: 0, w: 12, h: 3, options: { view: "lanes", search: "keep" } }]);
  expect(pages[1]!.widgets).toEqual([]);
});

it("adds a draft-only tab that opens before Save and keeps the draft inside the project", async () => {
  seed();
  const { host } = await mount();
  await startEdit(host);
  // The Add page "+" (TER-1107): its name and tooltip stay "Add page".
  const add = button(host, "Add page");
  expect([add.textContent, add.title, add.disabled]).toEqual(["+", "Add page", false]);
  add.click();
  const form = host.querySelector("nav form")!;
  const [slug, title] = [...form.querySelectorAll("input")];
  input(slug!, "new-tab"); input(title!, "New tab");
  form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await waitFor(() => expect(tabOrder(host)).toEqual(["a", "b", "new-tab"]));
  await waitFor(() => expect(host.querySelector("h2")?.textContent).toContain("New tab"));
  expect(host.querySelector(".error-banner")).toBeNull();
  // With pages ON the fixed tabs are gone: the page's own Board link stays inside the project.
  [...host.querySelectorAll<HTMLAnchorElement>('a[href="/p/demo/board"]')].find(a => !a.closest("nav"))!.click();
  await waitFor(() => expect(host.textContent).toContain("Board view"));
  expect(tabOrder(host)).toEqual(["a", "b", "new-tab"]);
});

it("warns before leaving the project or unloading with unsaved changes", async () => {
  seed();
  const confirm = vi.fn(() => false);
  vi.stubGlobal("confirm", confirm);
  const { host } = await mount();
  await startEdit(host);
  const unchanged = new Event("beforeunload", { cancelable: true });
  window.dispatchEvent(unchanged);
  expect(unchanged.defaultPrevented).toBe(false);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Unsaved");
  const unload = new Event("beforeunload", { cancelable: true });
  window.dispatchEvent(unload);
  expect(unload.defaultPrevented).toBe(true);
  const out = [...host.querySelectorAll("a")].find(a => a.textContent === "Leave project")!;
  out.click();
  await waitFor(() => expect(confirm).toHaveBeenCalledTimes(1));
  expect(host.textContent).not.toContain("Settings view");
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Unsaved");
  confirm.mockReturnValue(true);
  out.click();
  await waitFor(() => expect(host.textContent).toContain("Settings view"));
  expect(api.pages.saveLayout).not.toHaveBeenCalled();
});

it("gives readers and OFF projects no shared editor", async () => {
  seed();
  server.canEdit = false;
  const reader = await mount();
  await waitFor(() => expect(reader.host.textContent).toContain("Board"));
  expect(has(reader.host, "Edit pages") || has(reader.host, "Edit page") || has(reader.host, "Add page")).toBe(false);
  cleanup!(); cleanup = undefined;
  seed();
  server.enabled = false;
  const off = await mount();
  await waitFor(() => expect(has(off.host, "Edit page")).toBe(true));
  expect(has(off.host, "Add page")).toBe(true);
  expect(has(off.host, "Edit pages")).toBe(false);
  expect(api.pages.layout).not.toHaveBeenCalled();
});

it("treats an instance switch as leaving, even to the same project name", async () => {
  const { leavesProject } = await import("./ProjectScreen");
  const { instanceHref } = await import("../../instance");
  const remote = (path: string) => instanceHref("a", path);
  const home = (path: string) => instanceHref(undefined, path);
  // Inside the same project of the same instance: stay.
  expect(leavesProject(remote, "/p/demo", "/remote/a/p/demo/pages/b?x=1")).toBe(false);
  expect(leavesProject(home, "/p/demo", "/p/demo/board#top")).toBe(false);
  // Remote a to home, home to remote a, remote a to remote b: leave.
  expect(leavesProject(remote, "/p/demo", "/p/demo/pages/b")).toBe(true);
  expect(leavesProject(remote, "/p/demo", "/")).toBe(true);
  expect(leavesProject(home, "/p/demo", "/remote/a/p/demo/pages/b")).toBe(true);
  expect(leavesProject(remote, "/p/demo", "/remote/b/p/demo")).toBe(true);
  // Another project with a shared prefix, and history steps: leave.
  expect(leavesProject(home, "/p/demo", "/p/demo2/board")).toBe(true);
  expect(leavesProject(home, "/p/demo", -1)).toBe(true);
});

it("prompts when a link switches to another instance with unsaved changes", async () => {
  seed();
  const confirm = vi.fn(() => false);
  vi.stubGlobal("confirm", confirm);
  const { host } = await mount();
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Unsaved");
  const other = Object.assign(document.createElement("a"), { href: "/remote/a/p/demo/pages/a", textContent: "Remote a" });
  host.append(other);
  other.click();
  await waitFor(() => expect(confirm).toHaveBeenCalledTimes(1));
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Unsaved");
  expect(api.pages.saveLayout).not.toHaveBeenCalled();
});

// The browser Router asks beforeLeave with a numeric delta on Back and Forward.
async function mountBrowser(path: string) {
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  // A real browser history entry carries the router's depth, which gives Back its delta.
  window.history.replaceState({ _depth: window.history.length - 1 }, "", path);
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(Router, { get children() { return [
    createComponent(Route, { path: "/p/:project", component: ProjectScreen, get children() { return [
      createComponent(Route, { path: "/pages/:slug", component: CustomPage }),
    ]; } }),
  ]; } }), host);
  cleanup = () => { dispose(); host.remove(); window.history.replaceState(null, "", "/"); };
  await waitFor(() => expect(host.querySelector("nav")?.textContent).toContain("A"));
  return host;
}

it("keeps the draft without a prompt on Back between tabs of the same project", async () => {
  seed();
  const confirm = vi.fn(() => true);
  vi.stubGlobal("confirm", confirm);
  const host = await mountBrowser("/p/demo/pages/a");
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Unsaved");
  link(host, "/p/demo/pages/b").click();
  await waitFor(() => expect(window.location.pathname).toBe("/p/demo/pages/b"));
  window.history.back();
  await waitFor(() => expect(window.location.pathname).toBe("/p/demo/pages/a"));
  await waitFor(() => expect(link(host, "/p/demo/pages/a").classList.contains("active")).toBe(true));
  expect(confirm).not.toHaveBeenCalled();
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Unsaved");
  expect(tabOrder(host)).toEqual(["a", "b"]);
  expect(has(host, "Save pages")).toBe(true);
});

it("prompts on a Back that leaves the project and stays when it is refused", async () => {
  seed();
  const confirm = vi.fn(() => false);
  vi.stubGlobal("confirm", confirm);
  // A history entry outside the project, before the project page.
  window.history.replaceState({ _depth: window.history.length - 1 }, "", "/p/other/board");
  window.history.pushState({ _depth: window.history.length }, "", "/p/demo/pages/a");
  const host = await mountBrowser("/p/demo/pages/a");
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Unsaved");
  link(host, "/p/demo/pages/b").click();
  await waitFor(() => expect(window.location.pathname).toBe("/p/demo/pages/b"));
  window.history.go(-2);
  await waitFor(() => expect(confirm).toHaveBeenCalledTimes(1));
  expect(confirm).toHaveBeenCalledWith(LEAVE_DRAFT);
  await waitFor(() => expect(window.location.pathname).toBe("/p/demo/pages/b"));
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Unsaved");
  expect(has(host, "Save pages")).toBe(true);
});

it("takes the Back target from the address bar only when it moved", () => {
  const at = (path: string) => ({ pathname: path, search: "", hash: "" });
  expect(historyTarget(-1, at("/p/demo/pages/b"), at("/p/demo/pages/a"))).toBe("/p/demo/pages/a");
  expect(historyTarget(1, at("/p/demo/pages/a"), { pathname: "/p/demo/board", search: "?card=C1", hash: "#reply" })).toBe("/p/demo/board?card=C1#reply");
  // A memory router does not move the address bar: the step stays unknown.
  expect(historyTarget(-1, at("/p/demo/pages/b"), at("/p/demo/pages/b"))).toBe(-1);
  expect(historyTarget("/p/demo/board", at("/p/demo/pages/b"), at("/x"))).toBe("/p/demo/board");
});

it("brings the widget controls back after a Retry of the page's own rights", async () => {
  seed();
  const { host } = await mount();
  await startEdit(host);
  await waitFor(() => expect(has(host, "Add widget")).toBe(true));
  const list = api.pages.list.getMockImplementation()!;
  // The tab row reads first and passes. Only the page's own read fails.
  let calls = 0;
  api.pages.list.mockImplementation(async () => { if (++calls === 2) throw new ApiError(503, "unavailable"); return list(); });
  emit({ project: "demo", revision: 3, operation: "replace" });
  await waitFor(() => expect(has(host, "Retry page rights")).toBe(true));
  expect(has(host, "Add widget")).toBe(false);
  expect(has(host, "Save pages")).toBe(true);
  expect(has(host, "Retry")).toBe(false);
  button(host, "Retry page rights").click();
  await waitFor(() => expect(has(host, "Add widget")).toBe(true));
  expect(has(host, "Retry page rights")).toBe(false);
  api.pages.list.mockImplementation(list);
});

it("says why the draft stopped for a revoke and for v2 OFF, with no rights check text", async () => {
  seed();
  const { host } = await mount();
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Unsaved");
  server.canEdit = false;
  emit({ project: "demo", revision: 3, operation: "replace" });
  await waitFor(() => expect(host.textContent).toContain("You can no longer edit these pages. Your unsaved draft is kept."));
  expect(host.textContent).not.toContain("Could not check your rights");
  expect(has(host, "Retry")).toBe(false);
  expect(has(host, "Discard draft")).toBe(true);
  server.canEdit = true; server.enabled = false;
  emit({ project: "demo", revision: 3, operation: "replace" });
  await waitFor(() => expect(host.textContent).toContain("Project pages are off for this project. Your unsaved draft is kept."));
  expect(host.textContent).not.toContain("Could not check your rights");
  expect(has(host, "Retry")).toBe(false);
  expect(has(host, "Discard draft")).toBe(true);
});

it("keeps the draft and offers Retry when a refresh fails while editing", async () => {
  seed();
  const { host } = await mount();
  await startEdit(host);
  input(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!, "Unsaved");
  // Both the tab row and the page read the list again. Both fail.
  const list = api.pages.list.getMockImplementation()!;
  api.pages.list.mockImplementation(async () => { throw new ApiError(503, "unavailable"); });
  emit({ project: "demo", revision: 3, operation: "replace" });
  await waitFor(() => expect(has(host, "Retry")).toBe(true));
  expect(has(host, "Save pages")).toBe(false);
  api.pages.list.mockImplementation(list);
  button(host, "Retry").click();
  await waitFor(() => expect(has(host, "Save pages")).toBe(true));
  expect(host.querySelector<HTMLInputElement>('input[aria-label="Title of a"]')!.value).toBe("Unsaved");
  expect(has(host, "Retry")).toBe(false);
  expect(api.pages.saveLayout).not.toHaveBeenCalled();
});

// @vitest-environment jsdom
// TER-1077 (C7): the integrated project pages, with the real project shell,
// tab row, page host and feature panels over the mock API. The capability is
// a server answer: each case sets it ON or OFF.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent, PageBuiltin, PageWidget, ProjectLayout, ProjectPage, ProjectPageList, ProjectPageRights } from "../../api/types";
import { waitFor } from "../../test/wait";
import { PROJECT_WIDGET_VIEWS } from "./widgetRegistry";
import { projectIndexPath } from "../projects/projectIndex";
import { pageGrants } from "./pageGrants";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../components/Leases", () => ({ LeaseChips: () => null }));
vi.mock("../../components/HoldBadge", () => ({ HoldBadge: () => null }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  const { createBookmarksStore } = await import("../../stores/bookmarks");
  return instanceTestStores({
    bookmarks: createBookmarksStore(null, window.location.origin),
    api, events: { on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); }, status: () => ({ lastSeq: 1 }), state: () => "live" },
    agents: { inProject: () => [], byId: () => undefined, state: { loading: false, error: null }, refresh: async () => {} },
    repos: { inProject: () => [], refresh: async () => {}, refreshProject: async () => {} },
    leases: { loadAll: async () => {} }, pins: { isPinned: () => false, toggle: async () => {} },
    inbox: { refresh: async () => {} }, projects: { projects: () => [], prefixes: () => ({}) },
    cardPeek: { open: () => {}, close: () => {} },
  });
});
const { default: ProjectScreen } = await import("../projects/ProjectScreen");
const { default: CustomPage } = await import("./CustomPage");

const P = "terrarium";
const none: ProjectPageRights = { write_project: false, admin_project: false, manage_schedules: false, manage_global: false, operator: false };
const member: ProjectPageRights = { ...none, write_project: true };
const admin: ProjectPageRights = { write_project: true, admin_project: true, manage_schedules: true, manage_global: false, operator: false };
const server = { enabled: true, rights: admin, revision: 4, pages: [] as ProjectPage[], fail: false };
const viewport = (id: string, kind: PageWidget["kind"], view?: string, y = 0): PageWidget =>
  ({ id, kind, x: 0, y, w: 12, h: 12, frame: "viewport", ...(view ? { options: { view: view as never } } : {}) });
const page = (slug: string, position: number, widgets: PageWidget[], builtin?: PageBuiltin): ProjectPage =>
  ({ project: P, slug, title: slug[0]!.toUpperCase() + slug.slice(1), position, widgets, ...(builtin ? { builtin } : {}), updated_at: "", updated_by: "system" });
const list = (): ProjectPageList => ({
  items: structuredClone(server.pages), can_edit: server.rights.admin_project, rights: { ...server.rights }, dashboard_slug: "dashboard",
  ...(server.enabled ? { capabilities: { project_pages_v2: true } } : {}),
});
const snapshot = (): ProjectLayout => ({ revision: server.revision, pages: structuredClone(server.pages) });

function seed() {
  server.enabled = true; server.rights = admin; server.revision = 4; server.fail = false;
  server.pages = [
    page("dashboard", 0, [{ id: "intro", kind: "markdown", x: 0, y: 0, w: 12, h: 2, text: "Hello" }]),
    page("board", 1, [viewport("board-0", "board", "lanes"), viewport("board-1", "board", "columns", 12)], "board"),
    page("tests", 2, [viewport("tests-0", "tests", "runs")], "tests"),
    page("notes", 3, []),
  ];
}

beforeEach(() => {
  seed();
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  window.requestAnimationFrame ??= (fn => setTimeout(fn, 0)) as typeof requestAnimationFrame;
  window.cancelAnimationFrame ??= (id => clearTimeout(id)) as typeof cancelAnimationFrame;
  Object.defineProperty(HTMLElement.prototype, "scrollTo", { value: () => {}, configurable: true });
  vi.spyOn(api.pages, "list").mockImplementation(async () => { if (server.fail) throw new Error("offline"); return list(); });
  vi.spyOn(api.pages, "get").mockImplementation(async (_p, slug) => structuredClone(server.pages.find(item => item.slug === slug)!));
  vi.spyOn(api.pages, "layout").mockImplementation(async () => snapshot());
});
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); listeners.clear(); });

function mount(path: string) {
  const history = createMemoryHistory(); history.set({ value: path });
  const host = document.createElement("div"); document.body.append(host);
  dispose = render(() => <MemoryRouter history={history}>
    <Route path="/p/:project" component={ProjectScreen}>
      <Route path="/pages/:slug" component={CustomPage} />
      <Route path="/board" component={() => <p>Legacy board route</p>} />
      <Route path="/tests/:id?" component={() => <p>Legacy tests route</p>} />
      <Route path="/settings/*rest" component={() => <p>Project settings view</p>} />
    </Route>
  </MemoryRouter>, host);
  return { host, history };
}
const nav = (host: HTMLElement) => host.querySelector<HTMLElement>("nav.project-tabs")!;
const hrefs = (host: HTMLElement) => [...nav(host).querySelectorAll("a")].map(a => a.getAttribute("href"));
const link = (host: HTMLElement, href: string) => nav(host).querySelector<HTMLAnchorElement>(`a[href="${href}"]`);
const widgetEl = (host: HTMLElement, id: string) => host.querySelector<HTMLElement>(`[data-widget-id="${id}"]`);
const button = (root: Element, text: string) => [...root.querySelectorAll("button")].find(b => b.textContent?.trim() === text || b.getAttribute("aria-label") === text);
const emit = (event: Partial<AgentEvent>) => listeners.forEach(fn => fn({ seq: 1, id: "e", at: "", summary: "", data: {}, ...event } as AgentEvent));

it("ON: the stored pages replace the fixed tabs, the cog opens settings, legacy routes keep their built-in tab", async () => {
  const { host, history } = mount(`/p/${P}/board?card=TER-1`);
  await waitFor(() => expect(link(host, `/p/${P}/pages/board`)).toBeTruthy());
  expect(host.textContent).toContain("Legacy board route");
  // The old route with its query opens the old view, and the Board page is the active tab.
  expect(link(host, `/p/${P}/pages/board`)!.classList.contains("active")).toBe(true);
  expect(hrefs(host)).toEqual(expect.arrayContaining([`/p/${P}/pages/dashboard`, `/p/${P}/pages/tests`, `/p/${P}/pages/notes`]));
  for (const fixed of ["board", "tests", "schedules", "rules", "agents", "repos", "knowledge", "pipelines", "calendar", "logs"]) {
    expect(hrefs(host)).not.toContain(`/p/${P}/${fixed}`);
  }
  // Settings is no page and no tab: one cog next to Edit pages.
  const cog = nav(host).querySelector<HTMLAnchorElement>('a[aria-label="Project settings"]')!;
  expect(cog.getAttribute("href")).toBe(`/p/${P}/settings`);
  expect([...nav(host).querySelectorAll("a")].some(a => a.textContent === "Settings")).toBe(false);
  expect(cog.nextElementSibling?.textContent).toBe("Edit pages");
  history.set({ value: `/p/${P}/tests/RUN1` });
  await waitFor(() => expect(host.textContent).toContain("Legacy tests route"));
  expect(link(host, `/p/${P}/pages/tests`)!.classList.contains("active")).toBe(true);
  expect(link(host, `/p/${P}/pages/board`)!.classList.contains("active")).toBe(false);
  history.set({ value: `/p/${P}/settings/general` });
  await waitFor(() => expect(host.textContent).toContain("Project settings view"));
  expect(cog.classList.contains("active")).toBe(true);
});

it("OFF: the fixed tabs and Settings stay, built-in pages of a rollback stay hidden, custom pages stay", async () => {
  server.enabled = false;
  const { host } = mount(`/p/${P}/board`);
  await waitFor(() => expect(link(host, `/p/${P}/pages/notes`)).toBeTruthy());
  expect(hrefs(host)).toEqual(expect.arrayContaining([`/p/${P}/board`, `/p/${P}/schedules`, `/p/${P}/logs`, `/p/${P}/settings`, `/p/${P}/pages/dashboard`]));
  expect(link(host, `/p/${P}/pages/board`)).toBeNull();
  expect(link(host, `/p/${P}/pages/tests`)).toBeNull();
  expect(nav(host).querySelector('a[aria-label="Project settings"]')).toBeNull();
  expect(button(nav(host), "Edit pages")).toBeUndefined();
  expect(link(host, `/p/${P}/board`)!.classList.contains("active")).toBe(true);
});

it("OFF: a page with feature widgets shows Not Found for them and mounts no panel", async () => {
  server.enabled = false;
  const board = vi.spyOn(api.board, "get");
  const { host } = mount(`/p/${P}/pages/board`);
  await waitFor(() => expect(widgetEl(host, "board-0")?.textContent).toContain("Not Found"));
  expect(widgetEl(host, "board-1")?.textContent).toContain("Not Found");
  expect(board).not.toHaveBeenCalled();
});

it("ON: the host renders every registry part from the composed maps, and Not Found for an unknown view", async () => {
  const widgets = Object.entries(PROJECT_WIDGET_VIEWS).flatMap(([kind, views]) =>
    (views as readonly string[]).map(view => viewport(`${kind}-${view}`.replace(/_/g, "-"), kind as PageWidget["kind"], view)));
  widgets.forEach((w, i) => { w.y = i * 12; });
  widgets.push({ ...viewport("bad-view", "board", "runs"), y: widgets.length * 12 });
  server.pages = [page("all", 0, widgets)];
  const { host } = mount(`/p/${P}/pages/all`);
  await waitFor(() => expect(widgetEl(host, "bad-view")?.textContent).toBe("Not Found"));
  expect(widgets.length).toBe(28);
  // No part flashed Not Found while the capability loaded: only the bad view shows it.
  expect([...host.querySelectorAll("[data-widget-id]")].filter(el => el.textContent === "Not Found").map(el => el.getAttribute("data-widget-id"))).toEqual(["bad-view"]);
  for (const w of widgets.slice(0, -1)) {
    await waitFor(() => expect(widgetEl(host, w.id)?.textContent?.length ?? 0, w.id).toBeGreaterThan(0));
    expect(widgetEl(host, w.id)!.textContent, w.id).not.toBe("Not Found");
  }
});

it("ON: the widget controls follow the domain rights, never the layout right", async () => {
  // A reader of the project: no layout Edit, a read-only board and columns.
  server.rights = none;
  let { host } = mount(`/p/${P}/pages/board`);
  await waitFor(() => expect(widgetEl(host, "board-0")?.querySelector(".lane, .board-lanes, .card, [data-card]")).toBeTruthy());
  await waitFor(() => expect(widgetEl(host, "board-1")?.querySelector("table.columns-table")).toBeTruthy());
  expect(button(nav(host), "Edit pages")).toBeUndefined();
  expect(button(widgetEl(host, "board-0")!, "Edit columns")).toBeUndefined();
  expect(widgetEl(host, "board-0")!.querySelector('[draggable="true"]')).toBeNull();
  expect(button(widgetEl(host, "board-1")!, "Save")).toBeUndefined();
  dispose!(); dispose = undefined; document.body.replaceChildren();
  // A member writes cards (write_project) but has no column or layout right.
  server.rights = member;
  ({ host } = mount(`/p/${P}/pages/board`));
  await waitFor(() => expect(widgetEl(host, "board-0")!.querySelector('[draggable="true"]')).toBeTruthy());
  expect(button(widgetEl(host, "board-0")!, "Edit columns")).toBeUndefined();
  expect(button(nav(host), "Edit pages")).toBeUndefined();
  dispose!(); dispose = undefined; document.body.replaceChildren();
  // A project admin: Edit columns and the layout editor.
  server.rights = admin;
  ({ host } = mount(`/p/${P}/pages/board`));
  await waitFor(() => expect(button(widgetEl(host, "board-0")!, "Edit columns")).toBeTruthy());
  expect(button(nav(host), "Edit pages")).toBeTruthy();
});

it("ON: a failed rights read leaves the widgets read-only", () => {
  const grants = pageGrants(undefined, P);
  expect(Object.values(grants.boardTestsAgents).some(Boolean)).toBe(false);
  expect(grants.operations.createSchedule || grants.operations.createEventSource || grants.operations.manageRules || grants.operations.managePipelines).toBe(false);
  expect(grants.operations.manageSchedule!({ project: P } as never)).toBe(false);
});

it("ON: a page.updated and a rights change keep the mounted widget, its DOM and focus (no flicker, no jump)", async () => {
  server.rights = member;
  const { host } = mount(`/p/${P}/pages/board`);
  await waitFor(() => expect(button(widgetEl(host, "board-0")!, "Filters")).toBeTruthy());
  const section = widgetEl(host, "board-0")!;
  const filters = button(section, "Filters")!;
  filters.focus();
  const reads = vi.mocked(api.pages.list).mock.calls.length;
  // Someone renames another page and makes this caller an admin.
  server.pages[3] = { ...server.pages[3]!, title: "Renamed" };
  server.rights = admin;
  emit({ kind: "page.updated", project: P, data: { revision: 5 } });
  await waitFor(() => expect(vi.mocked(api.pages.list).mock.calls.length).toBeGreaterThan(reads));
  await waitFor(() => expect(button(section, "Edit columns")).toBeTruthy());
  await waitFor(() => expect(link(host, `/p/${P}/pages/notes`)?.textContent).toBe("Renamed"));
  expect(widgetEl(host, "board-0")).toBe(section);
  expect(button(section, "Filters")).toBe(filters);
  expect(document.activeElement).toBe(filters);
  // The rights shrink again: the column control goes, the widget stays.
  server.rights = member;
  emit({ kind: "page.updated", project: P, data: { revision: 6 } });
  await waitFor(() => expect(button(section, "Edit columns")).toBeUndefined());
  expect(widgetEl(host, "board-0")).toBe(section);
  expect(document.activeElement).toBe(filters);
});

it("ON: a missing built-in is added again and a present one reset, with the draft's base revision", async () => {
  const reset = vi.spyOn(api.pages, "resetLayout").mockImplementation(async (_p, body) => {
    expect(body.base_revision).toBe(server.revision);
    server.revision += 1;
    server.pages = [...server.pages, page("rules", server.pages.length, [viewport("rules-0", "rules", "flow")], "rules")];
    return snapshot();
  });
  const { host } = mount(`/p/${P}/pages/notes`);
  await waitFor(() => expect(button(nav(host), "Edit pages")).toBeTruthy());
  button(nav(host), "Edit pages")!.click();
  await waitFor(() => expect(nav(host).querySelectorAll(".project-tab-edit").length).toBe(4));
  const select = [...nav(host).querySelectorAll("select")].find(s => s.closest("label")?.textContent?.startsWith("Built-in page"))!;
  const options = [...select.options].map(o => o.textContent);
  expect(options).toContain("Board (reset to default)");
  expect(options).toContain("Rules (add again)");
  select.value = "rules"; select.dispatchEvent(new Event("change", { bubbles: true }));
  button(nav(host), "Restore default")!.click();
  await waitFor(() => expect(reset).toHaveBeenCalledWith(P, { base_revision: 4, builtin: "rules" }));
  await waitFor(() => expect([...nav(host).querySelectorAll<HTMLElement>(".project-tab-edit")].map(t => t.dataset.slug)).toContain("rules"));
  // A dirty draft must be saved or cancelled first: the reset would discard it.
  const title = nav(host).querySelector<HTMLInputElement>('input[aria-label="Title of notes"]')!;
  title.value = "Changed"; title.dispatchEvent(new Event("input", { bubbles: true }));
  select.value = "board"; select.dispatchEvent(new Event("change", { bubbles: true }));
  await waitFor(() => expect(button(nav(host), "Restore default")!.disabled).toBe(true));
  expect(reset).toHaveBeenCalledTimes(1);
});

it("ON: Copy layout replaces the whole layout only after an explicit confirmation", async () => {
  const copy = vi.spyOn(api.pages, "copyLayout").mockImplementation(async (_p, body) => {
    server.revision += 1;
    server.pages = [page("source", 0, [], undefined)];
    expect(body).toEqual({ base_revision: 4, source_project: "other", confirm: true });
    return snapshot();
  });
  const confirm = vi.fn(() => false);
  vi.stubGlobal("confirm", confirm);
  const { host, history } = mount(`/p/${P}/pages/notes`);
  await waitFor(() => expect(button(nav(host), "Edit pages")).toBeTruthy());
  button(nav(host), "Edit pages")!.click();
  await waitFor(() => expect(button(nav(host), "Copy layout")).toBeTruthy());
  const from = [...nav(host).querySelectorAll("input")].find(i => i.closest("label")?.textContent?.startsWith("Copy layout from project"))!;
  from.value = "other"; from.dispatchEvent(new Event("input", { bubbles: true }));
  button(nav(host), "Copy layout")!.click();
  expect(confirm).toHaveBeenCalledTimes(1);
  expect(copy).not.toHaveBeenCalled();
  confirm.mockReturnValue(true);
  button(nav(host), "Copy layout")!.click();
  await waitFor(() => expect(copy).toHaveBeenCalledTimes(1));
  await waitFor(() => expect([...nav(host).querySelectorAll<HTMLElement>(".project-tab-edit")].map(t => t.dataset.slug)).toEqual(["source"]));
  // The open page is gone from the layout: the editor goes to the first page.
  await waitFor(() => expect(history.get()).toBe(`/p/${P}/pages/source`));
});

it("ON: the mail widget asks the daemon for this project's mail only", async () => {
  const mail = vi.spyOn(api.mail, "list");
  server.pages = [page("logs", 0, [viewport("logs-0", "mail", "messages")], "logs")];
  const { host } = mount(`/p/${P}/pages/logs`);
  await waitFor(() => expect(mail).toHaveBeenCalled());
  expect(widgetEl(host, "logs-0")!.textContent).not.toBe("Not Found");
  for (const [query] of mail.mock.calls) expect(query).toMatchObject({ project: P });
});

it("opens the Board page, else the first page, else the Board route", () => {
  seed();
  expect(projectIndexPath(list())).toBe("pages/board");
  server.pages = [page("notes", 1, []), page("dashboard", 0, [])];
  expect(projectIndexPath(list())).toBe("pages/dashboard");
  server.pages = [];
  expect(projectIndexPath(list())).toBe("board");
  seed(); server.enabled = false;
  expect(projectIndexPath(list())).toBe("board");
  expect(projectIndexPath(undefined)).toBe("board");
});

it("maps each grant to one existing right, with the project and global scopes apart", () => {
  const sys = pageGrants({ ...admin, manage_global: true }, P);
  const lead = pageGrants(admin, P);
  const own = { project: P }, other = { project: "q" }, global = { project: null };
  expect(lead.boardTestsAgents).toEqual({ canWriteCards: true, canEditColumns: true, canCreateAgent: true, canPinAgents: false });
  expect(pageGrants(member, P).boardTestsAgents).toEqual({ canWriteCards: true, canEditColumns: false, canCreateAgent: false, canPinAgents: false });
  expect(pageGrants({ ...none, operator: true }, P).boardTestsAgents.canPinAgents).toBe(true);
  expect([lead.operations.manageSchedule!(own as never), lead.operations.manageSchedule!(global as never), lead.operations.manageSchedule!(other as never)]).toEqual([true, false, false]);
  expect([sys.operations.manageSchedule!(global as never), sys.operations.manageEventSource!(global as never)]).toEqual([true, true]);
  expect([lead.operations.manageEventSource!(own as never), lead.operations.manageEventSource!(global as never)]).toEqual([true, false]);
  expect(pageGrants({ ...member, manage_schedules: true }, P).operations.createEventSource).toBe(false);
});

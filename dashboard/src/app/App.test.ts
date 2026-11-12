// @vitest-environment jsdom
// Mount the real App route table with inert pages. These tests catch route
// nesting errors that raw source assertions and ProjectScreen alone cannot.

import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../test/wait";
import { MOVED_ROUTES, movedTo, PROJECT_TABS } from "./nav";

const { stub } = vi.hoisted(() => ({ stub: (label: string) => () => label }));
const dashboardRoute = vi.hoisted(() => ({ slug: "dashboard" }));

vi.mock("../api", () => ({ isMock: false }));
// The project shell reads the tab counts (TER-154): no agent, no count.
vi.mock("../stores", async () => { const { instanceTestStores } = await import("../test/instance"); return instanceTestStores(({
  auth: { mode: () => "operator", token: () => null, csrf: () => null },
  agents: { inProject: () => [] },
  api: { projects: { counts: async () => ({ blocked_cards: 0, questions: null }) }, pages: { list: async () => ({ items: [], can_edit: false, dashboard_slug: dashboardRoute.slug }) } },
  events: { on: () => () => {} },
})); });
vi.mock("./Layout", () => ({ Layout: (props: { children?: unknown }) => props.children }));
vi.mock("./LoginScreen", () => ({ LoginScreen: stub("Login") }));
vi.mock("../features/home/Home", () => ({ Home: stub("Home page") }));
vi.mock("../features/agents/AgentsPage", () => ({ AgentsPage: stub("Agents page") }));
vi.mock("../features/agents/AgentDetail", () => ({ default: stub("Agent page") }));
vi.mock("../features/board/BoardPage", () => ({ BoardPage: stub("Cards page"), DoneCardsPage: stub("Done cards page") }));
vi.mock("../features/projects/ProjectsPage", () => ({ default: stub("Projects page") }));
vi.mock("../features/projects/ProjectSettings", () => ({ default: stub("General page") }));
vi.mock("../features/pages/CustomPage", () => ({ default: stub("Custom page") }));
vi.mock("../features/dashboard/MonitorsSettings", () => ({ default: stub("Monitors page") }));
vi.mock("../features/pipelines/PipelinesPage", () => ({ default: stub("Pipelines page") }));
vi.mock("../features/calendar/CalendarPage", () => ({ default: stub("Calendar page") }));
vi.mock("../features/repos/ReposPage", () => ({ default: stub("Repos page") }));
vi.mock("../features/repos/RepoPage", () => ({ default: stub("Repo page") }));
vi.mock("../features/artifacts/ArtifactsPage", () => ({ default: stub("Artifacts page") }));
vi.mock("../features/tests/TestRunsPage", () => ({ default: stub("Tests page") }));
vi.mock("../features/tests/TestRunPage", () => ({ default: stub("Test run page") }));
vi.mock("../features/schedules/SchedulesPage", () => ({ default: stub("Schedules page") }));
vi.mock("../features/rules/RulesPage", () => ({ default: stub("Rules page") }));
vi.mock("../features/wiki/WikiPage", () => ({ default: stub("Wiki page") }));
vi.mock("../features/memory/MemoryPage", () => ({ default: stub("Memory page") }));
vi.mock("../features/db/DbTypesPage", () => ({ default: stub("DB page") }));
vi.mock("../features/db/DbRecordsPage", () => ({ default: stub("DB records page") }));
vi.mock("../features/mail/MailPage", () => ({ default: stub("Mail page") }));
vi.mock("../features/events/EventsPage", () => ({ default: stub("Events page") }));
vi.mock("../features/audit/AuditPage", () => ({ default: stub("Audit page") }));
vi.mock("../features/inbox/InboxPage", () => ({ default: stub("Inbox page") }));
vi.mock("../features/settings/AccountPage", () => ({ default: stub("Account page") }));
vi.mock("../features/subscriptions/SubscriptionsPage", () => ({ default: stub("Subscriptions page") }));
vi.mock("../features/secrets/SecretsPage", () => ({ default: stub("Secrets page") }));
vi.mock("../features/templates/TemplatesPage", () => ({ default: stub("Templates page") }));
vi.mock("../features/systemprompts/SystemPromptsPage", () => ({ default: stub("System prompts page") }));
vi.mock("../features/skills/SkillsPage", () => ({ default: stub("Skills page") }));
vi.mock("../features/tools/ToolsPage", () => ({ default: stub("Tools page") }));
vi.mock("../features/targets/TargetsPage", () => ({ default: stub("Targets page") }));
vi.mock("../features/config/ConfigPage", () => ({ default: stub("Harness page") }));
vi.mock("../features/hosts/HostsPage", () => ({ default: stub("Hosts page") }));
vi.mock("../features/settings/SoftwarePage", () => ({ default: stub("Software page") }));
vi.mock("../features/inbox/ThreadPage", () => ({ default: stub("Thread page") }));
vi.mock("../features/sessions/SessionPage", () => ({ default: stub("Session page") }));
vi.mock("./ManageBookmarks", () => ({ default: stub("Bookmarks page") }));

const { App } = await import("./App");
let dispose: (() => void) | undefined;

afterEach(() => {
  dashboardRoute.slug = "dashboard";
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
  window.history.replaceState({}, "", "/");
  vi.unstubAllGlobals();
});

function mount(path: string) {
  window.history.replaceState({}, "", path);
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(() => createComponent(App, {}), root);
  return root;
}

function activeProjectTab(root: HTMLElement) {
  return Array.from(root.querySelectorAll('nav[aria-label="Project"] a.active')).map((a) => a.textContent);
}

describe("App project routes", () => {
  it("mounts a remote page with its identity and keeps index redirects remote", async () => {
    const fetch = vi.fn(async () => new Response(JSON.stringify({ name: "lab", url: "https://lab.example", status: "version_skew", read_only: true })));
    vi.stubGlobal("fetch", fetch);
    const root = mount("/remote/lab/p/demo");
    await waitFor(() => expect(root.textContent).toContain("Cards page"));
    expect(window.location.pathname).toBe("/remote/lab/p/demo/board");
    expect(root.textContent).toContain("Remote: lab");
    expect(root.textContent).toContain("Version differs from home");
    expect(document.title).toBe("[lab] Terrarium");
    expect(activeProjectTab(root)).toEqual(["Board"]);
    const board = root.querySelector('nav[aria-label="Project"] a');
    expect(board?.getAttribute("href")).toMatch(/^\/remote\/lab\//);
    expect(fetch).toHaveBeenCalledWith("/api/remotes/lab", expect.anything());
  });

  it("does not mount home content when the remote does not exist", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response('{"error":"remote not found"}', { status: 404 })));
    const root = mount("/remote/missing/p/demo/board");
    await waitFor(() => expect(root.textContent).toContain("remote not found"));
    expect(root.textContent).not.toContain("Cards page");
  });

  it("keeps remote project names, queries and hashes on legacy redirects", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ name: "lab", url: "https://lab.example", status: "online" }))));
    const root = mount("/remote/lab/p/my%20app/board/tests/R1?filter=failed#result");
    await waitFor(() => expect(root.textContent).toContain("Test run page"));
    expect(window.location.pathname).toBe("/remote/lab/p/my%20app/tests/R1");
    expect(window.location.search).toBe("?filter=failed");
    expect(window.location.hash).toBe("#result");
  });
  it("opens Board at the project index in the real route tree", async () => {
    const root = mount("/p/my%20app");
    await waitFor(() => expect(root.textContent).toContain("Cards page"));
    expect(window.location.pathname).toBe("/p/my%20app/board");
    expect(root.querySelector("h1")?.textContent).toBe("my app");
    expect(activeProjectTab(root)).toEqual(["Board"]);
  });

  it("opens every project tab and its real child route", async () => {
    const cases = [
      ["/p/my%20app/board", "Cards page"],
      ["/p/my%20app/tests", "Tests page"],
      ["/p/my%20app/schedules", "Schedules page"],
      ["/p/my%20app/rules", "Rules page"],
      ["/p/my%20app/agents", "Agents page"],
      ["/p/my%20app/repos", "Repos page"],
      ["/p/my%20app/knowledge/wiki/a/b", "Wiki page"],
      ["/p/my%20app/pipelines", "Pipelines page"],
      ["/p/my%20app/calendar", "Calendar page"],
      ["/p/my%20app/logs/mail", "Mail page"],
      ["/p/my%20app/settings/secrets", "Secrets page"],
    ] as const;
    for (const [index, [path, page]] of cases.entries()) {
      const root = mount(path);
      await waitFor(() => expect(root.textContent).toContain(page));
      expect(root.textContent).not.toContain("Page not found.");
      expect(root.querySelector("h1")?.textContent).toBe("my app");
      expect(activeProjectTab(root)).toEqual([PROJECT_TABS[index]!.label]);
      dispose?.();
      document.body.replaceChildren();
    }
  });

  it("redirects the former Dashboard route to its custom page", async () => {
    const root = mount("/p/my%20app/dashboard");
    await waitFor(() => expect(root.textContent).toContain("Custom page"));
    expect(window.location.pathname).toBe("/p/my%20app/pages/dashboard");
  });

  it("uses the recorded fallback slug when a user already owns dashboard", async () => {
    dashboardRoute.slug = "dashboard-legacy-2";
    const root = mount("/p/my%20app/dashboard");
    await waitFor(() => expect(root.textContent).toContain("Custom page"));
    expect(window.location.pathname).toBe("/p/my%20app/pages/dashboard-legacy-2");
  });

  it("keeps the nested tab rows encoded and active on deep paths", async () => {
    const cases = [
      ["/p/my%20app/knowledge/wiki/a/b", "Wiki page", "Knowledge", "Wiki", "/p/my%20app/knowledge/wiki"],
      ["/p/my%20app/logs/mail", "Mail page", "Logs", "Mail", "/p/my%20app/logs/mail"],
      ["/p/my%20app/settings/secrets", "Secrets page", "Settings", "Secrets", "/p/my%20app/settings/secrets"],
      ["/p/my%20app/dashboard?tab=monitors", "Monitors page", "Settings", "Monitors", "/p/my%20app/settings/monitors"],
    ] as const;
    for (const [path, page, label, tab, href] of cases) {
      const root = mount(path);
      await waitFor(() => expect(root.textContent).toContain(page));
      const links = Array.from(root.querySelectorAll<HTMLAnchorElement>(`nav[aria-label="${label}"] a`));
      expect(links.find((a) => a.textContent === tab)?.getAttribute("href")).toBe(href);
      expect(links.filter((a) => a.classList.contains("active")).map((a) => a.textContent)).toEqual([tab]);
      expect(root.querySelector("h2")?.textContent).toBe(`${label} · my app`);
      dispose?.();
      document.body.replaceChildren();
    }
  });

  it("shows one tab row on the board and its former tabs (TER-507)", async () => {
    const cases = [
      ["/p/my%20app/board", "Cards page", "Board"],
      ["/p/my%20app/board/done", "Done cards page", "Board"],
      ["/p/my%20app/board/artifacts/ART-2", "Artifacts page", "Board"],
      ["/p/my%20app/tests/1", "Test run page", "Tests"],
      ["/p/my%20app/schedules", "Schedules page", "Schedules"],
      ["/p/my%20app/rules", "Rules page", "Rules"],
    ] as const;
    for (const [path, page, tab] of cases) {
      const root = mount(path);
      await waitFor(() => expect(root.textContent).toContain(page));
      expect(root.querySelectorAll("nav.tabs"), path).toHaveLength(1);
      expect(root.querySelector('nav[aria-label="Board"]'), path).toBeNull();
      expect(activeProjectTab(root), path).toEqual([tab]);
      expect(Array.from(root.querySelectorAll("nav.tabs a")).map((a) => a.textContent)).not.toContain("Artifacts");
      dispose?.();
      document.body.replaceChildren();
    }
  });

  it.each(["terrarium", "my%20app"])("resolves every moved route to a real destination for %s", async (project) => {
    for (const route of MOVED_ROUTES) {
      const from = route.path.replace(":project", project).replace(":id", "x1").replace(":name", "code-review").replace("*path", "a/b");
      const to = movedTo(from)!;
      const root = mount(from);
      await waitFor(() => expect(window.location.pathname).toBe(to));
      expect(root.textContent, from).not.toContain("Page not found.");
      dispose?.();
      document.body.replaceChildren();
    }
  });

  it("renders a project URL with bad percent-encoding", async () => {
    const root = mount("/p/a%zz/board");
    await waitFor(() => expect(root.textContent).toContain("Cards page"));
    expect(root.querySelector("h1")?.textContent).toBe("a%zz");
    // The board has no screen title of its own (TER-507): the project title is the only one.
    expect(root.querySelector("h2")).toBeNull();
    expect(window.location.pathname).toBe("/p/a%25zz/board");
    expect(root.textContent).not.toContain("Page not found.");
  });
});

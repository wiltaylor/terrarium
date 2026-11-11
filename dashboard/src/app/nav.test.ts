import { describe, expect, it } from "vitest";
import app from "./App.tsx?raw";
import layout from "./Layout.tsx?raw";
import nav from "./nav.ts?raw";
import { inboxRedirect, KNOWLEDGE_TABS, LOGS_TABS, MOVED_ROUTES, movedTo, PROJECT_TABS, withQuery, PROJECT_LOGS_TABS, PROJECT_SETTINGS_TABS, SETTINGS_ITEMS, SETTINGS_TABS, TOP_ITEMS } from "./nav";

describe("navigation", () => {
  it("has no Spend item; the project has Logs, not Inbox or Mail, with a route", () => {
    const paths = PROJECT_TABS.map((i) => i.path) as string[];
    expect(paths).not.toContain("spend");
    expect(paths).not.toContain("inbox");
    expect(paths).not.toContain("mail");
    expect(PROJECT_TABS.find((i) => i.path === "logs")?.label).toBe("Logs");
    expect(app).toMatch(/<Route path="\/logs" component=\{LogsPage\}>/);
  });

  it("has one Logs item where Mail was, and the logs screen has the tabs Mail, Events, Audit, each with a route", () => {
    const hrefs = TOP_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/logs")).toBe(hrefs.indexOf("/threads") + 1);
    expect(TOP_ITEMS.find((i) => i.href === "/logs")?.label).toBe("Logs");
    for (const gone of ["/mail", "/events", "/audit"]) expect(hrefs).not.toContain(gone);
    expect(LOGS_TABS.map((t) => t.label)).toEqual(["Mail", "Events", "Audit"]);
    expect(app).toMatch(/<Route path="\/logs" component=\{LogsPage\}>/);
    expect(app).toMatch(/path="\/mail" component=\{MailPage\}/);
    expect(app).toMatch(/path="\/events" component=\{EventsPage\}/);
    expect(app).toMatch(/path="\/audit" component=\{AuditPage\}/);
    for (const t of LOGS_TABS) expect(app).toContain(`path="/${t.path}"`);
  });

  it("has the tabs Mail and Events on the project logs screen, each with a route under the project", () => {
    expect(PROJECT_LOGS_TABS.map((t) => t.label)).toEqual(["Mail", "Events"]);
    const project = app.slice(app.indexOf('<Route path="/p/:project" component={ProjectScreen}>'));
    const start = project.indexOf('<Route path="/logs" component={LogsPage}>');
    const block = project.slice(start, project.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/mail" component=\{MailPage\}/);
    expect(block).toMatch(/path="\/events" component=\{EventsPage\}/);
    expect(block).not.toMatch(/AuditPage/);
  });

  it("has a project shell with Board first and every former view as a route tab", () => {
    expect(PROJECT_TABS).toEqual([
      { path: "board", label: "Board" },
      { path: "tests", label: "Tests" },
      { path: "schedules", label: "Schedules" },
      { path: "rules", label: "Rules" },
      { path: "dashboard", label: "Dashboard" },
      { path: "agents", label: "Agents" },
      { path: "repos", label: "Repos" },
      { path: "knowledge", label: "Knowledge" },
      { path: "pipelines", label: "Pipelines" },
      { path: "calendar", label: "Calendar" },
      { path: "logs", label: "Logs" },
      { path: "settings", label: "Settings" },
    ]);
    expect(app).toMatch(/<Route path="\/p\/:project" component=\{ProjectScreen\}>/);
    expect(app).toMatch(/<Route path="\/" component=\{ProjectIndex\} \/>/);
    expect(app).toMatch(/<Navigate href=\{`\/p\/\$\{params\.project\}\/board`\} \/>/);
    expect(app).not.toMatch(/<Navigate href=\{`\/p\/\$\{params\.project\}\/dashboard`\} \/>/);
  });

  it("has an Agents project tab with a route to the Agents page", () => {
    expect(PROJECT_TABS.find((i) => i.path === "agents")?.label).toBe("Agents");
    expect(app).toMatch(/path="\/agents" component=\{AgentsPage\}/);
  });

  it("has no global Agents item or page: the agents are in the project tree only, but the agent detail keeps its route", () => {
    expect([...TOP_ITEMS, ...SETTINGS_ITEMS].map((i) => i.href)).not.toContain("/agents");
    expect(layout).not.toMatch(/AgentTree/);
    expect(Object.keys(import.meta.glob("./AgentTree.tsx"))).toEqual([]);
    const global = app.slice(0, app.indexOf('<Route path="/p/:project" component={ProjectScreen}>'));
    expect(global).not.toMatch(/path="\/agents" component=/);
    expect(app).toMatch(/path="\/agents\/:id" component=\{AgentDetail\}/);
  });

  it("redirects the old global Agents page to the project cards, but not an agent page", () => {
    expect(movedTo("/agents")).toBe("/projects");
    expect(movedTo("/agents/")).toBe("/projects");
    expect(movedTo("/agents/a1")).toBeUndefined();
    expect(movedTo("/p/terrarium/agents")).toBeUndefined();
  });

  it("has General, Secrets, and Templates as tabs of the project settings screen, each with a route", () => {
    expect(PROJECT_SETTINGS_TABS).toEqual([
      { path: "general", label: "General" },
      { path: "secrets", label: "Secrets" },
      { path: "templates", label: "Templates" },
    ]);
    const project = app.slice(app.indexOf('<Route path="/p/:project" component={ProjectScreen}>'));
    expect(project).toMatch(/<Route path="\/settings" component=\{SettingsPage\}>/);
    const start = project.indexOf('<Route path="/settings" component={SettingsPage}>');
    const block = project.slice(start, project.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/general" component=\{ProjectSettings\}/);
    expect(block).toMatch(/path="\/secrets" component=\{SecretsPage\}/);
    expect(block).toMatch(/path="\/templates" component=\{TemplatesPage\}/);
    for (const t of PROJECT_SETTINGS_TABS) expect(block).toContain(`path="/${t.path}"`);
    expect(project).not.toMatch(/path="\/settings" component=\{ProjectSettings\}/);
  });

  it("redirects the old project Secrets, Templates, and Settings paths to the project settings tabs", () => {
    expect(movedTo("/p/terrarium/secrets")).toBe("/p/terrarium/settings/secrets");
    expect(movedTo("/p/my%20app/templates/")).toBe("/p/my%20app/settings/templates");
    expect(movedTo("/p/terrarium/settings")).toBe("/p/terrarium/settings/general");
    expect(movedTo("/p/terrarium/settings/")).toBe("/p/terrarium/settings/general");
    for (const now of ["/p/terrarium/settings/general", "/p/terrarium/settings/secrets", "/p/terrarium/settings/templates"]) expect(movedTo(now)).toBeUndefined();
    // The global pages are tabs of the settings screen.
    expect(movedTo("/secrets")).toBe("/settings/secrets");
    expect(movedTo("/templates")).toBe("/settings/templates");
  });

  it("has a Repos project tab with a list route and a repo route", () => {
    expect(PROJECT_TABS.find((i) => i.path === "repos")?.label).toBe("Repos");
    expect(app).toMatch(/path="\/repos" component=\{ReposPage\}/);
    expect(app).toMatch(/path="\/repos\/:repo" component=\{RepoPage\}/);
    expect(movedTo("/p/terrarium/repos")).toBeUndefined();
  });

  it("has Schedules after Logs in the top list with a route, and the project schedules as a project tab", () => {
    expect(PROJECT_TABS.find((i) => i.path === "schedules")?.label).toBe("Schedules");
    const hrefs = TOP_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/schedules")).toBe(hrefs.indexOf("/logs") + 1);
    expect(app).toMatch(/path="\/schedules" component=\{SchedulesPage\}/);
  });

  it("has Home, Inbox, Threads, Logs, Schedules, then Calendar above the project tree, with a route, and no Questions item", () => {
    expect(TOP_ITEMS.map((i) => i.href)).toEqual(["/", "/inbox", "/threads", "/logs", "/schedules", "/calendar"]);
    expect(TOP_ITEMS.find((i) => i.href === "/inbox")?.label).toBe("Inbox");
    expect(TOP_ITEMS.map((i) => i.href)).not.toContain("/questions");
    expect(app).toMatch(/path="\/inbox" component=\{InboxPage\}/);
    // The combined calendar and the calendar of a project use one page (docs/api.md §33).
    expect(app).toMatch(/path="\/calendar" component=\{CalendarPage\}/);
    expect(app).toMatch(/path="\/calendar" component=\{CalendarPage\}/);
    expect(app).not.toMatch(/path="\/questions" component/);
    expect(Object.keys(import.meta.glob("../features/questions/**"))).toEqual([]);
  });

  it("has no Secrets, Templates, Skills, or Targets item in the sidebar: they are tabs of the settings screen, each with a route", () => {
    const hrefs = [...TOP_ITEMS, ...SETTINGS_ITEMS].map((i) => i.href) as string[];
    for (const gone of ["/secrets", "/templates", "/skills", "/targets"]) expect(hrefs).not.toContain(gone);
    const start = app.indexOf('<Route path="/settings"');
    const block = app.slice(start, app.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/secrets" component=\{SecretsPage\}/);
    expect(block).toMatch(/path="\/templates" component=\{TemplatesPage\}/);
    // The Skills tab keeps the editor of one skill.
    expect(block).toMatch(/path="\/skills\/:name\?" component=\{SkillsPage\}/);
    expect(block).toMatch(/path="\/targets" component=\{TargetsPage\}/);
    for (const old of ["secrets", "templates", "skills[^\"]*", "targets"]) expect(app).not.toMatch(new RegExp(`\\n {8}<Route path="/${old}" component=`));
  });

  it("redirects the old Secrets, Templates, Skills, and Targets paths to the settings tabs, with the skill name", () => {
    expect(movedTo("/secrets")).toBe("/settings/secrets");
    expect(movedTo("/secrets/")).toBe("/settings/secrets");
    expect(movedTo("/templates")).toBe("/settings/templates");
    expect(movedTo("/skills")).toBe("/settings/skills");
    expect(movedTo("/skills/")).toBe("/settings/skills");
    expect(movedTo("/skills/code-review")).toBe("/settings/skills/code-review");
    expect(movedTo("/skills/my%20skill/")).toBe("/settings/skills/my%20skill");
    expect(movedTo("/targets")).toBe("/settings/targets");
    expect(movedTo("/skills/a/b")).toBeUndefined();
    for (const now of ["/settings/secrets", "/settings/templates", "/settings/skills", "/settings/skills/code-review", "/settings/targets"]) expect(movedTo(now)).toBeUndefined();
  });

  it("orders the sidebar: Home, Inbox, Logs, and Schedules, the project tree, then Settings; no Terrarium section", () => {
    const at = (s: string) => layout.indexOf(s);
    expect(at("TOP_ITEMS}")).toBeGreaterThan(0);
    expect(at("TOP_ITEMS}")).toBeLessThan(at("<ProjectTree />"));
    expect(at("<ProjectTree />")).toBeLessThan(at("SETTINGS_ITEMS}"));
    expect(layout).not.toMatch(/GLOBAL_ITEMS|<h4>Terrarium<\/h4>/);
    expect(at("SETTINGS_ITEMS}")).toBeLessThan(at('class="sidebar-foot"'));
    // The Projects root of the tree links the cards; no flat Projects item.
    const hrefs = [...TOP_ITEMS, ...SETTINGS_ITEMS].map((i) => i.href) as string[];
    expect(hrefs).not.toContain("/projects");
    expect(app).toMatch(/path="\/projects" component=\{ProjectsPage\}/);
  });

  it("has one Settings item, and the settings screen has its tabs in order, each with a route", () => {
    expect(SETTINGS_ITEMS).toEqual([{ href: "/settings", label: "Settings" }]);
    expect(SETTINGS_TABS.map((t) => t.label)).toEqual(["Account", "Subscriptions", "Secrets", "Templates", "System prompts", "Skills", "Tools", "Targets", "Harness config", "Hosts", "Links", "Software"]);
    const hrefs = TOP_ITEMS.map((i) => i.href) as string[];
    for (const gone of ["/subscriptions", "/hosts", "/config", "/settings"]) expect(hrefs).not.toContain(gone);
    expect(app).toMatch(/<Route path="\/settings" component=\{SettingsPage\}>/);
    expect(app).toMatch(/path="\/account" component=\{AccountPage\}/);
    expect(app).toMatch(/path="\/subscriptions" component=\{SubscriptionsPage\}/);
    expect(app).toMatch(/path="\/harness" component=\{ConfigPage\}/);
    expect(app).toMatch(/path="\/hosts" component=\{HostsPage\}/);
    for (const t of SETTINGS_TABS) expect(app).toMatch(new RegExp(`path="/${t.path}[/"]`));
  });

  it("redirects the old Subscriptions, Hosts, Config, and Settings paths to the settings tabs", () => {
    expect(movedTo("/subscriptions")).toBe("/settings/subscriptions");
    expect(movedTo("/hosts/")).toBe("/settings/hosts");
    expect(movedTo("/config")).toBe("/settings/harness");
    expect(movedTo("/settings")).toBe("/settings/account");
    expect(movedTo("/settings/hosts")).toBeUndefined();
    expect(movedTo("/settings/account")).toBeUndefined();
    // The redirect keeps the query (the add-API-key link opens the wizard).
    expect(app).toMatch(/withQuery\(movedTo\(location\.pathname\) \?\? r\.to, location\.search\)/);
  });

  it("redirects the old Spend route to Home, where the usage split lives", () => {
    expect(movedTo("/p/terrarium/spend")).toBe("/");
    expect(movedTo("/p/my-app/spend/")).toBe("/");
    expect(movedTo("/p/terrarium/board")).toBeUndefined();
    expect(movedTo("/p/terrarium/spend/x")).toBeUndefined();
    expect(movedTo("/spend")).toBeUndefined();
  });

  it("redirects Questions to the Inbox, and the project inbox to the project Mail with the same project", () => {
    expect(movedTo("/questions")).toBe("/inbox");
    expect(movedTo("/questions/")).toBe("/inbox");
    expect(movedTo("/questions/x")).toBeUndefined();
    // Straight to the Mail tab of the project logs: no second redirect.
    expect(movedTo("/p/terrarium/inbox")).toBe("/p/terrarium/logs/mail");
    expect(movedTo("/p/my%20app/inbox/")).toBe("/p/my%20app/logs/mail");
    expect(movedTo(movedTo("/p/terrarium/inbox")!)).toBeUndefined();
    expect(movedTo("/p/terrarium/inbox/x")).toBeUndefined();
    expect(movedTo("/inbox")).toBeUndefined();
    expect(MOVED_ROUTES.map((r) => r.path)).toEqual([
      "/p/:project/spend",
      "/questions",
      "/p/:project/inbox",
      "/subscriptions",
      "/hosts",
      "/config",
      "/settings",
      "/mail",
      "/events",
      "/audit",
      "/logs",
      "/p/:project/mail",
      "/p/:project/logs",
      "/agents",
      "/p/:project/secrets",
      "/p/:project/templates",
      "/p/:project/settings",
      "/p/:project/versions",
      "/p/:project/versions/:id",
      "/p/:project/board/versions",
      "/p/:project/board/versions/:id",
      "/p/:project/artifacts",
      "/p/:project/artifacts/:id",
      "/p/:project/board/schedules",
      "/p/:project/board/rules",
      "/p/:project/board/tests",
      "/p/:project/board/tests/:id",
      "/p/:project/wiki/*path",
      "/p/:project/memory",
      "/p/:project/knowledge",
      "/secrets",
      "/templates",
      "/skills",
      "/skills/:name",
      "/targets",
      "/inbox/threads/:id",
    ]);
  });

  it("redirects the old Mail, Event log, and Audit paths to the logs tabs, and the project Mail to the project logs", () => {
    expect(movedTo("/mail")).toBe("/logs/mail");
    expect(movedTo("/mail/")).toBe("/logs/mail");
    expect(movedTo("/events")).toBe("/logs/events");
    expect(movedTo("/audit")).toBe("/logs/audit");
    expect(movedTo("/logs")).toBe("/logs/mail");
    expect(movedTo("/logs/")).toBe("/logs/mail");
    expect(movedTo("/p/terrarium/mail")).toBe("/p/terrarium/logs/mail");
    expect(movedTo("/p/my%20app/mail/")).toBe("/p/my%20app/logs/mail");
    expect(movedTo("/p/terrarium/logs")).toBe("/p/terrarium/logs/mail");
    for (const now of ["/logs/mail", "/logs/events", "/logs/audit", "/p/terrarium/logs/mail", "/p/terrarium/logs/events"]) expect(movedTo(now)).toBeUndefined();
    expect(movedTo("/mail/x")).toBeUndefined();
    // Every redirect lands on a page, not on a second redirect.
    for (const r of MOVED_ROUTES) {
      const sample = r.path.replace(":project", "terrarium").replace(":id", "x1").replace(":name", "code-review").replace("*path", "a/b");
      const to = movedTo(sample);
      expect(to, sample).toBeDefined();
      expect(movedTo(to!), to).toBeUndefined();
      expect(movedTo(r.to.replace(":project", "terrarium").replace(":id", "x1").replace(":name", "code-review").replace("/*path", "")), r.to).toBeUndefined();
    }
  });

  it("has no board tab row (TER-507): Tests, Schedules, and Rules are project tabs with a route, and Artifacts has no tab", () => {
    expect(nav).not.toMatch(/BOARD_TABS/);
    expect(PROJECT_TABS.map((i) => i.label)).not.toContain("Artifacts");
    expect(PROJECT_TABS.map((i) => i.path).slice(0, 4)).toEqual(["board", "tests", "schedules", "rules"]);
    const project = app.slice(app.indexOf('<Route path="/p/:project" component={ProjectScreen}>'));
    expect(app).not.toMatch(/BoardScreen/);
    expect(project).toMatch(/<Route path="\/board" component=\{BoardPage\} \/>/);
    expect(project).toMatch(/<Route path="\/board\/done" component=\{DoneCardsPage\} \/>/);
    // Versions are card labels (TER-58): no Versions tab.
    expect(app).not.toMatch(/VersionsPage/);
    // The Artifacts page keeps its path with no tab, so old links still open it.
    expect(project).toMatch(/path="\/board\/artifacts\/:id\?" component=\{ArtifactsPage\}/);
    expect(project).toMatch(/<Route path="\/schedules" component=\{SchedulesPage\} \/>/);
    expect(project).toMatch(/<Route path="\/rules" component=\{RulesPage\} \/>/);
    expect(project).toMatch(/<Route path="\/tests" component=\{TestRunsPage\} \/>/);
    expect(project).toMatch(/<Route path="\/tests\/:id" component=\{TestRunPage\} \/>/);
    // The board and its `?card=` links keep the path.
    expect(movedTo("/p/terrarium/board")).toBeUndefined();
    for (const now of ["/p/terrarium/board/done", "/p/terrarium/board/artifacts", "/p/terrarium/board/artifacts/ART-2", "/p/terrarium/tests", "/p/terrarium/tests/01JBTRUN1", "/p/terrarium/schedules", "/p/terrarium/rules"]) expect(movedTo(now)).toBeUndefined();
  });

  it("redirects the old board tab paths to the project tabs in one hop (TER-507)", () => {
    expect(movedTo("/p/terrarium/board/tests")).toBe("/p/terrarium/tests");
    expect(movedTo("/p/terrarium/board/tests/")).toBe("/p/terrarium/tests");
    // Old mail and comments link a test run as `/p/<project>/board/tests/<id>`.
    expect(movedTo("/p/terrarium/board/tests/01JBTRUN1")).toBe("/p/terrarium/tests/01JBTRUN1");
    expect(movedTo("/p/terrarium/board/schedules")).toBe("/p/terrarium/schedules");
    expect(movedTo("/p/my%20app/board/schedules/")).toBe("/p/my%20app/schedules");
    expect(movedTo("/p/terrarium/board/rules")).toBe("/p/terrarium/rules");
    // A schedule link keeps its query.
    expect(withQuery(movedTo("/p/terrarium/board/schedules")!, "?open=S1&tab=triggers")).toBe("/p/terrarium/schedules?open=S1&tab=triggers");
    for (const old of ["/p/terrarium/board/tests/01JBTRUN1", "/p/terrarium/board/schedules", "/p/terrarium/board/rules"]) expect(movedTo(movedTo(old)!)).toBeUndefined();
    expect(movedTo("/p/terrarium/board/rules/x")).toBeUndefined();
  });

  it("has Knowledge with the tabs Wiki, Memory, and DB, each with a route; /p/:project/knowledge opens the Wiki tab", () => {
    expect(PROJECT_TABS.find((i) => i.path === "knowledge")?.label).toBe("Knowledge");
    expect(KNOWLEDGE_TABS).toEqual([
      { path: "wiki", label: "Wiki" },
      { path: "memory", label: "Memory" },
      { path: "db", label: "DB" },
    ]);
    const project = app.slice(app.indexOf('<Route path="/p/:project" component={ProjectScreen}>'));
    expect(project).toMatch(/<Route path="\/knowledge" component=\{KnowledgePage\}>/);
    const start = project.indexOf('<Route path="/knowledge" component={KnowledgePage}>');
    const block = project.slice(start, project.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/wiki\/\*path" component=\{WikiPage\}/);
    expect(block).toMatch(/path="\/memory" component=\{MemoryPage\}/);
    expect(block).toMatch(/path="\/db" component=\{DbTypesPage\}/);
    expect(block).toMatch(/path="\/db\/:type" component=\{DbRecordsPage\}/);
    expect(movedTo("/p/terrarium/knowledge")).toBe("/p/terrarium/knowledge/wiki");
    expect(movedTo("/p/terrarium/knowledge/")).toBe("/p/terrarium/knowledge/wiki");
    for (const now of ["/p/terrarium/knowledge/wiki", "/p/terrarium/knowledge/wiki/designs/runner-v2", "/p/terrarium/knowledge/memory", "/p/terrarium/knowledge/db", "/p/terrarium/knowledge/db/bug"]) expect(movedTo(now)).toBeUndefined();
  });

  it("redirects the old Versions paths in one hop to the Cards tab, and the Artifacts paths to the Artifacts page", () => {
    expect(movedTo("/p/terrarium/versions")).toBe("/p/terrarium/board");
    expect(movedTo("/p/terrarium/versions/")).toBe("/p/terrarium/board");
    // An old mail links `/p/<project>/versions/<id>`: version objects are gone (TER-58), so it opens the board.
    expect(movedTo("/p/terrarium/versions/01M5PA1A0K16V6G88P3SE78WAB")).toBe("/p/terrarium/board");
    expect(movedTo("/p/my%20app/versions/v0.1.0/")).toBe("/p/my%20app/board");
    // The paths of the old Versions tab.
    expect(movedTo("/p/terrarium/board/versions")).toBe("/p/terrarium/board");
    expect(movedTo("/p/terrarium/board/versions/01ABC")).toBe("/p/terrarium/board");
    // The query of the old URL joins the query of the target.
    expect(withQuery("/p/t/board?version=X", "?card=TER-1")).toBe("/p/t/board?version=X&card=TER-1");
    // A key of the target wins; an old `rows` key goes along, and the board ignores it.
    expect(withQuery("/p/t/board?version=X", "?version=Y&rows=none")).toBe("/p/t/board?version=X&rows=none");
    expect(withQuery("/logs/mail", "?thread=T")).toBe("/logs/mail?thread=T");
    expect(withQuery("/logs/mail", "")).toBe("/logs/mail");
    expect(movedTo("/p/terrarium/artifacts")).toBe("/p/terrarium/board/artifacts");
    expect(movedTo("/p/terrarium/artifacts/ART-2")).toBe("/p/terrarium/board/artifacts/ART-2");
    expect(movedTo("/p/terrarium/versions/a/b")).toBeUndefined();
  });

  it("keeps the project Schedules path and the global Schedules page", () => {
    expect(movedTo("/p/terrarium/schedules")).toBeUndefined();
    expect(movedTo("/p/my%20app/schedules")).toBeUndefined();
    expect(movedTo("/schedules")).toBeUndefined();
  });

  it("redirects the old Wiki path with its page path (a splat), and the old Memory path", () => {
    expect(movedTo("/p/terrarium/wiki")).toBe("/p/terrarium/knowledge/wiki");
    expect(movedTo("/p/terrarium/wiki/")).toBe("/p/terrarium/knowledge/wiki");
    expect(movedTo("/p/terrarium/wiki/index")).toBe("/p/terrarium/knowledge/wiki/index");
    expect(movedTo("/p/terrarium/wiki/designs/runner-v2")).toBe("/p/terrarium/knowledge/wiki/designs/runner-v2");
    expect(movedTo("/p/terrarium/wiki/reports/2026/q4/perf%20week")).toBe("/p/terrarium/knowledge/wiki/reports/2026/q4/perf%20week");
    expect(movedTo("/p/terrarium/wiki/designs/")).toBe("/p/terrarium/knowledge/wiki/designs");
    expect(movedTo("/p/terrarium/wikis")).toBeUndefined();
    expect(movedTo("/p/terrarium/memory")).toBe("/p/terrarium/knowledge/memory");
    expect(movedTo("/p/terrarium/memory/x")).toBeUndefined();
    // The router gives the query and the hash to the redirect (App.tsx).
    expect(app).toMatch(/location\.search\)\}\$\{location\.hash\}/);
  });

  it("has no Spend page left in the code", () => {
    expect(Object.keys(import.meta.glob("../features/spend/**"))).toEqual([]);
    expect(app).not.toMatch(/SpendPage|features\/spend/);
    // The route answers with the redirect, not with "Page not found".
    expect(app).toMatch(/MOVED_ROUTES/);
  });

  it("has a Threads item below Inbox with its own badge, and the Threads routes (TER-466)", () => {
    const hrefs = TOP_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/threads")).toBe(hrefs.indexOf("/inbox") + 1);
    expect(TOP_ITEMS.find((i) => i.href === "/threads")?.label).toBe("Threads");
    expect(app).toMatch(/path="\/threads" component=\{ThreadsPage\}/);
    expect(app).toMatch(/path="\/threads\/:id" component=\{ThreadPage\}/);
    expect(app).not.toMatch(/path="\/inbox\/threads\/:id" component=/);
    expect(layout).toContain("threadsBadge(inbox.counts())");
  });

  it("redirects the old thread paths and the Threads tab of the Inbox to the Threads screen (TER-466)", () => {
    expect(movedTo("/inbox/threads/01M7ABC")).toBe("/threads/01M7ABC");
    expect(movedTo("/inbox/threads/01M7ABC/")).toBe("/threads/01M7ABC");
    expect(movedTo("/threads/01M7ABC")).toBeUndefined();
    expect(movedTo("/inbox")).toBeUndefined();
    expect(inboxRedirect("?tab=thread")).toBe("/threads");
    expect(inboxRedirect("tab=thread&project=web")).toBe("/threads?project=web");
    expect(inboxRedirect("?tab=thread&status=done&project=web")).toBe("/threads?status=archived&project=web");
    expect(inboxRedirect("?tab=thread&status=tracking")).toBe("/threads");
    expect(inboxRedirect("")).toBeUndefined();
    expect(inboxRedirect("?tab=question&status=done")).toBeUndefined();
  });
});

import { describe, expect, it } from "vitest";
import app from "./App.tsx?raw";
import layout from "./Layout.tsx?raw";
import { BOARD_TABS, GLOBAL_ITEMS, KNOWLEDGE_TABS, LOGS_TABS, MOVED_ROUTES, movedTo, PROJECT_ITEMS, PROJECT_LOGS_TABS, PROJECT_SETTINGS_TABS, SETTINGS_ITEMS, SETTINGS_TABS, TOP_ITEMS } from "./nav";

describe("navigation", () => {
  it("has no Spend item; the project has Logs, not Inbox or Mail, with a route", () => {
    const paths = PROJECT_ITEMS.map((i) => i.path) as string[];
    expect(paths).not.toContain("spend");
    expect(paths).not.toContain("inbox");
    expect(paths).not.toContain("mail");
    expect(PROJECT_ITEMS.find((i) => i.path === "logs")?.label).toBe("Logs");
    expect(app).toMatch(/<Route path="\/p\/:project\/logs" component=\{LogsPage\}>/);
    expect(app).not.toMatch(/path="\/p\/:project\/inbox" component/);
    expect(app).not.toMatch(/path="\/p\/:project\/mail" component/);
  });

  it("has one Logs item where Mail was, and the logs screen has the tabs Mail, Events, Audit, each with a route", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/logs")).toBe(hrefs.indexOf("/secrets") + 1);
    expect(GLOBAL_ITEMS.find((i) => i.href === "/logs")?.label).toBe("Logs");
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
    const block = app.slice(app.indexOf('<Route path="/p/:project/logs"'), app.indexOf("</Route>", app.indexOf('<Route path="/p/:project/logs"')));
    expect(block).toMatch(/path="\/mail" component=\{MailPage\}/);
    expect(block).toMatch(/path="\/events" component=\{EventsPage\}/);
    expect(block).not.toMatch(/AuditPage/);
  });

  it("has Agents first under the project, with a route to the Agents page", () => {
    expect(PROJECT_ITEMS[0]).toEqual({ path: "agents", label: "Agents" });
    expect(app).toMatch(/path="\/p\/:project\/agents" component=\{AgentsPage\}/);
  });

  it("has no global Agents item or page: the agents are in the project tree only, but the agent detail keeps its route", () => {
    expect(GLOBAL_ITEMS.map((i) => i.href)).not.toContain("/agents");
    expect(GLOBAL_ITEMS[0]).toEqual({ href: "/templates", label: "Templates" });
    expect(layout).not.toMatch(/AgentTree/);
    expect(Object.keys(import.meta.glob("./AgentTree.tsx"))).toEqual([]);
    expect(app).not.toMatch(/path="\/agents" component=/);
    expect(app).toMatch(/path="\/agents\/:id" component=\{AgentDetail\}/);
  });

  it("redirects the old global Agents page to the project cards, but not an agent page", () => {
    expect(movedTo("/agents")).toBe("/projects");
    expect(movedTo("/agents/")).toBe("/projects");
    expect(movedTo("/agents/a1")).toBeUndefined();
    expect(movedTo("/p/terrarium/agents")).toBeUndefined();
  });

  it("has no Templates, Secrets, or Settings item under the project: they are tabs of the project settings screen, each with a route", () => {
    const paths = PROJECT_ITEMS.map((i) => i.path) as string[];
    for (const gone of ["templates", "secrets", "settings"]) expect(paths).not.toContain(gone);
    expect(paths).toEqual(["agents", "board", "repos", "logs", "knowledge", "schedules"]);
    expect(PROJECT_SETTINGS_TABS).toEqual([
      { path: "general", label: "General" },
      { path: "secrets", label: "Secrets" },
      { path: "templates", label: "Templates" },
    ]);
    expect(app).toMatch(/<Route path="\/p\/:project\/settings" component=\{SettingsPage\}>/);
    const start = app.indexOf('<Route path="/p/:project/settings"');
    const block = app.slice(start, app.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/general" component=\{ProjectSettings\}/);
    expect(block).toMatch(/path="\/secrets" component=\{SecretsPage\}/);
    expect(block).toMatch(/path="\/templates" component=\{TemplatesPage\}/);
    for (const t of PROJECT_SETTINGS_TABS) expect(block).toContain(`path="/${t.path}"`);
    for (const old of ["secrets", "templates"]) expect(app).not.toMatch(new RegExp(`path="/p/:project/${old}" component=`));
    expect(app).not.toMatch(/path="\/p\/:project\/settings" component=\{ProjectSettings\}/);
  });

  it("redirects the old project Secrets, Templates, and Settings paths to the project settings tabs", () => {
    expect(movedTo("/p/terrarium/secrets")).toBe("/p/terrarium/settings/secrets");
    expect(movedTo("/p/my%20app/templates/")).toBe("/p/my%20app/settings/templates");
    expect(movedTo("/p/terrarium/settings")).toBe("/p/terrarium/settings/general");
    expect(movedTo("/p/terrarium/settings/")).toBe("/p/terrarium/settings/general");
    for (const now of ["/p/terrarium/settings/general", "/p/terrarium/settings/secrets", "/p/terrarium/settings/templates"]) expect(movedTo(now)).toBeUndefined();
    // The global pages stay.
    expect(movedTo("/secrets")).toBeUndefined();
    expect(movedTo("/templates")).toBeUndefined();
  });

  it("has Repos under the project after Board, with a list route and a repo route", () => {
    const paths = PROJECT_ITEMS.map((i) => i.path) as string[];
    expect(paths.indexOf("repos")).toBe(paths.indexOf("board") + 1);
    expect(PROJECT_ITEMS.find((i) => i.path === "repos")?.label).toBe("Repos");
    expect(app).toMatch(/path="\/p\/:project\/repos" component=\{ReposPage\}/);
    expect(app).toMatch(/path="\/p\/:project\/repos\/:repo" component=\{RepoPage\}/);
    expect(movedTo("/p/terrarium/repos")).toBeUndefined();
  });

  it("has Schedules under the project and in the global list, each with a route", () => {
    expect(PROJECT_ITEMS.map((i) => i.path)).toContain("schedules");
    expect(GLOBAL_ITEMS.map((i) => i.href)).toContain("/schedules");
    expect(app).toMatch(/path="\/schedules"/);
    expect(app).toMatch(/path="\/p\/:project\/schedules"/);
  });

  it("has Templates in the global list, with a route", () => {
    expect(GLOBAL_ITEMS.map((i) => i.href)).toContain("/templates");
    expect(app).toMatch(/path="\/templates"/);
  });

  it("has Home, then Inbox, above the project tree, with a route, and no Questions item", () => {
    expect(TOP_ITEMS.map((i) => i.href)).toEqual(["/", "/inbox"]);
    expect(TOP_ITEMS.find((i) => i.href === "/inbox")?.label).toBe("Inbox");
    expect([...TOP_ITEMS, ...GLOBAL_ITEMS].map((i) => i.href)).not.toContain("/questions");
    expect(app).toMatch(/path="\/inbox" component=\{InboxPage\}/);
    expect(app).not.toMatch(/path="\/questions" component/);
    expect(Object.keys(import.meta.glob("../features/questions/**"))).toEqual([]);
  });

  it("has Skills after Templates in the global list, with a route that takes a skill name", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/skills")).toBe(hrefs.indexOf("/templates") + 1);
    expect(app).toMatch(/path="\/skills\/:name\?" component=\{SkillsPage\}/);
  });

  it("orders the sidebar: Home and Inbox, the project tree, the global items, then Settings", () => {
    const at = (s: string) => layout.indexOf(s);
    expect(at("TOP_ITEMS}")).toBeGreaterThan(0);
    expect(at("TOP_ITEMS}")).toBeLessThan(at("<ProjectTree />"));
    expect(at("<ProjectTree />")).toBeLessThan(at("GLOBAL_ITEMS}"));
    expect(at("GLOBAL_ITEMS}")).toBeLessThan(at("SETTINGS_ITEMS}"));
    expect(at("SETTINGS_ITEMS}")).toBeLessThan(at('class="sidebar-foot"'));
    // The Projects root of the tree links the cards; no flat Projects item.
    const hrefs = [...TOP_ITEMS, ...GLOBAL_ITEMS, ...SETTINGS_ITEMS].map((i) => i.href) as string[];
    expect(hrefs).not.toContain("/projects");
    expect(app).toMatch(/path="\/projects" component=\{ProjectsPage\}/);
  });

  it("has one Settings item, and the settings screen has the tabs Account, Subscriptions, Harness config, Hosts, each with a route", () => {
    expect(SETTINGS_ITEMS).toEqual([{ href: "/settings", label: "Settings" }]);
    expect(SETTINGS_TABS.map((t) => t.label)).toEqual(["Account", "Subscriptions", "Harness config", "Hosts"]);
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    for (const gone of ["/subscriptions", "/hosts", "/config", "/settings"]) expect(hrefs).not.toContain(gone);
    expect(app).toMatch(/<Route path="\/settings" component=\{SettingsPage\}>/);
    expect(app).toMatch(/path="\/account" component=\{AccountPage\}/);
    expect(app).toMatch(/path="\/subscriptions" component=\{SubscriptionsPage\}/);
    expect(app).toMatch(/path="\/harness" component=\{ConfigPage\}/);
    expect(app).toMatch(/path="\/hosts" component=\{HostsPage\}/);
    for (const t of SETTINGS_TABS) expect(app).toContain(`path="/${t.path}"`);
  });

  it("redirects the old Subscriptions, Hosts, Config, and Settings paths to the settings tabs", () => {
    expect(movedTo("/subscriptions")).toBe("/settings/subscriptions");
    expect(movedTo("/hosts/")).toBe("/settings/hosts");
    expect(movedTo("/config")).toBe("/settings/harness");
    expect(movedTo("/settings")).toBe("/settings/account");
    expect(movedTo("/settings/hosts")).toBeUndefined();
    expect(movedTo("/settings/account")).toBeUndefined();
    // The redirect keeps the query (the add-API-key link opens the wizard).
    expect(app).toMatch(/\$\{location\.search\}/);
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
      "/p/:project/artifacts",
      "/p/:project/artifacts/:id",
      "/p/:project/wiki/*path",
      "/p/:project/memory",
      "/p/:project/knowledge",
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
      const sample = r.path.replace(":project", "terrarium").replace(":id", "x1").replace("*path", "a/b");
      const to = movedTo(sample);
      expect(to, sample).toBeDefined();
      expect(movedTo(to!), to).toBeUndefined();
      expect(movedTo(r.to.replace(":project", "terrarium").replace(":id", "x1").replace("/*path", "")), r.to).toBeUndefined();
    }
  });

  it("has Board with the tabs Cards, Versions, Artifacts, each with a route; Cards is the index route at /p/:project/board", () => {
    expect(BOARD_TABS).toEqual([
      { path: "", label: "Cards" },
      { path: "versions", label: "Versions" },
      { path: "artifacts", label: "Artifacts" },
    ]);
    expect(app).toMatch(/<Route path="\/p\/:project\/board" component=\{BoardScreen\}>/);
    const start = app.indexOf('<Route path="/p/:project/board"');
    const block = app.slice(start, app.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/" component=\{BoardPage\}/);
    expect(block).toMatch(/path="\/versions\/:id\?" component=\{VersionsPage\}/);
    expect(block).toMatch(/path="\/artifacts\/:id\?" component=\{ArtifactsPage\}/);
    for (const gone of ["versions", "artifacts", "wiki", "memory"]) expect(app).not.toMatch(new RegExp(`path="/p/:project/${gone}[^"]*" component=`));
    // The Cards tab and its `?card=` links keep the path.
    expect(movedTo("/p/terrarium/board")).toBeUndefined();
    for (const now of ["/p/terrarium/board/versions", "/p/terrarium/board/versions/01ABC", "/p/terrarium/board/artifacts/ART-2"]) expect(movedTo(now)).toBeUndefined();
  });

  it("has Knowledge with the tabs Wiki and Memory, each with a route; /p/:project/knowledge opens the Wiki tab", () => {
    expect(PROJECT_ITEMS.find((i) => i.path === "knowledge")?.label).toBe("Knowledge");
    expect(KNOWLEDGE_TABS).toEqual([
      { path: "wiki", label: "Wiki" },
      { path: "memory", label: "Memory" },
    ]);
    expect(app).toMatch(/<Route path="\/p\/:project\/knowledge" component=\{KnowledgePage\}>/);
    const start = app.indexOf('<Route path="/p/:project/knowledge"');
    const block = app.slice(start, app.indexOf("</Route>", start));
    expect(block).toMatch(/path="\/wiki\/\*path" component=\{WikiPage\}/);
    expect(block).toMatch(/path="\/memory" component=\{MemoryPage\}/);
    expect(movedTo("/p/terrarium/knowledge")).toBe("/p/terrarium/knowledge/wiki");
    expect(movedTo("/p/terrarium/knowledge/")).toBe("/p/terrarium/knowledge/wiki");
    for (const now of ["/p/terrarium/knowledge/wiki", "/p/terrarium/knowledge/wiki/designs/runner-v2", "/p/terrarium/knowledge/memory"]) expect(movedTo(now)).toBeUndefined();
  });

  it("redirects the old Versions and Artifacts paths to the board tabs, with the ID", () => {
    expect(movedTo("/p/terrarium/versions")).toBe("/p/terrarium/board/versions");
    expect(movedTo("/p/terrarium/versions/")).toBe("/p/terrarium/board/versions");
    // The daemon links `/p/<project>/versions/<id>` (Version.url, the review mail).
    expect(movedTo("/p/terrarium/versions/01M5PA1A0K16V6G88P3SE78WAB")).toBe("/p/terrarium/board/versions/01M5PA1A0K16V6G88P3SE78WAB");
    expect(movedTo("/p/my%20app/versions/v0.1.0/")).toBe("/p/my%20app/board/versions/v0.1.0");
    expect(movedTo("/p/terrarium/artifacts")).toBe("/p/terrarium/board/artifacts");
    expect(movedTo("/p/terrarium/artifacts/ART-2")).toBe("/p/terrarium/board/artifacts/ART-2");
    expect(movedTo("/p/terrarium/versions/a/b")).toBeUndefined();
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
    expect(app).toMatch(/\$\{location\.search\}\$\{location\.hash\}/);
  });

  it("has Secrets after Skills in the global list, with a route to the page", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/secrets")).toBe(hrefs.indexOf("/skills") + 1);
    expect(GLOBAL_ITEMS.find((i) => i.href === "/secrets")?.label).toBe("Secrets");
    expect(app).toMatch(/path="\/secrets" component=\{SecretsPage\}/);
  });

  it("has no Spend page left in the code", () => {
    expect(Object.keys(import.meta.glob("../features/spend/**"))).toEqual([]);
    expect(app).not.toMatch(/SpendPage|features\/spend/);
    // The route answers with the redirect, not with "Page not found".
    expect(app).toMatch(/MOVED_ROUTES/);
  });
});

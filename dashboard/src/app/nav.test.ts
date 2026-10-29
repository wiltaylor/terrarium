import { describe, expect, it } from "vitest";
import app from "./App.tsx?raw";
import layout from "./Layout.tsx?raw";
import { GLOBAL_ITEMS, MOVED_ROUTES, movedTo, PROJECT_ITEMS, SETTINGS_ITEMS, SETTINGS_TABS, TOP_ITEMS } from "./nav";

describe("navigation", () => {
  it("has no Spend item; the project has Mail, not Inbox, with a route", () => {
    const paths = PROJECT_ITEMS.map((i) => i.path) as string[];
    expect(paths).not.toContain("spend");
    expect(paths).not.toContain("inbox");
    expect(PROJECT_ITEMS.find((i) => i.path === "mail")?.label).toBe("Mail");
    expect(GLOBAL_ITEMS.map((i) => i.href)).toContain("/mail");
    expect(app).toMatch(/path="\/mail" component=\{MailPage\}/);
    expect(app).toMatch(/path="\/p\/:project\/mail" component=\{MailPage\}/);
    expect(app).not.toMatch(/path="\/p\/:project\/inbox" component/);
  });

  it("has Agents first under the project, with a route to the Agents page", () => {
    expect(PROJECT_ITEMS[0]).toEqual({ path: "agents", label: "Agents" });
    expect(GLOBAL_ITEMS.map((i) => i.href)).toContain("/agents");
    expect(app).toMatch(/path="\/agents" component=\{AgentsPage\}/);
    expect(app).toMatch(/path="\/p\/:project\/agents" component=\{AgentsPage\}/);
  });

  it("shows the global Agents item as the agent tree, first in the Terrarium section", () => {
    expect(GLOBAL_ITEMS[0]).toEqual({ href: "/agents", label: "Agents" });
    expect(layout).toMatch(/<Show when=\{item\.href !== "\/agents"\} fallback=\{<AgentTree \/>\}>/);
    expect(app).toMatch(/path="\/agents\/:id" component=\{AgentDetail\}/);
  });

  it("has Templates right after Agents under the project, with a route to the Templates page", () => {
    const paths = PROJECT_ITEMS.map((i) => i.path) as string[];
    expect(paths.indexOf("templates")).toBe(paths.indexOf("agents") + 1);
    expect(PROJECT_ITEMS.find((i) => i.path === "templates")?.label).toBe("Templates");
    expect(app).toMatch(/path="\/p\/:project\/templates" component=\{TemplatesPage\}/);
  });

  it("has Schedules under the project and in the global list, each with a route", () => {
    expect(PROJECT_ITEMS.map((i) => i.path)).toContain("schedules");
    expect(GLOBAL_ITEMS.map((i) => i.href)).toContain("/schedules");
    expect(app).toMatch(/path="\/schedules"/);
    expect(app).toMatch(/path="\/p\/:project\/schedules"/);
  });

  it("has Templates after Agents in the global list, with a route", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href);
    expect(hrefs.indexOf("/templates")).toBe(hrefs.indexOf("/agents") + 1);
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
    expect(movedTo("/p/terrarium/inbox")).toBe("/p/terrarium/mail");
    expect(movedTo("/p/my%20app/inbox/")).toBe("/p/my%20app/mail");
    expect(movedTo("/p/terrarium/inbox/x")).toBeUndefined();
    expect(movedTo("/inbox")).toBeUndefined();
    expect(movedTo("/p/terrarium/mail")).toBeUndefined();
    expect(MOVED_ROUTES.map((r) => r.path)).toEqual(["/p/:project/spend", "/questions", "/p/:project/inbox", "/subscriptions", "/hosts", "/config", "/settings"]);
  });

  it("has Secrets after Skills in the global list, and before Settings under the project, each with a route to one page", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/secrets")).toBe(hrefs.indexOf("/skills") + 1);
    expect(GLOBAL_ITEMS.find((i) => i.href === "/secrets")?.label).toBe("Secrets");
    const paths = PROJECT_ITEMS.map((i) => i.path) as string[];
    expect(paths.indexOf("secrets")).toBe(paths.indexOf("settings") - 1);
    expect(PROJECT_ITEMS.find((i) => i.path === "secrets")?.label).toBe("Secrets");
    expect(app).toMatch(/path="\/secrets" component=\{SecretsPage\}/);
    expect(app).toMatch(/path="\/p\/:project\/secrets" component=\{SecretsPage\}/);
  });

  it("has no Spend page left in the code", () => {
    expect(Object.keys(import.meta.glob("../features/spend/**"))).toEqual([]);
    expect(app).not.toMatch(/SpendPage|features\/spend/);
    // The route answers with the redirect, not with "Page not found".
    expect(app).toMatch(/MOVED_ROUTES/);
  });
});

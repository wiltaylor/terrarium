import { describe, expect, it } from "vitest";
import app from "./App.tsx?raw";
import { GLOBAL_ITEMS, MOVED_ROUTES, movedTo, PROJECT_ITEMS } from "./nav";

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

  it("has Inbox right after Home in the global list, with a route, and no Questions item", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/inbox")).toBe(hrefs.indexOf("/") + 1);
    expect(GLOBAL_ITEMS.find((i) => i.href === "/inbox")?.label).toBe("Inbox");
    expect(hrefs).not.toContain("/questions");
    expect(app).toMatch(/path="\/inbox" component=\{InboxPage\}/);
    expect(app).not.toMatch(/path="\/questions" component/);
    expect(Object.keys(import.meta.glob("../features/questions/**"))).toEqual([]);
  });

  it("has Skills after Templates in the global list, with a route that takes a skill name", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href) as string[];
    expect(hrefs.indexOf("/skills")).toBe(hrefs.indexOf("/templates") + 1);
    expect(app).toMatch(/path="\/skills\/:name\?" component=\{SkillsPage\}/);
  });

  it("has Config before Settings in the global list, with a route", () => {
    const hrefs = GLOBAL_ITEMS.map((i) => i.href);
    expect(hrefs.indexOf("/config")).toBe(hrefs.indexOf("/settings") - 1);
    expect(app).toMatch(/path="\/config"/);
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
    expect(MOVED_ROUTES.map((r) => r.path)).toEqual(["/p/:project/spend", "/questions", "/p/:project/inbox"]);
  });

  it("has no Spend page left in the code", () => {
    expect(Object.keys(import.meta.glob("../features/spend/**"))).toEqual([]);
    expect(app).not.toMatch(/SpendPage|features\/spend/);
    // The route answers with the redirect, not with "Page not found".
    expect(app).toMatch(/MOVED_ROUTES/);
  });
});

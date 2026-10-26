import { describe, expect, it } from "vitest";
import app from "./App.tsx?raw";
import { GLOBAL_ITEMS, MOVED_ROUTES, movedTo, PROJECT_ITEMS } from "./nav";

describe("navigation", () => {
  it("has no Spend item, and keeps Mail and the project Inbox", () => {
    expect(PROJECT_ITEMS.map((i) => i.path)).not.toContain("spend");
    expect(PROJECT_ITEMS.map((i) => i.path)).toContain("inbox");
    expect(GLOBAL_ITEMS.map((i) => i.href)).toContain("/mail");
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

  it("redirects the old Spend route to Home, where the usage split lives", () => {
    expect(movedTo("/p/terrarium/spend")).toBe("/");
    expect(movedTo("/p/my-app/spend/")).toBe("/");
    expect(movedTo("/p/terrarium/board")).toBeUndefined();
    expect(movedTo("/p/terrarium/spend/x")).toBeUndefined();
    expect(movedTo("/spend")).toBeUndefined();
    expect(MOVED_ROUTES.map((r) => r.path)).toEqual(["/p/:project/spend"]);
  });

  it("has no Spend page left in the code", () => {
    expect(Object.keys(import.meta.glob("../features/spend/**"))).toEqual([]);
    expect(app).not.toMatch(/SpendPage|features\/spend/);
    // The route answers with the redirect, not with "Page not found".
    expect(app).toMatch(/MOVED_ROUTES/);
  });
});

import { expect, it } from "vitest";
import { createMockApi } from "./index";
import { allHome, defaultHomeColumns } from "../home";

it("uses per-project selectors and paginates compact Home cards", async () => {
  const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
  const p = await api.projects.create({ name: "home-test" });
  await api.cards.create(p.name, { title: "One" });
  await api.cards.create(p.name, { title: "Two" });
  await api.projects.update(p.name, { home_board_columns: { ...defaultHomeColumns, accepted: ["backlog"], in_progress: [] } });
  const rows = await allHome(cursor => api.home.cards({ project: p.name, column: "accepted", limit: 1, cursor }));
  expect(rows.map(c => c.title).sort()).toEqual(["One", "Two"]);
  expect(rows[0]).not.toHaveProperty("body");
  expect((await api.home.cards({ project: p.name, column: "in_progress" })).items).toEqual([]);
});

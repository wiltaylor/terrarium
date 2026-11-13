// @vitest-environment jsdom
import { render } from "solid-js/web";
import { expect, it } from "vitest";
import type { Subscription } from "../api/types";
import { AssignedAgentsChip } from "./AssignedAgentsChip";

it.each([
  [{ assigned_agent_count: 0, assigned_agent_count_scope: "all_projects" }, "0 agents", "across all projects"],
  [{ assigned_agent_count: 2, assigned_agent_count_scope: "project", assigned_agent_count_project: "p" }, "2 agents in this project", "in project p"],
  [{}, "Agents unavailable", "unavailable"],
  [{ assigned_agent_count: 0 }, "Agents unavailable", "unavailable"],
] as const)("renders count and scope without inventing zero: %j", (sub, label, title) => {
  const root = document.createElement("div");
  const dispose = render(() => <AssignedAgentsChip sub={sub as Subscription} />, root);
  expect(root.textContent).toBe(label);
  expect(root.querySelector("span")?.title).toContain(title);
  dispose();
});

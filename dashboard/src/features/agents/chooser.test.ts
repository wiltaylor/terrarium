import { describe, expect, it } from "vitest";
import type { AgentTemplate } from "../../api/types";
import { chooserList, firstLine, SEARCH_ABOVE, showSearch, startStep, templateSummary } from "./chooser";

const tpl = (name: string, project: string | null, extra: Partial<AgentTemplate> = {}): AgentTemplate => ({
  id: `T-${project ?? "_"}-${name}`,
  name,
  project,
  description: "",
  spec: { harness: "codex", model: "gpt", restart: "on-failure" },
  revision: 1,
  created_by: "operator",
  source: "api",
  created_at: "",
  updated_at: "",
  ...extra,
});

describe("template chooser", () => {
  it("puts the project's templates first, then the global ones, each by name; never another project's", () => {
    const list = [tpl("b", null), tpl("z", "p"), tpl("a", "p"), tpl("a", null), tpl("q", "other")];
    expect(chooserList(list, "p").map((t) => `${t.project ?? "global"}:${t.name}`)).toEqual(["p:a", "p:z", "global:a", "global:b"]);
  });

  it("searches the name, description, harness, model, and roles; each word must match", () => {
    const list = [
      tpl("reviewer", "p", { description: "Reviews versions" }),
      tpl("lead", "p", { spec: { harness: "claude", model: "opus", roles: ["project_admin"] } }),
      tpl("worker", null, { spec: { harness: "pi" } }),
    ];
    const names = (q: string) => chooserList(list, "p", q).map((t) => t.name);
    expect(names("")).toEqual(["lead", "reviewer", "worker"]);
    expect(names("REVIEWS")).toEqual(["reviewer"]);
    expect(names("claude admin")).toEqual(["lead"]);
    expect(names("global")).toEqual(["worker"]);
    expect(names("gpt pi")).toEqual([]);
  });

  it("opens on the chooser with templates, on the custom form with none; a search above eight", () => {
    expect(startStep([])).toBe("custom");
    expect(startStep([tpl("a", null)])).toBe("choose");
    const many = Array.from({ length: SEARCH_ABOVE + 1 }, (_, i) => tpl(`t${i}`, "p"));
    expect(showSearch(many.slice(0, SEARCH_ABOVE))).toBe(false);
    expect(showSearch(many)).toBe(true);
  });

  it("sums up a card: harness, model and subscription, roles, and the first line of the description or system prompt", () => {
    expect(templateSummary(tpl("a", "p"))).toEqual({ harness: "codex", model: "gpt", roles: [], line: "", global: false });
    const t = tpl("b", null, { description: "\n  First line.\nSecond.", spec: { harness: "claude", subscription: "max", subscription_pool: ["max", "max2"], roles: ["member", "sysadmin"], system_prompt: "Be brief." } });
    expect(templateSummary(t)).toEqual({ harness: "claude", model: "default model · max · pool of 2", roles: ["sysadmin"], line: "First line.", global: true });
    expect(templateSummary(tpl("c", "p", { spec: { harness: "pi", system_prompt: "You review.\nMore." } })).line).toBe("You review.");
    expect(firstLine(null)).toBe("");
  });
});

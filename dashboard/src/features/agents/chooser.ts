// The first step of "New agent": the template chooser (docs/api.md §21).
// The order of the cards, the search, the summary of each card, and the
// step that the dialog opens on. Pure.

import type { AgentTemplate, Role } from "../../api/types";
import { selectable } from "../templates/model";

/** The chooser shows a search box above this number of templates. */
export const SEARCH_ABOVE = 8;

/** The steps of the dialog: the chooser, the short form of a template, and the full form with no template. */
export type NewAgentStep = "choose" | "template" | "custom";

/** The step that the dialog opens on: with no template, the custom form. */
export const startStep = (templates: AgentTemplate[]): NewAgentStep => (templates.length ? "choose" : "custom");

/** True when the chooser shows the search box. */
export const showSearch = (templates: AgentTemplate[]) => templates.length > SEARCH_ABOVE;

/** The first line with text, or "". */
export const firstLine = (s: string | null | undefined) =>
  (s ?? "")
    .split("\n")
    .map((l) => l.trim())
    .find(Boolean) ?? "";

/** The card text of a template. */
export interface TemplateSummary {
  harness: string;
  /** The model and the subscription, or "default model". */
  model: string;
  /** The roles above member. */
  roles: Role[];
  /** The first line of the description, else of the system prompt. Empty when none. */
  line: string;
  global: boolean;
}

export function templateSummary(t: AgentTemplate): TemplateSummary {
  const s = t.spec;
  const parts = [s.model || "default model"];
  if (s.subscription) parts.push(s.subscription);
  if (s.subscription_pool?.length) parts.push(`pool of ${s.subscription_pool.length}`);
  return {
    harness: s.harness,
    model: parts.join(" · "),
    roles: (s.roles ?? []).filter((r) => r !== "member"),
    line: firstLine(t.description) || firstLine(s.system_prompt),
    global: t.project === null,
  };
}

/**
 * The cards of the chooser for an agent of `project`: the project's
 * templates, then the global ones, each by name. A search matches the
 * name, the description, the harness, the model, the subscription, and
 * the roles, with no case. Each word of the search must match.
 */
export function chooserList(list: AgentTemplate[], project: string, query = ""): AgentTemplate[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const text = (t: AgentTemplate) => [t.name, t.description, t.spec.harness, t.spec.model, t.spec.subscription, ...(t.spec.roles ?? []), t.project ?? "global"].join(" ").toLowerCase();
  return selectable(list, project).filter((t) => words.every((w) => text(t).includes(w)));
}

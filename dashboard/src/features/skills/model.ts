// The skills model: the checks of a new skill, the problems of a 422, a
// daemon with no skills, the options and the label of a skill list on an
// agent or a template, and the count of "used by". Pure.

import { ApiError, isMissingEndpoint } from "../../api/http";
import type { EffectiveSkill, SkillSummary, SkillUsage } from "../../api/types";
import { splitFrontMatter } from "../../lib/markdown";

/** The name in a skill list that means every skill. */
export const ALL_SKILLS = "*";

/** The note of a daemon with no config repo, so no skills. */
export const NO_SKILLS_NOTE = "This daemon has no config repo, so it has no skills. Set TERRARIUM_CONFIG_REPO on the daemon to use skills.";

/** The note under a write: the Harness config page commits it. */
export const COMMIT_NOTE = "A change to a skill stays in the config repo as a local change. Commit it on the Harness config page.";

/** The title of a "default" badge in Settings: the marker lives in SKILL.md. */
export const DEFAULT_ON_TITLE = 'Default on: each agent gets this skill unless it turns it off. To change it, edit SKILL.md: metadata, terrarium-default: "true".';

/** The note of the default-on skills. */
export const DEFAULT_NOTE = "A default skill goes to each agent. Clear its box to turn it off for this agent. A skill is default on when its SKILL.md has metadata terrarium-default: \"true\".";

/** The note of a changed skill list: the agent gets it at the next start. */
export const RESTART_NOTE = "The skills of a live agent change at the next start or restart.";

const NAME_RE = /^[a-z0-9]+(-[a-z0-9]+)*$/;
export const MAX_NAME = 64;
export const MAX_DESCRIPTION = 1024;

/** The client check of a new skill name, or null. */
export function nameProblem(name: string): string | null {
  const n = name.trim();
  if (!n) return "Give a name.";
  if (n.length > MAX_NAME || !NAME_RE.test(n)) return `Name: 1 to ${MAX_NAME} of a-z and 0-9, with one - between the words (fern-check).`;
  return null;
}

/** The client check of a description, or null. */
export function descriptionProblem(description: string): string | null {
  const d = description.trim();
  if (!d) return "Give a description. The agent reads it to know when to use the skill.";
  const n = [...d].length;
  if (n > MAX_DESCRIPTION) return `Description: ${n} characters; the limit is ${MAX_DESCRIPTION}.`;
  return null;
}

/** The problems of a write that failed the checks (`422`, `details.problems`), else null. */
export function skillProblems(e: unknown): string[] | null {
  if (!(e instanceof ApiError) || e.status !== 422) return null;
  const d = e.details;
  const p = typeof d === "object" && d !== null && !Array.isArray(d) ? (d as { problems?: unknown }).problems : undefined;
  if (!Array.isArray(p)) return null;
  const list = p.filter((x): x is string => typeof x === "string");
  return list.length ? list : null;
}

/** The daemon has no skills: no config repo (`501 not_available`), or no `/api/skills` route. */
export const noSkills = (e: unknown): boolean => (e instanceof ApiError && (e.status === 501 || e.code === "not_available")) || isMissingEndpoint(e);

/** The options of a skill picker: every skill by name, and the names in `value` that the list does not have (so that they stay visible). No `*`. */
export function skillOptions(list: Pick<SkillSummary, "name">[], value: string[] = []): string[] {
  const names = new Set(list.map((s) => s.name));
  for (const v of value) if (v !== ALL_SKILLS) names.add(v);
  return [...names].sort((a, b) => a.localeCompare(b));
}

/**
 * A skill list after a pick. `*` and the names do not go together: `*` on
 * clears the names; a name on clears `*`. The order of the names stays.
 */
export function toggleSkill(value: string[], name: string, on: boolean): string[] {
  if (!on) return value.filter((x) => x !== name);
  if (name === ALL_SKILLS) return [ALL_SKILLS];
  return value.includes(name) ? value : [...value.filter((x) => x !== ALL_SKILLS), name];
}

/** The skill settings of an agent or a template. */
export interface SkillSelection {
  /** The explicit skills, or `["*"]`. */
  skills: string[];
  /** The skills that the agent does not get. */
  skills_off: string[];
  /** False: no default-on skill. */
  default_skills: boolean;
}

/** The names of the default-on skills of the list. */
export const defaultOnNames = (list: Pick<SkillSummary, "name" | "default_on">[]): Set<string> => new Set(list.filter((s) => s.default_on).map((s) => s.name));

/** True when the selection gives the skill `name` only because it is default on. */
export const byDefault = (sel: SkillSelection, name: string, defaultOn: Set<string>): boolean => sel.default_skills && defaultOn.has(name) && !sel.skills.includes(name) && !sel.skills.includes(ALL_SKILLS);

/** True when the selection gives the skill `name`: (default on, when `default_skills`) or explicit, and not in `skills_off`. */
export function skillOn(sel: SkillSelection, name: string, defaultOn: Set<string>): boolean {
  if (sel.skills_off.includes(name)) return false;
  if (sel.skills.includes(ALL_SKILLS) || sel.skills.includes(name)) return true;
  return sel.default_skills && defaultOn.has(name);
}

/**
 * The selection after a pick of one skill. On: the name leaves
 * `skills_off`, and goes in `skills` when the defaults or `*` do not give
 * it. Off: the name leaves `skills`, and goes in `skills_off` when the
 * defaults or `*` give it.
 */
export function pickSkill(sel: SkillSelection, name: string, on: boolean, defaultOn: Set<string>): SkillSelection {
  const all = sel.skills.includes(ALL_SKILLS);
  const fromDefault = sel.default_skills && defaultOn.has(name);
  if (on) {
    const skills_off = sel.skills_off.filter((x) => x !== name);
    const skills = fromDefault || all ? sel.skills : toggleSkill(sel.skills, name, true);
    return { ...sel, skills, skills_off };
  }
  const skills = sel.skills.filter((x) => x !== name);
  const skills_off = (fromDefault || all) && !sel.skills_off.includes(name) ? [...sel.skills_off, name] : sel.skills_off;
  return { ...sel, skills, skills_off };
}

/** The effective skills of a selection, as the daemon computes them, sorted by name. */
export function effectiveOf(sel: SkillSelection, list: Pick<SkillSummary, "name" | "default_on">[]): EffectiveSkill[] {
  const names = list.map((s) => s.name);
  const defaultOn = defaultOnNames(list);
  const out = new Map<string, EffectiveSkill["source"]>();
  if (sel.default_skills) for (const n of names) if (defaultOn.has(n)) out.set(n, "default");
  const explicit = sel.skills.includes(ALL_SKILLS) ? names : sel.skills.filter((n) => names.includes(n));
  for (const n of explicit) out.set(n, "explicit");
  for (const n of sel.skills_off) out.delete(n);
  return [...out.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([name, source]) => ({ name, source }));
}

/** "none", or "fern-check, terrarium-db (default)". */
export function effectiveLabel(list: EffectiveSkill[] | undefined): string {
  if (!list?.length) return "none";
  return list.map((e) => (e.source === "default" ? `${e.name} (default)` : e.name)).join(", ");
}

/** "none", "all skills", or "fern-check, release-notes". */
export function skillsLabel(list: string[] | undefined): string {
  if (!list?.length) return "none";
  if (list.includes(ALL_SKILLS)) return "all skills";
  return list.join(", ");
}

/** The names in `value` that are not skills of the list, for a warning. `*` is always good. */
export function unknownSkills(value: string[], list: Pick<SkillSummary, "name">[]): string[] {
  const names = new Set(list.map((s) => s.name));
  return value.filter((v) => v !== ALL_SKILLS && !names.has(v));
}

/** The number of agents and templates that use a skill, and how many of them through `*` (`all`) and as a default skill (`byDefault`). */
export function usedBy(u: SkillUsage): { agents: number; templates: number; all: number; byDefault: number; total: number } {
  const all = [...u.agents, ...u.templates].filter((x) => x.all).length;
  const defaults = [...u.agents, ...u.templates].filter((x) => x.default).length;
  return { agents: u.agents.length, templates: u.templates.length, all, byDefault: defaults, total: u.agents.length + u.templates.length };
}

/** "2 agents, 1 template", or "nothing". */
export function usedByText(u: SkillUsage): string {
  const n = usedBy(u);
  const parts = [n.agents ? `${n.agents} agent${n.agents === 1 ? "" : "s"}` : "", n.templates ? `${n.templates} template${n.templates === 1 ? "" : "s"}` : ""].filter(Boolean);
  return parts.length ? parts.join(", ") : "nothing";
}

/** The body of SKILL.md without its front matter: the Markdown of the preview. */
export const previewBody = (skillMd: string): string => splitFrontMatter(skillMd).body;

/** The skills of the list, with a search on the name and the description. */
export function filterSkills<T extends Pick<SkillSummary, "name" | "description">>(list: T[], q: string): T[] {
  const s = q.trim().toLowerCase();
  return s ? list.filter((x) => x.name.includes(s) || x.description.toLowerCase().includes(s)) : list;
}

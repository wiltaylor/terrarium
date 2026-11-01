// The model of the System prompts tab (docs/api.md §28). Pure.

import { ApiError } from "../../api/http";
import type { SystemPromptSummary, SystemPromptUser } from "../../api/types";

/** The note under a write: it stays uncommitted in the config repo. */
export const COMMIT_NOTE = "A change goes to system-prompts/ in the config repo and stays uncommitted. Commit it on the Harness config page. Agents get it at their next start.";

/** The users in the `409` of a delete in use, or null for another error. */
export function usersOfConflict(e: unknown): SystemPromptUser[] | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null;
  const users = (e.details as { users?: SystemPromptUser[] } | undefined)?.users;
  return Array.isArray(users) ? users : null;
}

/** One line for a user: `agent p/w (by role)`. */
export function userLine(u: SystemPromptUser): string {
  return `${u.kind} ${u.name}${u.by_role ? " (by role)" : ""}`;
}

/** The entries whose name or description holds `q`, case-insensitive. */
export function filterPrompts(list: readonly SystemPromptSummary[], q: string): SystemPromptSummary[] {
  const s = q.trim().toLowerCase();
  if (!s) return [...list];
  return list.filter((e) => e.name.includes(s) || e.description.toLowerCase().includes(s));
}

/** The source as the table shows it. */
export function sourceText(e: Pick<SystemPromptSummary, "source" | "builtin">): string {
  return `${e.source === "default" ? "packaged default" : "config repo"}${e.builtin ? " · by role" : ""}`;
}

// Tool proxies in the dashboard (GitHub #21, docs/api.md §17.11): pure
// helpers for the Settings > Tools page and the Tools section of the agent
// page. No Solid imports.

import type { Agent, AuditEntry, Project, Tool } from "../../api/types";

/** The command of a tool as one line: the argv joined with spaces, each word that needs it quoted. */
export const commandLine = (argv: string[]): string => argv.map((w) => (/^[\w@%+=:,./-]+$/.test(w) ? w : `'${w.replace(/'/g, `'\\''`)}'`)).join(" ");

/** The approval rule in words. */
export function approvalText(a: Tool["approval"]): string {
  if (!a) return "none";
  if (a === true) return "every run";
  return `first argument ${a.first.join(", ")}`;
}

/** The argument rules in words. */
export function argsText(a: Tool["args"]): string {
  const parts: string[] = [];
  if (a?.allow?.length) parts.push(`first argument one of ${a.allow.join(", ")}`);
  if (a?.deny_flags?.length) parts.push(`no ${a.deny_flags.join(", ")}`);
  if (a?.max !== undefined) parts.push(`at most ${a.max} arguments`);
  if (a?.max_len !== undefined) parts.push(`each at most ${a.max_len} bytes`);
  return parts.length ? parts.join("; ") : "any";
}

/** Who may get the tool: a project tool only its project and that project's agents; a global tool any project and agent. */
export function granteeOptions(t: Pick<Tool, "project" | "grants">, projects: Pick<Project, "name">[], agents: Pick<Agent, "id" | "project" | "name" | "ephemeral">[]): { value: string; label: string }[] {
  const have = new Set((t.grants ?? []).map((g) => g.grantee));
  const ps = projects.map((p) => p.name).filter((p) => !t.project || p === t.project);
  const out = [
    ...ps.map((p) => ({ value: `project:${p}`, label: `project:${p} (every agent of it)` })),
    ...agents
      .filter((a) => !a.ephemeral && (!t.project || a.project === t.project))
      .map((a) => ({ value: `agent:${a.id}`, label: `agent:${a.project}/${a.name}` })),
  ];
  return out.filter((o) => !have.has(o.value)).sort((a, b) => a.label.localeCompare(b.label));
}

/** The tools that `agent` may run: its own grants and its project's (as the daemon's `granted_to`). */
export const toolsOf = (tools: Tool[], agent: Pick<Agent, "id" | "project">): { tool: Tool; via: "agent" | "project" }[] =>
  tools
    .filter((t) => !t.project || t.project === agent.project)
    .flatMap((t): { tool: Tool; via: "agent" | "project" }[] => {
      const g = t.grants ?? [];
      if (g.some((x) => x.grantee === `agent:${agent.id}`)) return [{ tool: t, via: "agent" }];
      if (g.some((x) => x.grantee === `project:${agent.project}`)) return [{ tool: t, via: "project" }];
      return [];
    });

/** One row of the recent runs, from a `tool.run` audit row. */
export interface RunRow {
  at: string;
  tool: string;
  agent: string;
  argv: string;
  exit: string;
  duration: string;
  truncated: boolean;
}

/** The recent runs from the `tool.run` audit rows (the argv is redacted by the daemon). */
export function runRows(rows: AuditEntry[]): RunRow[] {
  return rows
    .filter((r) => r.action === "tool.run")
    .map((r) => {
      const d = (r.detail ?? {}) as Record<string, unknown>;
      const code = d.exit;
      const ms = typeof d.duration_ms === "number" ? d.duration_ms : undefined;
      const end = (d.end ?? {}) as Record<string, unknown>;
      return {
        at: r.at,
        tool: r.target,
        agent: typeof d.agent === "string" ? d.agent : r.actor,
        argv: Array.isArray(d.argv) ? commandLine(d.argv.map(String)) : "",
        exit: code === undefined || code === null ? (typeof end.code === "string" ? end.code : "—") : String(code),
        duration: ms === undefined ? "" : ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`,
        truncated: d.truncated === true,
      };
    });
}

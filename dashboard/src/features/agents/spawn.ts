// Ephemeral agents in the dashboard (docs/api.md §6.10): the spawn rights
// of the agent form, and the rows of the Spawned tab. Pure.

import type { Agent, AgentTemplate, Spawn, SpawnPolicy } from "../../api/types";

/** The defaults of the daemon (`terrarium_core::config::SpawnPolicy`). */
export const SPAWN_DEFAULT_CONCURRENT = 2;
export const SPAWN_DEFAULT_LIFETIME = "2h";
const MAX_COUNT = 50;
const MAX_LIFETIME_SECS = 24 * 3600;

/** The block that the form starts with when the operator turns spawn rights on. */
export const newSpawnPolicy = (): SpawnPolicy => ({ templates: ["project:*"], max_concurrent: SPAWN_DEFAULT_CONCURRENT });

/** `30m`, `2h`, `1h30m`, `90s` -> seconds; null when it does not parse. */
export function durationSecs(text: string): number | null {
  const m = /^(?:(\d+)d)?(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(text.trim());
  if (!m || !(m[1] || m[2] || m[3] || m[4])) return null;
  return Number(m[1] ?? 0) * 86400 + Number(m[2] ?? 0) * 3600 + Number(m[3] ?? 0) * 60 + Number(m[4] ?? 0);
}

/** The problem of a spawn block, as the daemon would refuse it (`SpawnPolicy::check`), or null. */
export function spawnProblem(sp: SpawnPolicy): string | null {
  for (const t of sp.templates) {
    const e = t.trim();
    if (e === "*" || e === "project:*" || e === "global:*") continue;
    const name = e.startsWith("global:") ? e.slice(7) : e;
    if (!/^[A-Za-z0-9._-]+$/.test(name)) return `Spawn rights: \`${e}\` is not a template name, global:<name>, project:*, global:*, or *.`;
  }
  const n = sp.max_concurrent ?? SPAWN_DEFAULT_CONCURRENT;
  if (!Number.isInteger(n) || n < 1 || n > MAX_COUNT) return `Spawn rights: max concurrent must be 1 to ${MAX_COUNT}.`;
  if (sp.max_per_day !== undefined && (!Number.isInteger(sp.max_per_day) || sp.max_per_day < 1 || sp.max_per_day > MAX_COUNT * 20)) return `Spawn rights: max per day must be 1 to ${MAX_COUNT * 20}, or empty.`;
  if (sp.max_lifetime?.trim()) {
    const secs = durationSecs(sp.max_lifetime);
    if (!secs) return `Spawn rights: max lifetime \`${sp.max_lifetime}\` is not a duration such as 30m or 2h.`;
    if (secs > MAX_LIFETIME_SECS) return "Spawn rights: max lifetime is at most 24h.";
  }
  return null;
}

/** The same spawn rights? Absent and null are the same. */
export function sameSpawn(a: SpawnPolicy | null | undefined, b: SpawnPolicy | null | undefined): boolean {
  if (!a || !b) return !a && !b;
  const list = (x: string[]) => [...x].sort().join("\n");
  return (
    list(a.templates) === list(b.templates) &&
    (a.max_concurrent ?? SPAWN_DEFAULT_CONCURRENT) === (b.max_concurrent ?? SPAWN_DEFAULT_CONCURRENT) &&
    a.max_per_day === b.max_per_day &&
    (a.max_lifetime?.trim() || undefined) === (b.max_lifetime?.trim() || undefined) &&
    !!a.may_spawn_children === !!b.may_spawn_children
  );
}

/** One choice of the template list of the form: a pattern, or one template. */
export interface TemplateChoice {
  /** The entry of `spawn.templates`. */
  value: string;
  label: string;
}

/** The choices for an agent of `project`: the two patterns, the project's templates, then the global ones. */
export function templateChoices(project: string, templates: AgentTemplate[]): TemplateChoice[] {
  const own = templates.filter((t) => t.project === project).sort((x, y) => x.name.localeCompare(y.name));
  const global = templates.filter((t) => !t.project).sort((x, y) => x.name.localeCompare(y.name));
  return [
    { value: "project:*", label: "every template of the project" },
    { value: "global:*", label: "every global template" },
    ...own.map((t) => ({ value: t.name, label: t.name })),
    ...global.map((t) => ({ value: `global:${t.name}`, label: `${t.name} (global)` })),
  ];
}

/** One line for people: the rights of an agent, or "none". */
export function spawnRightsText(sp: SpawnPolicy | null | undefined): string {
  if (!sp) return "none";
  const parts = [sp.templates.length ? sp.templates.join(", ") : "no templates", `${sp.max_concurrent ?? SPAWN_DEFAULT_CONCURRENT} at a time`];
  if (sp.max_per_day) parts.push(`${sp.max_per_day} a day`);
  parts.push(`lifetime ${sp.max_lifetime?.trim() || SPAWN_DEFAULT_LIFETIME}`);
  if (sp.may_spawn_children) parts.push("children may spawn");
  return parts.join(" · ");
}

/** `42s`, `4m 12s`, `2h 5m`. */
export function durationText(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

/** The chip of a row: the live status while it runs, the outcome after. */
export function spawnChip(s: Pick<Spawn, "status" | "outcome" | "finished_at">): { label: string; cls: string } {
  if (!s.finished_at) {
    const cls = s.status === "working" ? "ok" : s.status === "error" || s.status === "blocked" ? "warn" : "";
    return { label: s.status, cls };
  }
  switch (s.outcome) {
    case "done":
      return { label: "done", cls: "ok" };
    case "timeout":
    case "cancelled":
      return { label: s.outcome, cls: "warn" };
    default:
      return { label: s.outcome ?? "finished", cls: "danger" };
  }
}

/** The first `lines` lines of a result, at most `chars` characters; "" when none. */
export function resultPreview(result: string | undefined, lines = 2, chars = 160): string {
  if (!result?.trim()) return "";
  const head = result.trim().split("\n").slice(0, lines).join(" · ");
  return head.length > chars ? `${head.slice(0, chars - 1)}…` : head;
}

/** The duration of a row now: to the finish, or to `now` while it runs. */
export function spawnDuration(s: Pick<Spawn, "spawned_at" | "finished_at" | "duration_secs">, now: number): number {
  if (s.finished_at) return s.duration_secs;
  const start = Date.parse(s.spawned_at);
  return Number.isNaN(start) ? s.duration_secs : Math.max(0, (now - start) / 1000);
}

/** The label of the Spawned tab: the count of running ones when there are any. */
export const spawnedTabLabel = (running: number | undefined): string => (running ? `Spawned (${running})` : "Spawned");

/** "Ephemeral agent of lead · lifetime left 1h 3m" parts for the page of an ephemeral agent. */
export function lifetimeLeft(a: Pick<Agent, "spawn">, now: number): string | null {
  const end = a.spawn ? Date.parse(a.spawn.deadline_at) : NaN;
  if (Number.isNaN(end)) return null;
  const left = (end - now) / 1000;
  return left <= 0 ? "lifetime over" : `lifetime left ${durationText(left)}`;
}

/** The running rows first (newest first), then the finished ones (newest first). The daemon gives newest first. */
export function orderSpawns(rows: Spawn[]): Spawn[] {
  return [...rows.filter((s) => !s.finished_at), ...rows.filter((s) => s.finished_at)];
}

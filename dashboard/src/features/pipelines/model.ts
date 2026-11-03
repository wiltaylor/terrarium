// The pure parts of the Pipelines page (docs/api.md §31): the status words and
// glyphs, the state of a pipeline, the text of a source, the duration of a
// run, and the form of the dialog. No Solid here, so tests can read it.

import type { Pipeline, PipelineDef, PipelineRun, PipelineRunStatus, PipelineSource } from "../../api/types";

/** The label, the glyph, and the badge class of each status. The glyph and the word carry the state, not only the colour. */
export const STATUS_META: Record<PipelineRunStatus | "error" | "pending" | "off", { label: string; glyph: string; cls: string }> = {
  success: { label: "Success", glyph: "✓", cls: "ok" },
  failed: { label: "Failed", glyph: "✕", cls: "crit" },
  running: { label: "Running", glyph: "▶", cls: "running" },
  queued: { label: "Queued", glyph: "○", cls: "unknown" },
  waiting: { label: "Waiting", glyph: "⏸", cls: "warn" },
  cancelled: { label: "Cancelled", glyph: "⊘", cls: "unknown" },
  skipped: { label: "Skipped", glyph: "↷", cls: "unknown" },
  unknown: { label: "Unknown", glyph: "?", cls: "unknown" },
  error: { label: "Poll error", glyph: "!", cls: "crit" },
  pending: { label: "Not polled yet", glyph: "…", cls: "unknown" },
  off: { label: "Off", glyph: "–", cls: "unknown" },
};

export type PipelineState = keyof typeof STATUS_META;

/** The state of a pipeline for its badge: off, a poll error, not polled yet, or the status of its newest run. */
export function pipelineState(p: Pick<Pipeline, "enabled" | "error" | "status" | "polled_at">): PipelineState {
  if (!p.enabled) return "off";
  if (p.error) return "error";
  if (p.status) return p.status;
  return p.polled_at ? "unknown" : "pending";
}

/** `github wiltaylor/terrarium · ci.yml @ main` or `script on wsl`. */
export function sourceText(s: PipelineSource): string {
  if (s.kind === "github") return [`github ${s.repo}`, s.workflow, s.branch ? `@ ${s.branch}` : null].filter(Boolean).join(" · ");
  return `script on ${s.target}`;
}

/** The time of a run from its start to its end (or to `now` while it runs): `2m 05s`, `1h 03m`, `–`. */
export function runDuration(r: Pick<PipelineRun, "started_at" | "finished_at" | "status">, now: number): string {
  if (!r.started_at) return "–";
  const end = r.finished_at ? Date.parse(r.finished_at) : r.status === "running" || r.status === "waiting" ? now : NaN;
  const ms = end - Date.parse(r.started_at);
  if (!Number.isFinite(ms) || ms < 0) return "–";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

/** The counts of the pipelines by state, for the summary at the top of the page. */
export function stateCounts(ps: Pick<Pipeline, "enabled" | "error" | "status" | "polled_at">[]): Partial<Record<PipelineState, number>> {
  const out: Partial<Record<PipelineState, number>> = {};
  for (const p of ps) {
    const s = pipelineState(p);
    out[s] = (out[s] ?? 0) + 1;
  }
  return out;
}

/** The fields of the dialog, as text. */
export interface PipelineForm {
  name: string;
  description: string;
  kind: "github" | "script";
  repo: string;
  workflow: string;
  branch: string;
  tokenSecret: string;
  apiUrl: string;
  target: string;
  script: string;
  timeout: string;
  /** `VAR=secret`, one per line. */
  envSecrets: string;
  every: string;
  maxRuns: string;
  enabled: boolean;
}

export function formOf(p?: Pipeline): PipelineForm {
  const s = p?.source;
  const gh = s?.kind === "github" ? s : undefined;
  const sc = s?.kind === "script" ? s : undefined;
  return {
    name: p?.name ?? "",
    description: p?.description ?? "",
    kind: s?.kind ?? "github",
    repo: gh?.repo ?? "",
    workflow: gh?.workflow ?? "",
    branch: gh?.branch ?? "",
    tokenSecret: gh?.token_secret ?? "",
    apiUrl: gh?.api_url ?? "",
    target: sc?.target ?? "",
    script: sc?.script ?? "",
    timeout: String(sc?.timeout_secs ?? 60),
    envSecrets: Object.entries(sc?.env_secrets ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
    every: p?.schedule.kind === "interval" ? p.schedule.every : "5m",
    maxRuns: String(p?.max_runs ?? 20),
    enabled: p?.enabled ?? true,
  };
}

/** Read `VAR=secret` lines. A bad line is an error that names it. */
export function parseEnvSecrets(text: string): { env: Record<string, string> } | { error: string } {
  const env: Record<string, string> = {};
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const i = line.indexOf("=");
    const k = line.slice(0, i).trim();
    const v = line.slice(i + 1).trim();
    if (i <= 0 || !v || !/^[A-Z_][A-Z0-9_]*$/.test(k)) return { error: `\`${line}\` is not VAR=secret (VAR in A-Z, 0-9, _)` };
    env[k] = v;
  }
  return { env };
}

/** The body of a create, an update, or a test. `null` fields are absent, so the daemon uses its defaults. */
export function defOf(f: PipelineForm): PipelineDef {
  const opt = (s: string) => s.trim() || null;
  let source: PipelineSource;
  if (f.kind === "github") {
    source = { kind: "github", repo: f.repo.trim(), workflow: opt(f.workflow), branch: opt(f.branch), token_secret: opt(f.tokenSecret), api_url: opt(f.apiUrl) };
  } else {
    const env = parseEnvSecrets(f.envSecrets);
    source = { kind: "script", target: f.target, script: f.script, timeout_secs: Number(f.timeout) || 60, env_secrets: "env" in env ? env.env : {} };
  }
  // Leave the null fields out: the source of a create has no optional field set to null.
  source = Object.fromEntries(Object.entries(source).filter(([, v]) => v !== null)) as PipelineSource;
  return { name: f.name.trim(), description: opt(f.description), source, schedule: { kind: "interval", every: f.every.trim() || "5m" }, enabled: f.enabled, max_runs: Number(f.maxRuns) || 20 };
}

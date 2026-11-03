// Pipelines of the mock (docs/api.md §31), after the daemon's
// `store/pipelines` and `pipelines/`: the CRUD with its checks, refresh, the
// runs and the run detail, and the test poll. The mock reaches no CI system:
// each poll moves a synthetic history on (a queued run starts, a running run
// ends, now and then a new run comes), then records it as the daemon does,
// with the `pipeline.run`, `pipeline.status`, and `pipeline.error` events.
// The seed gives project `terrarium` a GitHub pipeline and an Octopus Deploy
// script pipeline.

import type { Api } from "../client";
import type { AgentEvent, Pipeline, PipelineDef, PipelineRun, PipelineRunStatus, PipelineSource, PipelineStep } from "../types";
import { parseEvery } from "../../lib/schedule";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface PipelineMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: AgentEvent["kind"]; actor: string; project: string; summary: string; data: Record<string, unknown> }) => unknown;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  caller?: ScheduleCaller;
  ulid: (tag: string) => string;
  now: () => number;
}

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const REPO_RE = /^[A-Za-z0-9._-]{1,100}\/[A-Za-z0-9._-]{1,100}$/;
const MIN_EVERY = 30;
const iso = (ms: number) => new Date(ms).toISOString();

interface State {
  p: Pipeline;
  runs: PipelineRun[];
  anchor: number;
  /** The next synthetic run number. */
  seq: number;
  /** The mock fails every poll while this is set. */
  failing?: string;
}

export function createPipelineMock(ctx: PipelineMockContext) {
  const pipelines: State[] = [];
  const field = (f: string, message: string): never => ctx.fail(422, "invalid", message, { field: f });
  const canWrite = (project: string, what: string) => {
    const c = ctx.caller;
    if (!c || c.role === "operator" || c.role === "sysadmin" || (c.role === "project_admin" && c.project === project)) return;
    ctx.fail(403, "forbidden", `${what} needs project_admin of \`${project}\`, sysadmin, or the operator`);
  };
  const find = (key: string): State => {
    const [p, n] = key.includes("/") ? key.split("/") : [undefined, undefined];
    return pipelines.find((s) => (p ? s.p.project === p && s.p.name === n : s.p.id === key)) ?? ctx.fail(404, "not_found", `no pipeline \`${key}\``);
  };
  const periodOf = (d: Pick<Pipeline, "schedule">) => (d.schedule.kind === "interval" ? Math.max(parseEvery(d.schedule.every) ?? 300, MIN_EVERY) : 600);

  const validate = (d: PipelineDef) => {
    if (!NAME_RE.test(d.name)) field("name", `bad pipeline name \`${d.name}\`: use 1-63 of a-z, 0-9, \`-\`, \`_\``);
    const s = d.source;
    if (s.kind === "github") {
      if (!REPO_RE.test(s.repo)) field("source.repo", `\`${s.repo}\` is not \`owner/name\``);
      if (s.workflow?.includes("/")) field("source.workflow", "give the file name of the workflow (`ci.yml`) or its ID, not a path");
      if (s.api_url && !/^https?:\/\//.test(s.api_url)) field("source.api_url", "give an http or https URL");
    } else if (s.kind === "script") {
      if (!s.target?.trim()) field("source.target", "name a target");
      if (!s.script?.trim()) field("source.script", "the script is empty");
      const t = s.timeout_secs ?? 60;
      if (t < 1 || t > 600) field("source.timeout_secs", `${t} is not 1-600`);
    } else field("source.kind", "use github or script");
    if (d.schedule?.kind === "interval") {
      const secs = parseEvery(d.schedule.every);
      if (secs === null) field("schedule.every", `\`${d.schedule.every}\` is not an interval`);
      else if (secs < MIN_EVERY) field("schedule.every", `\`${d.schedule.every}\` is under the shortest interval, ${MIN_EVERY} s`);
    }
    const m = d.max_runs ?? 20;
    if (!Number.isInteger(m) || m < 1 || m > 100) field("max_runs", `${m} is not 1-100`);
  };

  const normal = (d: PipelineDef): Required<Pick<PipelineDef, "source" | "schedule" | "enabled" | "max_runs">> & { name: string; description: string | null } => {
    const source: PipelineSource = d.source.kind === "script" ? { ...d.source, timeout_secs: d.source.timeout_secs ?? 60 } : { ...d.source };
    return { name: d.name, description: d.description ?? null, source, schedule: d.schedule ?? { kind: "interval", every: "5m" }, enabled: d.enabled ?? true, max_runs: d.max_runs ?? 20 };
  };

  // ----- the synthetic CI -----
  const stepsFor = (s: State, status: PipelineRunStatus): PipelineStep[] => {
    const names = s.p.source.kind === "github" ? ["build", "test", "lint", "package"] : ["Acquire packages", "Deploy web", "Run migrations", "Smoke test"];
    const order: PipelineRunStatus[] = status === "running" ? ["success", "running", "queued", "queued"] : status === "failed" ? ["success", "success", "failed", "skipped"] : status === "queued" ? ["queued", "queued", "queued", "queued"] : status === "waiting" ? ["success", "success", "waiting", "queued"] : [status, status, status, status];
    return names.map((name, i) => ({ name, status: order[i]! }));
  };

  const newRun = (s: State, at: number, status: PipelineRunStatus): PipelineRun => {
    const n = s.seq++;
    const gh = s.p.source.kind === "github";
    const env = ["Development", "Test", "Production"][n % 3]!;
    return {
      id: gh ? String(9_000_000 + n) : `Deployments-${n}`,
      pipeline: s.p.id,
      name: gh ? `CI #${n}` : `1.${Math.floor(n / 3)}.${n % 3} to ${env}`,
      status,
      url: gh ? `https://github.com/${(s.p.source as { repo: string }).repo}/actions/runs/${9_000_000 + n}` : `https://octopus.example/app#/Spaces-1/deployments/Deployments-${n}`,
      branch: gh ? (n % 4 === 0 ? "feat/pipelines" : "main") : null,
      commit: gh ? (n * 2654435761).toString(16).padStart(8, "0").slice(0, 8) + "c0ffee" : null,
      actor: "wil",
      event: gh ? "push" : null,
      environment: gh ? null : env,
      started_at: status === "queued" ? null : iso(at),
      finished_at: null,
      summary: gh ? ["Fix the build", "Add the pipelines page", "Bump the harness pins", "Tidy the docs"][n % 4]! : null,
      steps: stepsFor(s, status),
      first_seen: iso(at),
      changed_at: iso(at),
    };
  };

  /** Move the synthetic history of `s` on to `now`: what one poll sees. */
  const advanceHistory = (s: State, now: number) => {
    for (const r of s.runs) {
      if (r.status === "queued") Object.assign(r, { status: "running", started_at: iso(now), steps: stepsFor(s, "running") });
      else if (r.status === "running" || r.status === "waiting") {
        const end: PipelineRunStatus = s.seq % 5 === 0 ? "failed" : r.status === "running" && s.p.source.kind === "script" && r.environment === "Production" ? "waiting" : "success";
        Object.assign(r, { status: end, finished_at: end === "waiting" ? null : iso(now), steps: stepsFor(s, end) });
      }
    }
    if (s.seq % 3 === 0 || !s.runs.length) s.runs.unshift(newRun(s, now, "queued"));
    else s.seq++;
  };

  const record = (s: State, at: number, before: Map<string, PipelineRunStatus>, first: boolean) => {
    const p = s.p;
    const data = { project: p.project, pipeline: p.name, pipeline_id: p.id };
    if (s.failing) {
      const wasOk = p.error === null;
      Object.assign(p, { polled_at: iso(at), error: s.failing });
      if (wasOk) ctx.emit({ kind: "pipeline.error", actor: "system", project: p.project, summary: `pipeline ${p.name}: the poll failed: ${s.failing}`, data: { ...data, error: s.failing } });
      return { runs: 0, new: 0, changed: 0, status: p.status, error: s.failing };
    }
    let added = 0;
    let changed = 0;
    s.runs = s.runs.slice(0, p.max_runs);
    for (const r of s.runs) {
      const was = before.get(r.id);
      if (was === r.status) continue;
      if (was === undefined) added++;
      else {
        changed++;
        r.changed_at = iso(at);
      }
      if (!first) ctx.emit({ kind: "pipeline.run", actor: "system", project: p.project, summary: `pipeline ${p.name}: ${r.name ?? r.id} ${was ? `${was} → ` : ""}${r.status}`, data: { ...data, run_id: r.id, name: r.name, from: was ?? null, to: r.status, url: r.url, branch: r.branch, environment: r.environment } });
    }
    const status = s.runs[0]?.status ?? null;
    const from = p.status;
    Object.assign(p, { polled_at: iso(at), ok_at: iso(at), error: null, status, latest: s.runs[0] ?? null });
    if (status && status !== from) ctx.emit({ kind: "pipeline.status", actor: "system", project: p.project, summary: `pipeline ${p.name}: ${from ?? "none"} → ${status}`, data: { ...data, from, to: status, run_id: s.runs[0]?.id, url: s.runs[0]?.url } });
    return { runs: s.runs.length, new: added, changed, status, error: null };
  };

  const poll = (s: State, at: number) => {
    const before = new Map(s.runs.map((r) => [r.id, r.status]));
    const first = s.p.ok_at === null;
    if (!s.failing) advanceHistory(s, at);
    return record(s, at, before, first);
  };

  const create = (project: string, d: PipelineDef, at: number): State => {
    const n = normal(d);
    const s: State = {
      p: { ...n, id: ctx.ulid("PIPE"), project, period_secs: periodOf(n), created_by: "operator", run_as: "operator", created_at: iso(at), updated_at: iso(at), next_run_at: n.enabled ? iso(at) : null, skipped: 0, status: null, polled_at: null, ok_at: null, error: null, latest: null, running: false },
      runs: [],
      anchor: at,
      seq: 1,
    };
    pipelines.push(s);
    return s;
  };

  const api: Api["pipelines"] = {
    list: (project) =>
      ctx.reply(() => {
        ctx.project(project);
        return pipelines.filter((s) => s.p.project === project).sort((a, b) => a.p.name.localeCompare(b.p.name)).map((s) => structuredClone(s.p));
      }),
    get: (id) => ctx.reply(() => structuredClone(find(id).p)),
    create: (project, def) =>
      ctx.reply(() => {
        ctx.project(project);
        canWrite(project, "creating a pipeline");
        validate(def);
        if (pipelines.some((s) => s.p.project === project && s.p.name === def.name)) ctx.fail(409, "conflict", `project \`${project}\` has a pipeline named \`${def.name}\``, { field: "name" });
        const s = create(project, def, ctx.now());
        ctx.audit("pipeline.create", `${project}/${def.name}`, project, { id: s.p.id, adapter: def.source.kind });
        if (s.p.enabled) poll(s, ctx.now());
        return structuredClone(s.p);
      }),
    update: (id, patch) =>
      ctx.reply(() => {
        const s = find(id);
        canWrite(s.p.project, "changing a pipeline");
        const { source: srcPatch, ...rest } = patch;
        const source = (srcPatch?.kind && srcPatch.kind !== s.p.source.kind ? srcPatch : { ...s.p.source, ...srcPatch }) as PipelineSource;
        const def: PipelineDef = { name: s.p.name, description: s.p.description, schedule: s.p.schedule, enabled: s.p.enabled, max_runs: s.p.max_runs, ...rest, source };
        validate(def);
        const n = normal(def);
        const restart = n.enabled && !s.p.enabled;
        Object.assign(s.p, n, { period_secs: periodOf(n), updated_at: iso(ctx.now()), next_run_at: n.enabled ? (restart ? iso(ctx.now()) : (s.p.next_run_at ?? iso(ctx.now()))) : null });
        s.runs = s.runs.slice(0, n.max_runs);
        ctx.audit("pipeline.update", `${s.p.project}/${s.p.name}`, s.p.project, { id: s.p.id });
        return structuredClone(s.p);
      }),
    remove: (id) =>
      ctx.reply(() => {
        const s = find(id);
        canWrite(s.p.project, "deleting a pipeline");
        pipelines.splice(pipelines.indexOf(s), 1);
        ctx.audit("pipeline.delete", `${s.p.project}/${s.p.name}`, s.p.project, { id: s.p.id });
      }),
    refresh: (id) =>
      ctx.reply(() => {
        const s = find(id);
        canWrite(s.p.project, "refreshing a pipeline");
        const r = poll(s, ctx.now());
        const output = s.p.source.kind === "script" ? { exit_code: s.failing ? 1 : 0, stdout: s.failing ? "" : JSON.stringify({ runs: s.runs.slice(0, 2) }, null, 1).slice(0, 2000), stderr: s.failing ?? "" } : null;
        return { pipeline: structuredClone(s.p), poll: r, output };
      }),
    runs: (id, limit) => ctx.reply(() => structuredClone(find(id).runs.slice(0, limit ?? 100))),
    run: (id, run) =>
      ctx.reply(() => {
        const s = find(id);
        const r = s.runs.find((x) => x.id === run) ?? ctx.fail(404, "not_found", `pipeline \`${id}\` has no run \`${run}\``);
        return { ...structuredClone(r), project: s.p.project, pipeline_name: s.p.name, adapter: s.p.source.kind };
      }),
    test: (project, def) =>
      ctx.reply(() => {
        ctx.project(project);
        canWrite(project, "testing a pipeline");
        validate({ ...def, name: def.name && NAME_RE.test(def.name) ? def.name : "test-poll" });
        const s = create(project, { ...def, name: "test-poll", enabled: false }, ctx.now());
        pipelines.pop();
        advanceHistory(s, ctx.now());
        advanceHistory(s, ctx.now());
        const runs = s.runs.map(({ pipeline: _p, first_seen: _f, changed_at: _c, ...r }) => r);
        const output = def.source.kind === "script" ? { exit_code: 0, stdout: JSON.stringify({ runs }, null, 1).slice(0, 2000), stderr: "" } : null;
        return { at: iso(ctx.now()), duration_ms: 240, runs, error: null, output };
      }),
  };

  /** Poll the due pipelines, as the daemon's tick does. */
  const tick = () => {
    const now = ctx.now();
    for (const s of pipelines) {
      if (!s.p.enabled || !s.p.next_run_at || Date.parse(s.p.next_run_at) > now) continue;
      s.p.next_run_at = iso(now + s.p.period_secs * 1000);
      poll(s, now);
    }
  };

  /** A GitHub pipeline and an Octopus Deploy script pipeline on project `terrarium`, with some history. */
  const seed = () => {
    const now = ctx.now();
    const gh = create("terrarium", { name: "ci", description: "GitHub Actions: the CI workflow on main.", source: { kind: "github", repo: "wiltaylor/terrarium", workflow: "ci.yml", token_secret: "github-token" }, schedule: { kind: "interval", every: "2m" } }, now - 86_400_000);
    const octo = create(
      "terrarium",
      {
        name: "octopus-deploy",
        description: "The deploys of Octopus Deploy, read by a Python script on the WSL host.",
        source: { kind: "script", target: "pi-runner", script: "python3 ~/bin/octopus-runs.py --project terrarium", env_secrets: { OCTOPUS_API_KEY: "octopus-key" } },
        schedule: { kind: "interval", every: "5m" },
      },
      now - 86_400_000,
    );
    for (const [s, polls] of [
      [gh, 14],
      [octo, 8],
    ] as const) {
      for (let i = polls; i >= 1; i--) poll(s, now - i * 600_000);
      s.anchor = now;
      s.p.next_run_at = iso(now + s.p.period_secs * 1000);
    }
    const broken = create("terrarium", { name: "nightly-release", description: "A release workflow of a private repo, with no token.", source: { kind: "github", repo: "wiltaylor/private-tools", workflow: "release.yml" }, schedule: { kind: "cron", expr: "0 2 * * *", tz: "UTC" } }, now - 86_400_000);
    broken.failing = "GitHub has no such repo or workflow (404: Not Found); a private repo needs a token_secret";
    poll(broken, now - 3_600_000);
  };

  return { api, tick, seed };
}

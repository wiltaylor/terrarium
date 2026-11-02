// Monitors and the project dashboard of the mock (docs/api.md §30), after the
// daemon's `store/monitors` and `monitors/`: the CRUD with its checks, run
// now, the test run, the results with the downsample, and the layout. The
// mock has no target: each run makes the output of the script from the
// monitor name (synthetic values that drift), then reads it as the daemon
// does. The seed gives project `terrarium` ten monitors of every kind, a
// week of history, and a dashboard; `tick` runs the due ones, so the page
// gets `monitor.result` and `monitor.status` events.

import type { Api } from "../client";
import type { AgentEvent, DashboardBlock, DashboardLayout, DashboardView, Monitor, MonitorDef, MonitorJson, MonitorKind, MonitorPoint, MonitorResult, MonitorRow, MonitorRunReport, MonitorStatus, Target } from "../types";
import { kindsFor } from "../../features/dashboard/model";
import { parseEvery } from "../../lib/schedule";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface MonitorMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: AgentEvent["kind"]; actor: string; project: string; summary: string; data: Record<string, unknown> }) => unknown;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  targets: () => Target[];
  caller?: ScheduleCaller;
  ulid: (tag: string) => string;
  now: () => number;
}

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const MIN_EVERY = 15;
const MAX_POINTS = 10_000;

const iso = (ms: number) => new Date(ms).toISOString();

/** The first number in a text, as the daemon reads it (`1,234.5`, `-3`, `42%`). */
export function firstNumber(s: string): number | undefined {
  const m = /([-+]?)(\d{1,3}(?:,\d{3})+(?!\d)(?:\.\d+)?|\d*\.\d+|\d+(?:\.\d+)?)([eE][-+]?\d+)?/.exec(s);
  if (!m) return undefined;
  // A sign counts only at the start of a word: `a-5` is 5.
  const signed = m[1] === "-" && (m.index === 0 || !/[A-Za-z0-9_]/.test(s[m.index - 1]!));
  const n = Number(`${signed ? "-" : ""}${m[2]!.replace(/,/g, "")}${m[3] ?? ""}`);
  return Number.isFinite(n) ? n : undefined;
}

const ofExit = (code: number | null): MonitorStatus => (code === 0 ? "ok" : code === 1 ? "warn" : code === 2 ? "crit" : "unknown");

function byThresholds(v: number, m: Pick<MonitorDef, "warn" | "crit" | "direction">): MonitorStatus {
  const past = (t: number) => (m.direction === "below" ? v <= t : v >= t);
  if (m.crit !== null && m.crit !== undefined && past(m.crit)) return "crit";
  if (m.warn !== null && m.warn !== undefined && past(m.warn)) return "warn";
  return "ok";
}

type Parsed = Pick<MonitorResult, "status" | "value" | "text" | "json" | "error">;

/** Read the output of one run by the output kind (`monitors::parse`). */
export function parseOutput(kind: MonitorKind, m: Pick<MonitorDef, "warn" | "crit" | "direction">, stdout: string, exit: number | null): Parsed {
  const firstLine = stdout.split("\n").map((l) => l.trim()).find(Boolean)?.slice(0, 500);
  switch (kind) {
    case "number": {
      const v = firstNumber(stdout);
      return v === undefined ? { status: "unknown", text: firstLine, error: `no number in the output (exit code ${exit})` } : { status: byThresholds(v, m), value: v };
    }
    case "status": {
      const s = ofExit(exit);
      return { status: s, text: firstLine, ...(s === "unknown" ? { error: `exit code ${exit}: use 0 ok, 1 warn, 2 crit` } : {}) };
    }
    case "text": {
      const s = ofExit(exit);
      return { status: s, text: stdout.trimEnd().slice(0, 8192), ...(s === "unknown" ? { error: `exit code ${exit}` } : {}) };
    }
    case "json": {
      let o: unknown;
      try {
        o = JSON.parse(stdout);
      } catch (e) {
        return { status: "unknown", text: firstLine, error: `the output is not JSON: ${String(e)}` };
      }
      if (typeof o !== "object" || o === null || Array.isArray(o)) return { status: "unknown", error: "the output is JSON but not an object" };
      const j = o as Record<string, unknown>;
      const out: MonitorJson = {};
      const value = typeof j.value === "number" ? j.value : typeof j.value === "string" ? firstNumber(j.value) : undefined;
      if (value !== undefined) out.value = value;
      if (typeof j.unit === "string") out.unit = j.unit.slice(0, 32);
      if (typeof j.message === "string") out.message = j.message.trim().slice(0, 500);
      if (Array.isArray(j.rows)) {
        out.rows = j.rows.slice(0, 200).map((r) => Object.fromEntries(Object.entries(r as object).slice(0, 20)) as MonitorRow);
        if (j.rows.length > 200) out.truncated = true;
      }
      const given = typeof j.status === "string" ? (["ok", "warn", "crit", "unknown"].includes(j.status) ? (j.status as MonitorStatus) : "unknown") : undefined;
      const status = given ?? (value !== undefined && (m.warn != null || m.crit != null) ? byThresholds(value, m) : ofExit(exit));
      out.status = status;
      return { status, value, text: out.message, json: out };
    }
  }
}

/** The output of a script in the mock: synthetic, from the monitor name, and drifting with the time. */
function synth(m: Pick<MonitorDef, "name" | "kind" | "target" | "script">, t: number, targets: Target[]): { stdout: string; stderr: string; exit: number | null; down?: string } {
  const target = targets.find((x) => x.name === m.target);
  if (target && (target.kind === "docker" || target.kind === "vmlab") && (target.state === "stopped" || target.state === "absent"))
    return { stdout: "", stderr: "", exit: null, down: `target down: target \`${target.name}\` is ${target.state}; a monitor does not bring a target up` };
  // A plain `echo <text>` (with an optional `; exit <n>`) runs as written, so a test run of the editor shows its own text.
  const plain = /^echo\s+(['"]?)(.*?)\1\s*(?:;\s*exit\s+(\d+))?\s*$/.exec(m.script.trim());
  if (plain) return { stdout: `${plain[2]}\n`, stderr: "", exit: plain[3] ? Number(plain[3]) : 0 };
  const h = t / 3_600_000;
  const wave = (period: number, phase = 0) => Math.sin((2 * Math.PI * h) / period + phase);
  const noise = (seed: number) => {
    const x = Math.sin(Math.floor(t / 30_000) * 12.9898 + seed) * 43758.5453;
    return x - Math.floor(x);
  };
  switch (m.name) {
    case "load":
      return { stdout: `${(1.6 + 1.2 * wave(6) + noise(1) * 0.8 + (wave(24) > 0.9 ? 3.5 : 0)).toFixed(2)}\n`, stderr: "", exit: 0 };
    case "memory":
      return { stdout: `${(62 + 8 * wave(12, 1) + noise(2) * 3).toFixed(1)}\n`, stderr: "", exit: 0 };
    case "disk-root": {
      // Fills up over the week, so the block is a warning now.
      const days = (t % (7 * 86_400_000)) / 86_400_000;
      return { stdout: `${Math.round(74 + days * 1.3 + noise(3))}%\n`, stderr: "", exit: 0 };
    }
    case "uptime":
      return { stdout: ` ${new Date(t).toTimeString().slice(0, 8)} up 12 days,  3:04,  2 users,  load average: 1.12, 0.98, 0.87\n`, stderr: "", exit: 0 };
    case "docker":
      return { stdout: "docker is active\n", stderr: "", exit: 0 };
    case "containers":
      return { stdout: `${6 + Math.round(2 * (wave(8) + 1) / 2 + noise(4))}\n`, stderr: "", exit: 0 };
    case "dashboard-http": {
      const bad = noise(5) > 0.93;
      return { stdout: bad ? "HTTP 502 from http://localhost:8080/\n" : "HTTP 200 from http://localhost:8080/\n", stderr: "", exit: bad ? 2 : 0 };
    }
    case "api-latency": {
      const v = Math.round(120 + 60 * wave(3) + noise(6) * 90 + (noise(7) > 0.97 ? 400 : 0));
      return { stdout: JSON.stringify({ value: v, unit: "ms", message: `p95 of the last minute` }), stderr: "", exit: 0 };
    }
    case "top-cpu": {
      const procs = ["terrariumd", "node", "dockerd", "postgres", "claude", "containerd", "sshd"];
      const rows = procs
        .map((cmd, i) => ({ pid: 1000 + i * 37, user: i % 3 ? "root" : "wil", cpu: Number((noise(10 + i) * (40 - i * 5)).toFixed(1)), mem: Number((noise(20 + i) * 8).toFixed(1)), cmd }))
        .sort((a, b) => b.cpu - a.cpu)
        .slice(0, 5);
      return { stdout: JSON.stringify({ status: "ok", message: `${rows[0]!.cmd} uses the most CPU`, rows }), stderr: "", exit: 0 };
    }
    default:
      switch (m.kind) {
        case "number":
          return { stdout: `${(noise(30) * 100).toFixed(1)}\n`, stderr: "", exit: 0 };
        case "json":
          return { stdout: JSON.stringify({ value: Number((noise(31) * 10).toFixed(2)), message: "from the mock" }), stderr: "", exit: 0 };
        default:
          return { stdout: `mock output of: ${m.script.split("\n")[0]}\n`, stderr: "", exit: 0 };
      }
  }
}

interface Stored {
  m: Monitor;
  results: MonitorResult[];
  anchor: number;
}

function scheduleError(def: Partial<MonitorDef>): string | null {
  const s = def.schedule;
  if (!s) return "schedule: give an interval or a cron spec";
  if (s.kind === "interval") {
    const secs = parseEvery(s.every);
    if (secs === null) return `schedule.every: bad interval \`${s.every}\``;
    if (secs < MIN_EVERY) return `schedule.every: \`${s.every}\` is under the shortest interval, ${MIN_EVERY} s`;
    return null;
  }
  if (s.kind === "cron") return s.expr.trim().split(/\s+/).length === 5 || s.expr.startsWith("@") ? null : `schedule.expr: bad cron expression \`${s.expr}\`: use five fields`;
  return "schedule: a monitor runs on an interval or a cron spec";
}

const periodOf = (s: MonitorDef["schedule"]): number => (s.kind === "interval" ? Math.max(MIN_EVERY, parseEvery(s.every) ?? 60) : s.expr.startsWith("*/") ? Number(s.expr.slice(2).split(" ")[0]) * 60 || 300 : 86_400);

export function createMonitorMock(ctx: MonitorMockContext) {
  const monitors: Stored[] = [];
  const layouts = new Map<string, { layout: DashboardLayout; at: string; by: string }>();
  const running = new Set<string>();
  let pk = 0;

  const canWrite = (project: string) => {
    const c = ctx.caller;
    if (!c || c.role === "operator" || c.role === "sysadmin" || (c.role === "project_admin" && c.project === project)) return;
    ctx.fail(403, "forbidden", `changing the monitors of project \`${project}\` needs project_admin of \`${project}\`, sysadmin, or the operator`);
  };
  const canRead = (project: string) => {
    const c = ctx.caller;
    if (c && (c.role === "member" || c.role === "project_admin") && c.project !== project) ctx.fail(403, "forbidden", `project \`${project}\` is not your project; reading it needs sysadmin`);
  };
  const field = (f: string, msg: string): never => ctx.fail(422, "invalid", `${f}: ${msg}`, { field: f });

  const find = (key: string): Stored => {
    const [p, n] = key.includes("/") ? key.split("/", 2) : [undefined, undefined];
    const s = monitors.find((x) => x.m.id === key || (p !== undefined && x.m.project === p && x.m.name === n));
    return s ?? ctx.fail(404, "not_found", `no monitor \`${key}\``);
  };
  const view = (s: Stored): Monitor => ({ ...s.m, last: s.results.at(-1) ?? null, running: running.has(s.m.id) });

  type Normal = Required<Omit<MonitorDef, "description" | "unit" | "warn" | "crit">> & { description: string | null; unit: string | null; warn: number | null; crit: number | null };
  const normalize = (project: string, def: Partial<MonitorDef>): Normal => {
    if (!def.name || !NAME_RE.test(def.name)) field("name", `bad monitor name \`${def.name ?? ""}\`: use 1-63 of a-z, 0-9, \`-\`, \`_\``);
    if (!def.target) field("target", "name a target");
    const t = ctx.targets().find((x) => x.name === def.target) ?? field("target", `no target \`${def.target}\``);
    if (t.project && t.project !== project) field("target", `target \`${t.name}\` belongs to project \`${t.project}\`; use a global target or one of \`${project}\``);
    if (!def.script?.trim()) field("script", "the script is empty");
    const se = scheduleError(def);
    if (se) field(se.split(":")[0]!, se.slice(se.indexOf(":") + 2));
    if (!def.kind || !["number", "status", "text", "json"].includes(def.kind)) field("kind", "use number, status, text, or json");
    const timeout = def.timeout_secs ?? 30;
    if (timeout < 1 || timeout > 600) field("timeout_secs", `${timeout} is not 1-600`);
    const retention = def.retention_days ?? 7;
    if (retention < 1 || retention > 90) field("retention_days", `${retention} is not 1-90`);
    if ((def.warn != null || def.crit != null) && def.kind !== "number" && def.kind !== "json") field("warn", `a ${def.kind} monitor has no value; thresholds need kind number or json`);
    const dir = def.direction ?? "above";
    if (def.warn != null && def.crit != null && (dir === "above" ? def.warn > def.crit : def.warn < def.crit)) field("warn", `with direction ${dir}, warn (${def.warn}) comes before crit (${def.crit})`);
    return {
      name: def.name!,
      description: def.description ?? null,
      target: def.target!,
      script: def.script!,
      schedule: def.schedule!,
      timeout_secs: timeout,
      kind: def.kind!,
      unit: def.unit ?? null,
      warn: def.warn ?? null,
      crit: def.crit ?? null,
      direction: dir,
      enabled: def.enabled ?? true,
      retention_days: retention,
    };
  };

  /** Run the script of `def` once (no record). */
  const execute = (def: Pick<MonitorDef, "name" | "kind" | "target" | "script" | "warn" | "crit" | "direction">, at: number): MonitorRunReport => {
    const out = synth(def, at, ctx.targets());
    const duration_ms = 20 + Math.round(Math.abs(Math.sin(at)) * 180);
    if (out.down) return { result: { at: iso(at), status: "unknown", error: out.down, duration_ms }, output: null };
    const p = parseOutput(def.kind, def, out.stdout, out.exit);
    return { result: { at: iso(at), duration_ms, ...strip(p) }, output: { exit_code: out.exit, stdout: out.stdout, stderr: out.stderr } };
  };
  const strip = <T extends object>(o: T): T => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined)) as T;

  /** Keep a result; emit `monitor.result`, and `monitor.status` on a change. */
  const record = (s: Stored, r: MonitorRunReport["result"], quiet = false): MonitorResult => {
    const prev = s.results.at(-1)?.status ?? null;
    const row: MonitorResult = { id: ++pk, monitor: s.m.id, ...r, at: r.at } as MonitorResult;
    s.results.push(row);
    const cut = Date.parse(r.at) - s.m.retention_days * 86_400_000;
    while (s.results.length && (Date.parse(s.results[0]!.at) < cut || s.results.length > MAX_POINTS)) s.results.shift();
    if (quiet) return row;
    const unit = row.json?.unit ?? s.m.unit;
    const shown = row.value !== undefined ? ` ${row.value}${unit ? ` ${unit}` : ""}` : "";
    const message = s.m.kind !== "text" ? (row.text ?? null) : null;
    ctx.emit({
      kind: "monitor.result",
      actor: "system",
      project: s.m.project,
      summary: `monitor ${s.m.name}: ${row.status}${shown}`,
      data: { project: s.m.project, monitor: s.m.name, monitor_id: s.m.id, status: row.status, value: row.value ?? null, unit, message, error: row.error ?? null, at: row.at, duration_ms: row.duration_ms, result_id: row.id },
    });
    if (prev !== row.status)
      ctx.emit({
        kind: "monitor.status",
        actor: "system",
        project: s.m.project,
        summary: `monitor ${s.m.name}: ${prev ?? "none"} → ${row.status}${shown}`,
        data: { project: s.m.project, monitor: s.m.name, monitor_id: s.m.id, from: prev, to: row.status, value: row.value ?? null, unit, message, error: row.error ?? null },
      });
    return row;
  };

  const nextRun = (s: Stored, after: number) => {
    const step = s.m.period_secs * 1000;
    return s.anchor + (Math.floor((after - s.anchor) / step) + 1) * step;
  };

  const create = (project: string, def: Partial<MonitorDef>, at: number, by = "operator"): Stored => {
    const d = normalize(project, def);
    if (monitors.some((x) => x.m.project === project && x.m.name === d.name)) ctx.fail(409, "conflict", `project \`${project}\` has a monitor named \`${d.name}\``, { field: "name" });
    const m: Monitor = {
      ...d,
      id: ctx.ulid("mon"),
      project,
      period_secs: periodOf(d.schedule),
      created_by: by,
      run_as: by,
      created_at: iso(at),
      updated_at: iso(at),
      next_run_at: d.enabled ? iso(at) : null,
      skipped: 0,
      last: null,
      running: false,
    };
    const s: Stored = { m, results: [], anchor: at };
    monitors.push(s);
    return s;
  };

  const dropBlocks = (project: string, id: string) => {
    const l = layouts.get(project);
    if (l) l.layout.blocks = l.layout.blocks.filter((b) => b.monitor !== id);
  };

  const dashboardView = (project: string): DashboardView => {
    const l = layouts.get(project);
    const mine = monitors.filter((s) => s.m.project === project);
    const blocks = (l?.layout.blocks ?? []).filter((b) => b.kind === "note" || mine.some((s) => s.m.id === b.monitor && kindsFor(s.m.kind).includes(b.kind)));
    return { project, blocks, updated_at: l?.at ?? null, updated_by: l?.by ?? null, monitors: mine.map(view).sort((a, b) => a.name.localeCompare(b.name)) };
  };

  /** One point per bucket (`monitors::layout::downsample`). */
  const downsample = (rows: MonitorResult[], bucketMs: number): MonitorPoint[] => {
    const out: MonitorPoint[] = [];
    let cur: { start: number; status: MonitorStatus; sum: number; n: number; min: number; max: number; count: number } | null = null;
    const rank = { ok: 0, unknown: 1, warn: 2, crit: 3 } as const;
    const flush = () => cur && out.push({ at: iso(cur.start), status: cur.status, count: cur.count, ...(cur.n ? { value: cur.sum / cur.n, min: cur.min, max: cur.max } : {}) });
    for (const r of rows) {
      const start = Math.floor(Date.parse(r.at) / bucketMs) * bucketMs;
      if (!cur || cur.start !== start) {
        flush();
        cur = { start, status: r.status, sum: 0, n: 0, min: Infinity, max: -Infinity, count: 0 };
      } else if (rank[r.status] > rank[cur.status]) cur.status = r.status;
      cur.count++;
      if (r.value !== undefined) {
        cur.sum += r.value;
        cur.n++;
        cur.min = Math.min(cur.min, r.value);
        cur.max = Math.max(cur.max, r.value);
      }
    }
    flush();
    return out;
  };

  const since = (v: string | undefined): number | undefined => {
    if (!v) return undefined;
    const secs = parseEvery(v);
    if (secs !== null && !/^\d+$/.test(v)) return ctx.now() - secs * 1000;
    const t = Date.parse(v);
    return Number.isFinite(t) ? t : ctx.fail(400, "bad_request", `bad \`since\` \`${v}\``);
  };

  const api: Api["monitors"] = {
    list: (project) =>
      ctx.reply(() => {
        ctx.project(project);
        canRead(project);
        return monitors.filter((s) => s.m.project === project).map(view).sort((a, b) => a.name.localeCompare(b.name));
      }),
    get: (id) =>
      ctx.reply(() => {
        const s = find(id);
        canRead(s.m.project);
        return view(s);
      }),
    create: (project, def) =>
      ctx.reply(() => {
        ctx.project(project);
        canWrite(project);
        const s = create(project, def, ctx.now());
        ctx.audit("monitor.create", `${project}/${s.m.name}`, project, { id: s.m.id, target: s.m.target, kind: s.m.kind });
        return view(s);
      }),
    update: (id, patch) =>
      ctx.reply(() => {
        const s = find(id);
        canWrite(s.m.project);
        const merged: Partial<MonitorDef> = { ...s.m };
        for (const [k, v] of Object.entries(patch)) {
          if (v === null) delete (merged as Record<string, unknown>)[k];
          else (merged as Record<string, unknown>)[k] = v;
        }
        const d = normalize(s.m.project, merged);
        if (d.name !== s.m.name && monitors.some((x) => x !== s && x.m.project === s.m.project && x.m.name === d.name)) ctx.fail(409, "conflict", `project \`${s.m.project}\` has a monitor named \`${d.name}\``, { field: "name" });
        const restart = JSON.stringify(d.schedule) !== JSON.stringify(s.m.schedule) || (d.enabled && !s.m.enabled);
        const at = ctx.now();
        Object.assign(s.m, d, { period_secs: periodOf(d.schedule), updated_at: iso(at) });
        if (!d.enabled) s.m.next_run_at = null;
        else if (restart || !s.m.next_run_at) {
          s.anchor = at;
          s.m.next_run_at = iso(at);
        }
        ctx.audit("monitor.update", `${s.m.project}/${s.m.name}`, s.m.project, { id: s.m.id, changed: Object.keys(patch) });
        return view(s);
      }),
    remove: (id) =>
      ctx.reply(() => {
        const s = find(id);
        canWrite(s.m.project);
        monitors.splice(monitors.indexOf(s), 1);
        dropBlocks(s.m.project, s.m.id);
        ctx.audit("monitor.delete", `${s.m.project}/${s.m.name}`, s.m.project, { id: s.m.id });
      }),
    run: (id) =>
      ctx.reply(() => {
        const s = find(id);
        canWrite(s.m.project);
        if (running.has(s.m.id)) ctx.fail(409, "conflict", `monitor \`${s.m.name}\` is running now; wait for the run to end`);
        ctx.audit("monitor.run", `${s.m.project}/${s.m.name}`, s.m.project, { id: s.m.id });
        const rep = execute(s.m, ctx.now());
        const r = record(s, rep.result);
        return { result: r, output: rep.output };
      }),
    test: (project, def) =>
      ctx.reply(() => {
        ctx.project(project);
        canWrite(project);
        const d = normalize(project, { name: "test-run", schedule: { kind: "interval", every: "1m" }, ...def, ...(def.name && NAME_RE.test(def.name) ? {} : { name: "test-run" }) });
        ctx.audit("monitor.test", d.target, project, { script: d.script.slice(0, 200) });
        return execute({ ...d, name: def.name && NAME_RE.test(def.name) ? def.name : d.name }, ctx.now());
      }),
    results: (id, q = {}) =>
      ctx.reply(() => {
        const s = find(id);
        canRead(s.m.project);
        const from = since(q.since);
        const until = q.until ? Date.parse(q.until) : undefined;
        const rows = s.results.filter((r) => (from === undefined || Date.parse(r.at) >= from) && (until === undefined || Date.parse(r.at) <= until));
        return rows.slice(-Math.min(MAX_POINTS, q.limit ?? 500));
      }),
    series: (id, q) =>
      ctx.reply(() => {
        const s = find(id);
        canRead(s.m.project);
        if (!(q.bucket >= 1 && q.bucket <= 7 * 86_400)) ctx.fail(400, "bad_request", `bad \`bucket\` \`${q.bucket}\`: use 1 s to 7 d`);
        const from = since(q.since);
        const rows = s.results.filter((r) => from === undefined || Date.parse(r.at) >= from);
        return downsample(rows, q.bucket * 1000).slice(-Math.min(MAX_POINTS, q.limit ?? 500));
      }),
    dashboard: (project) =>
      ctx.reply(() => {
        ctx.project(project);
        canRead(project);
        return dashboardView(project);
      }),
    saveDashboard: (project, layout) =>
      ctx.reply(() => {
        ctx.project(project);
        canWrite(project);
        const mine = monitors.filter((s) => s.m.project === project);
        const ids = new Set<string>();
        layout.blocks.forEach((b, i) => {
          const at = (f: string) => `blocks[${i}].${f}`;
          if (!/^[A-Za-z0-9_-]{1,40}$/.test(b.id)) field(at("id"), `\`${b.id}\` is not 1-40 of a-z, A-Z, 0-9, \`-\`, \`_\``);
          if (ids.has(b.id)) field(at("id"), `\`${b.id}\` is used twice`);
          ids.add(b.id);
          if (b.kind === "note") {
            if (!b.text?.trim()) field(at("text"), "a note needs Markdown text");
            return;
          }
          const s = mine.find((x) => x.m.id === b.monitor) ?? field(at("monitor"), `no monitor \`${b.monitor}\` in this project`);
          if (!kindsFor(s.m.kind).includes(b.kind)) field(at("kind"), `a ${b.kind} block does not fit monitor \`${s.m.name}\` (a ${s.m.kind} monitor)`);
        });
        layouts.set(project, { layout: { blocks: layout.blocks.map((b) => ({ ...b })) }, at: iso(ctx.now()), by: "operator" });
        ctx.audit("dashboard.update", project, project, { blocks: layout.blocks.length });
        ctx.emit({ kind: "dashboard.updated", actor: "operator", project, summary: `dashboard of ${project} saved (${layout.blocks.length} blocks)`, data: { project, blocks: layout.blocks.length } });
        return dashboardView(project);
      }),
  };

  /** Run the due monitors, as the daemon's tick does. */
  const tick = () => {
    const now = ctx.now();
    for (const s of monitors) {
      if (!s.m.enabled || !s.m.next_run_at || Date.parse(s.m.next_run_at) > now) continue;
      s.m.next_run_at = iso(nextRun(s, now));
      record(s, execute(s.m, now).result);
    }
  };

  /** Ten monitors of every kind on project `terrarium`, a week of history, and a dashboard. */
  const seed = () => {
    const now = ctx.now();
    const project = "terrarium";
    const host = ctx.targets().find((t) => t.kind === "host")?.name ?? "pi-runner";
    const defs: (Partial<MonitorDef> & { name: string; kind: MonitorKind })[] = [
      { name: "load", kind: "number", target: host, script: "cut -d' ' -f1 /proc/loadavg", schedule: { kind: "interval", every: "30s" }, warn: 4, crit: 8, description: "The 1-minute load average." },
      { name: "memory", kind: "number", target: host, unit: "%", script: "free | awk '/Mem:/ {printf \"%.1f\\n\", $3/$2*100}'", schedule: { kind: "interval", every: "1m" }, warn: 80, crit: 90 },
      { name: "disk-root", kind: "number", target: host, unit: "%", script: "df -P / | awk 'NR==2 {print $5}'", schedule: { kind: "interval", every: "5m" }, warn: 80, crit: 90 },
      { name: "uptime", kind: "text", target: host, script: "uptime", schedule: { kind: "interval", every: "5m" } },
      { name: "docker", kind: "status", target: host, script: 'if systemctl is-active --quiet docker; then echo "docker is active"; else echo "docker is not active"; exit 2; fi', schedule: { kind: "interval", every: "1m" } },
      { name: "containers", kind: "number", target: host, script: "docker ps -q | wc -l", schedule: { kind: "interval", every: "1m" }, warn: 12, crit: 20 },
      {
        name: "dashboard-http",
        kind: "status",
        target: host,
        script: 'code=$(curl -s -o /dev/null -w \'%{http_code}\' http://localhost:8080/)\necho "HTTP $code from http://localhost:8080/"\n[ "$code" = 200 ] || exit 2',
        schedule: { kind: "interval", every: "30s" },
      },
      { name: "api-latency", kind: "json", target: host, script: "/opt/checks/latency.sh", schedule: { kind: "interval", every: "30s" }, warn: 250, crit: 500, description: "p95 latency of the API, in ms." },
      { name: "top-cpu", kind: "json", target: host, script: "ps -eo pid,user,pcpu,pmem,comm --sort=-pcpu | head -6 | awk 'NR>1 {...}'", schedule: { kind: "interval", every: "1m" } },
      { name: "nightly-backup", kind: "status", target: "ci-box", script: "/opt/backup/check.sh", schedule: { kind: "cron", expr: "0 3 * * *", tz: "UTC" } },
    ];
    for (const d of defs) {
      const s = create(project, d, now - 7 * 86_400_000);
      // A week of history: one result every 10 minutes, then the monitor's own period for the last hour.
      const step = Math.max(s.m.period_secs * 1000, 600_000);
      const hourStart = now - 3_600_000;
      const fill = (from: number, to: number, every: number) => {
        for (let t = from; t <= to; t += every) record(s, execute(s.m, t).result, true);
      };
      if (s.m.schedule.kind === "cron") fill(now - 7 * 86_400_000, now - 10 * 3_600_000, 86_400_000);
      else {
        fill(now - 7 * 86_400_000, hourStart, step);
        fill(hourStart + s.m.period_secs * 1000, now - 5_000, s.m.period_secs * 1000);
      }
      s.anchor = now;
      s.m.next_run_at = iso(nextRun(s, now));
    }
    const id = (n: string) => monitors.find((s) => s.m.name === n)!.m.id;
    const blocks: DashboardBlock[] = [
      { id: "head-host", kind: "note", size: "l", text: "## Host health\nThe build host `" + host + "`. Thresholds: load 4 / 8, memory and disk 80 % / 90 %.", options: {} },
      { id: "load", kind: "stat", monitor: id("load"), size: "s", options: { decimals: 2 } },
      { id: "memory", kind: "stat", monitor: id("memory"), size: "s", options: { decimals: 1 } },
      { id: "disk", kind: "stat", monitor: id("disk-root"), size: "s", title: "Root disk used", options: { decimals: 0 } },
      { id: "docker", kind: "status", monitor: id("docker"), size: "s", title: "Docker service", options: {} },
      { id: "load-chart", kind: "chart", monitor: id("load"), size: "m", title: "Load average", options: { range: "24h" } },
      { id: "latency-chart", kind: "chart", monitor: id("api-latency"), size: "m", title: "API latency (p95)", options: { range: "6h" } },
      { id: "head-svc", kind: "note", size: "l", text: "## Services\nChecks of the services on the host. A status check exits with 0 (ok), 1 (warning), or 2 (critical).", options: {} },
      { id: "http", kind: "status", monitor: id("dashboard-http"), size: "s", title: "Dashboard HTTP", options: {} },
      { id: "containers", kind: "stat", monitor: id("containers"), size: "s", title: "Running containers", options: { decimals: 0, sparkline: true } },
      { id: "backup", kind: "status", monitor: id("nightly-backup"), size: "s", title: "Nightly backup", options: {} },
      { id: "latency", kind: "stat", monitor: id("api-latency"), size: "s", options: { decimals: 0 } },
      { id: "procs", kind: "table", monitor: id("top-cpu"), size: "m", title: "Top 5 processes by CPU", options: {} },
      { id: "uptime", kind: "text", monitor: id("uptime"), size: "m", options: {} },
      { id: "mem-week", kind: "chart", monitor: id("memory"), size: "l", title: "Memory used, last 7 days", options: { range: "7d" } },
    ];
    layouts.set(project, { layout: { blocks }, at: iso(now - 86_400_000), by: "operator" });
  };

  return { api, tick, seed };
}

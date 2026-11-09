// The schedules of the mock (docs/api.md §19), after the daemon's
// `api/schedules.rs` and `store/schedules.rs`: CRUD, enable and disable,
// run now (the answer is the run when it ends; a run while one goes on is
// `skipped`), the runs list (newest first, with a cursor), the `schedule`
// events with IDs only (`{change, schedule, name, project, run?, status?,
// trigger?}`), the daemon's `422 invalid` with `details.field` and the
// message `<field>: ...` (a cron error quotes the expression), and the
// rights (`403 forbidden`): the operator and a sysadmin everything; a
// project_admin in its project, with an action that stays inside it; a
// member reads its project only; the global schedules are for a sysadmin.
// A `file` schedule changes only in its file (`409 file_managed`).
// Triggers (spec `on`, §19.4) are rows of the same list; `?kind=time|trigger`
// filters them. The mock checks a trigger as the daemon does (the kinds, the
// match rules, dedupe, escalate, max_per_hour, and only mail, exec, card, or
// webhook), but it does not run triggers on events: the seed has trigger runs.
// A webhook action (§19.5) has a delivery log: a run-now or a test send
// makes a delivery at once; a redeliver makes a copy that ends after
// `runMs`. The mock answers from the URL: `fail` in it is a 503 (a retry),
// `404` a 404, `redirect` a 302 (no follow); any other URL is a 200.

import type { Api } from "../client";
import type { Agent, AgentEvent, MatchRule, Message, NewSchedule, Schedule, ScheduleAction, ScheduleRun, ScheduleSpec, SecretKind, Subscription, Target, TriggerSpec, WebhookAction, WebhookDelivery, WebhookTestResult } from "../types";
import { contentTypeOf, headerNameProblem, MAX_TIMEOUT, previewRequest, sampleEvent, templateProblem, urlProblem } from "../../features/schedules/webhook";
import { nextRuns, parseCron, validTimeZone } from "../../lib/cron";
import { paginate } from "../../lib/paging";
import { parseEvery } from "../../lib/schedule";
import type { MockWorld } from "./data";

/** Who calls the schedule endpoints. Default: the operator. */
export type ScheduleCaller = { role: "operator" } | { role: "sysadmin" } | { role: "project_admin"; project: string } | { role: "member"; project: string };

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const MIN_EVERY = 60;

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface ScheduleMockContext {
  w: MockWorld;
  now: () => number;
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: "schedule" | "webhook.delivery"; actor?: string; project?: string; agent?: string; agent_id?: string; summary: string; data: Record<string, unknown> }) => AgentEvent;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  agent: (key: string) => Agent;
  target: (name: string) => Target;
  sub: (name: string) => Subscription;
  project: (name: string) => unknown;
  sendMail: (to: string, from: string, subject: string, body: string) => Message;
  /** How long a run takes before `run_finished`. */
  runMs: number;
  caller?: ScheduleCaller;
  /** The secret that a webhook names: the project's first, then the global one (./secrets.ts). */
  webhookSecret?: (project: string | null, name: string) => { scope: string; kind: SecretKind; value: string; usable: boolean } | undefined;
}

/** When the schedule fires next, from `from` (ms), or null. */
export function computeNext(s: Pick<Schedule, "enabled" | "spec" | "last_run">, from: number): string | null {
  if (!s.enabled) return null;
  const spec = s.spec;
  if (spec.kind === "on" || spec.kind === "manual") return null;
  if (spec.kind === "at") {
    const at = Date.parse(spec.at);
    return s.last_run || Number.isNaN(at) ? null : new Date(at).toISOString();
  }
  if (spec.kind === "interval") {
    const sec = parseEvery(spec.every) ?? 3600;
    const base = s.last_run ? Date.parse(s.last_run.at) : from;
    let next = base + sec * 1000;
    while (next <= from) next += sec * 1000;
    return new Date(next).toISOString();
  }
  const p = parseCron(spec.expr);
  if (!p.ok) return null;
  const [n] = nextRuns(p.cron, from, spec.tz, 1);
  return n === undefined ? null : new Date(n).toISOString();
}

/** The seed: two schedules of terrarium, one of webshop, one global, two triggers, and a project action in each project; runs for each. */
export function seedSchedules(now: number, agents: Agent[]): { schedules: Schedule[]; runs: ScheduleRun[]; deliveries: WebhookDelivery[] } {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const id = (tag: string, n: number) => `01JB${tag.toUpperCase().padEnd(6, "0").slice(0, 6)}${String(n).padStart(16, "0")}`;
  const runs: ScheduleRun[] = [];
  let rn = 0;
  const run = (schedule: string, min: number, status: ScheduleRun["status"], summary: string, output: unknown = {}, error: string | null = null, trigger: ScheduleRun["trigger"] = "cron"): ScheduleRun => {
    const r: ScheduleRun = { id: id("run", ++rn), schedule, started_at: ago(min), finished_at: ago(min - 0.2), status, trigger, summary, output, error, actor: "operator", event_seq: null };
    runs.push(r);
    return r;
  };
  const last = (r: ScheduleRun) => ({ at: r.finished_at ?? r.started_at, status: r.status as "ok", summary: r.summary, run_id: r.id });
  const base = (n: number, name: string, project: string | null, spec: ScheduleSpec, action: ScheduleAction, enabled = true, source: Schedule["source"] = "api"): Schedule => ({
    id: id("sch", n),
    name,
    project,
    enabled,
    spec,
    action,
    catch_up: "skip",
    max_concurrency: 1,
    created_by: "operator",
    run_as: "operator",
    next_run_at: null,
    last_run: null,
    runs_count: 0,
    source,
    consecutive_skips: 0,
    created_at: ago(60 * 24 * 3),
    updated_at: ago(60 * 24 * 3),
  });
  const lead = agents.find((a) => a.name === "lead");
  const shop = agents.find((a) => a.name === "shop-lead");

  // A file schedule (`[[schedules]]` in projects/terrarium.toml): it changes only in the file.
  const standup = base(1, "standup", "terrarium", { kind: "cron", expr: "0 9 * * 1-5", tz: "UTC" }, { type: "mail", to: `terrarium/${lead?.name ?? "lead"}`, subject: "Standup", body: "Post the standup on the board: done, next, blocked." }, true, "file");
  for (const d of [3, 2, 1]) run(standup.id, 60 * 24 * d, "ok", "mail to terrarium/lead", { message_id: id("msg", d), thread_id: id("th", d), to: "terrarium/lead" });

  const tests = base(2, "tests", "terrarium", { kind: "interval", every: "15m" }, { type: "exec", target: "build-box", cmd: ["make", "test"], cwd: "/srv/terrarium", timeout_secs: 600 });
  tests.catch_up = "once";
  for (let i = 25; i >= 1; i--) {
    const bad = i === 7;
    run(
      tests.id,
      i * 15 + 3,
      bad ? "failed" : "ok",
      bad ? "exec on build-box: exit code 2" : "exec on build-box: exit 0",
      {
        target: "build-box",
        cmd: "make test",
        exit_code: bad ? 2 : 0,
        end: { kind: "exit", code: bad ? 2 : 0 },
        stdout: bad ? "running 214 tests\ntest mail::loop_guard ... FAILED\n" : "running 214 tests\ntest result: ok. 214 passed\n",
        stderr: bad ? "make: *** [test] Error 2\n" : "",
      },
      bad ? "exit code 2" : null,
    );
  }

  const nudge = base(3, "nudge-shop-lead", "webshop", { kind: "interval", every: "1h" }, { type: "turn", agent: `webshop/${shop?.name ?? "shop-lead"}`, text: "Check CI and pick the next card.", start_if_stopped: false });
  const turnOk = { agent: "webshop/shop-lead", started: false, delivery: "accepted" };
  run(nudge.id, 160, "ok", "turn to webshop/shop-lead", turnOk);
  run(nudge.id, 100, "ok", "turn to webshop/shop-lead", turnOk);
  run(nudge.id, 40, "failed", "failed: agent webshop/shop-lead is held by a quota hold", { agent: "webshop/shop-lead", started: false }, "agent webshop/shop-lead is held: quota hold on copilot-pro (premium 100%)");

  const probe = base(4, "probe-anthropic", null, { kind: "cron", expr: "0 */6 * * *", tz: "UTC" }, { type: "probe", subscription: "anthropic-max" }, false);
  run(probe.id, 60 * 30, "ok", "probe anthropic-max: ok", { subscription: "anthropic-max", windows: [{ window: "5h", used_pct: 42 }, { window: "7d", used_pct: 18 }] });
  run(probe.id, 60 * 26, "skipped", "skipped: a probe ran 4 minutes ago", {}, null, "catch_up");

  // A trigger (§19.4): CI failed on main mails the lead. Its runs name the event that started them.
  const ci = base(
    5,
    "ci-failed",
    "terrarium",
    {
      kind: "on",
      kinds: ["ci.run.failed"],
      match: [{ path: "data.branch", op: "eq", value: "main" }],
      dedupe: { key: "{{event.data.repo}}", window: "10m" },
      escalate: { key: "{{event.data.repo}}", after: 3, reset_on: ["ci.run.succeeded"], to: "operator" },
      max_per_hour: 30,
    },
    { type: "mail", to: `terrarium/${lead?.name ?? "lead"}`, subject: "CI failed: {{event.data.repo}}", body: "Fix the build on {{event.data.branch}}. Run: {{event.data.run_url}}" },
  );
  const evRun = (min: number, seq: number, kind: string, summary: string, status: ScheduleRun["status"], runSummary: string, output: unknown = {}) => {
    const r = run(ci.id, min, status, runSummary, output, null, "event");
    Object.assign(r, { actor: "source:terrarium/gh", event_seq: seq, event: { seq, kind, summary } });
  };
  evRun(60 * 20, 41, "ci.run.failed", "CI failed: wil/terrarium on main (ci.yml)", "ok", "mail to terrarium/lead", { message_id: id("msg", 7), thread_id: id("th", 7), to: "terrarium/lead" });
  evRun(60 * 2, 88, "ci.run.failed", "CI failed: wil/terrarium on main (ci.yml)", "ok", "mail to terrarium/lead", { message_id: id("msg", 8), thread_id: id("th", 8), to: "terrarium/lead" });
  evRun(60 * 2 - 30, 90, "ci.run.failed", "CI failed: wil/terrarium on main (ci.yml)", "skipped", "skipped: rate limit");

  // A webhook trigger (§19.5): a monitor that goes crit pages ntfy. Its deliveries: ok, a retry, a 404, a dropped note.
  const page = base(
    6,
    "page-ntfy",
    "terrarium",
    { kind: "on", kinds: ["monitor.status"], match: [{ path: "data.to", op: "eq", value: "crit" }], max_per_hour: 30 },
    { type: "webhook", url: "https://ntfy.example/terrarium-alerts", format: "ntfy", headers: [{ name: "Authorization", secret: "github-deploy" }] },
  );
  const deliveries: WebhookDelivery[] = [];
  const delivery = (n: number, min: number, seq: number | null, status: WebhookDelivery["status"], http: number | null, attempt: number, extra: Partial<WebhookDelivery> = {}): WebhookDelivery => {
    const d: WebhookDelivery = {
      id: id("dlv", n),
      schedule: page.id,
      event_seq: seq,
      event_kind: seq ? "monitor.status" : "",
      test: false,
      url: "https://ntfy.example/terrarium-alerts",
      method: "POST",
      content_type: "text/plain; charset=utf-8",
      attempt,
      status,
      http_status: http,
      duration_ms: http ? 80 + n * 7 : null,
      response: http === 200 ? '{"id":"mock","event":"message"}' : null,
      error: null,
      next_attempt_at: null,
      created_at: ago(min),
      updated_at: ago(min - 0.1),
      request: "monitor disk-usage: ok → crit 93 %",
      request_bytes: 36,
      ...extra,
    };
    deliveries.push(d);
    return d;
  };
  const pageRun = (min: number, seq: number, d: WebhookDelivery) => {
    const r = run(page.id, min, "ok", `webhook delivery ${d.id} queued (${d.url})`, { delivery: d.id, url: d.url }, null, "event");
    Object.assign(r, { actor: "system", event_seq: seq, event: { seq, kind: "monitor.status", summary: "monitor disk-usage: ok → crit 93 %" } });
  };
  pageRun(60 * 6, 61, delivery(1, 60 * 6, 61, "ok", 200, 1));
  pageRun(60 * 3, 72, delivery(2, 60 * 3, 72, "failed", 404, 1, { error: "status 404", response: "topic not found" }));
  pageRun(12, 95, delivery(3, 12, 95, "pending", 503, 2, { error: "status 503", response: "busy", next_attempt_at: new Date(now + 4 * 60_000).toISOString() }));
  delivery(4, 5, null, "dropped", null, 0, { dropped: 3, error: "rate limit: 3 events dropped in this minute", request: "", request_bytes: 0 });

  // Project actions (TER-53, spec `manual`): a button on the project dashboard and on Home runs them.
  const deploy = base(7, "deploy-staging", "terrarium", { kind: "manual" }, { type: "exec", target: "build-box", cmd: ["./deploy.sh", "staging"], cwd: "/srv/terrarium", timeout_secs: 900 });
  run(deploy.id, 60 * 5, "ok", "exec on build-box: exit 0", { target: "build-box", cmd: "./deploy.sh staging", exit_code: 0, end: { kind: "exit", code: 0 }, stdout: "deployed staging\n", stderr: "" }, null, "manual");
  const notify = base(8, "notify-shop", "webshop", { kind: "manual" }, { type: "mail", to: `webshop/${shop?.name ?? "shop-lead"}`, subject: "Release check", body: "Check the release notes before the deploy." });

  const schedules = [standup, tests, nudge, probe, ci, page, deploy, notify];
  for (const s of schedules) {
    const mine = runs.filter((r) => r.schedule === s.id);
    s.runs_count = mine.length;
    const l = mine.at(-1);
    s.last_run = l ? last(l) : null;
    s.next_run_at = computeNext(s, now);
  }
  return { schedules, runs, deliveries };
}

/** The `Api["schedules"]` of the mock. */
export function createScheduleMock(c: ScheduleMockContext): Api["schedules"] {
  const { w, fail } = c;
  // The daemon's `field_error`: the message starts with the field.
  const invalid = (field: string, message: string): never => fail(422, "invalid", `${field}: ${message}`, { field });
  const schedule = (key: string): Schedule => w.schedules.find((s) => s.id === key) ?? fail(404, "not_found", `schedule \`${key}\` not found`);
  const caller = () => c.caller ?? { role: "operator" as const };

  /** Read rights (`policy::read_schedules`): the global schedules need a sysadmin; a project needs a member of it. */
  const mayRead = (project: string | null) => {
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if (project === null) fail(403, "forbidden", "reading the global schedules needs sysadmin or the operator");
    if (who.project !== project) fail(403, "forbidden", `reading project \`${project}\` needs a member of it`);
  };
  const readable = (s: Schedule) => {
    try {
      mayRead(s.project);
      return true;
    } catch {
      return false;
    }
  };

  /** Write rights on a schedule of `project` (null: global), `policy::schedule_scope`. */
  const mayWrite = (project: string | null) => {
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if (project === null) fail(403, "forbidden", "managing a global schedule needs sysadmin or the operator");
    if (who.role === "member" || who.project !== project) fail(403, "forbidden", `managing the schedules of project \`${project}\` needs project_admin of \`${project}\`, sysadmin, or the operator`);
  };

  /** `policy::manage_schedule`: a project_admin's action stays inside its project. */
  const mayReach = (project: string | null, action: ScheduleAction, spec?: ScheduleSpec) => {
    const who = caller();
    if (who.role !== "project_admin" || !project) return;
    const outside = (what: string): never => fail(403, "forbidden", `a schedule of project \`${project}\` stays inside it: ${what}; that needs sysadmin or the operator`);
    if (action.type === "mail" && action.to !== "operator" && action.to !== `project:${project}` && !action.to.startsWith(`${project}/`)) outside(`mail to \`${action.to}\` leaves the project`);
    if ((action.type === "turn" || action.type === "agent") && !action.agent.startsWith(`${project}/`)) outside(`agent \`${action.agent}\` is in another project`);
    if (action.type === "exec") {
      const t = w.targets.find((x) => x.name === action.target);
      if (t?.project && t.project !== project) outside(`target \`${t.name}\` belongs to project \`${t.project}\``);
    }
    const to = spec?.kind === "on" ? spec.escalate?.to : undefined;
    if (to && to !== "operator" && to !== `project:${project}` && !to.startsWith(`${project}/`)) outside(`escalation to \`${to}\` leaves the project`);
  };

  const notFile = (s: Schedule) => {
    if (s.source === "file") fail(409, "file_managed", `schedule \`${s.project ? `${s.project}/` : ""}${s.name}\` is declared in projects/${s.project ?? ""}.toml; edit the file, then POST /api/reload`);
  };

  const checkSpec = (spec: ScheduleSpec | undefined): ScheduleSpec => {
    if (!spec || typeof spec !== "object") return invalid("spec", "a schedule needs `spec`");
    if (spec.kind === "cron") {
      const expr = typeof spec.expr === "string" ? spec.expr.trim() : "";
      const p = parseCron(expr);
      if (!p.ok) return invalid("spec.expr", `bad cron expression \`${expr}\`: ${p.error}`);
      const tz = spec.tz?.trim() || "UTC";
      if (!validTimeZone(tz)) return invalid("spec.tz", `\`${tz}\` is not an IANA time zone`);
      return { kind: "cron", expr, tz };
    }
    if (spec.kind === "interval") {
      const sec = parseEvery(spec.every ?? "");
      if (sec === null) return invalid("spec.every", `\`${String(spec.every)}\` is not a duration such as 15m or 1h30m`);
      if (sec < MIN_EVERY) return invalid("spec.every", `\`${spec.every}\` is shorter than the minimum of 1m`);
      return { kind: "interval", every: String(spec.every).trim() };
    }
    if (spec.kind === "at") {
      const at = Date.parse(spec.at ?? "");
      if (Number.isNaN(at)) return invalid("spec.at", `\`${String(spec.at)}\` is not an RFC 3339 time`);
      if (at <= c.now()) return invalid("spec.at", `\`${spec.at}\` is in the past`);
      return { kind: "at", at: new Date(at).toISOString() };
    }
    if (spec.kind === "on") return checkOn(spec);
    if (spec.kind === "manual") return { kind: "manual" };
    return invalid("spec.kind", `\`${String((spec as { kind?: unknown }).kind)}\` is not cron, interval, at, on, or manual`);
  };

  /** The daemon's `validate_on`, with its field names. */
  const checkOn = (spec: TriggerSpec): TriggerSpec => {
    const kinds = Array.isArray(spec.kinds) ? spec.kinds.map((k) => String(k).trim()) : [];
    if (!kinds.length) return invalid("spec.kinds", 'name one or more event kinds, for example ["ci.run.failed"]');
    if (kinds.some((k) => !k)) invalid("spec.kinds", "a kind glob is empty");
    const rules: MatchRule[] = [];
    for (const m of spec.match ?? []) {
      const p = m.path;
      const known = ["project", "kind", "actor", "summary"].includes(p) || (p.startsWith("data.") && p.slice(5).split(".").every((x) => x !== ""));
      if (!known) invalid("spec.match", `bad path \`${p}\`: use project, kind, actor, summary, or data.<path>`);
      const op = m.op ?? "eq";
      if (op === "in" && !(Array.isArray(m.value) && m.value.length)) invalid("spec.match", `\`in\` on \`${p}\` needs a list of values`);
      if (op === "glob" && !(typeof m.value === "string" && m.value)) invalid("spec.match", `\`glob\` on \`${p}\` needs a pattern`);
      if (op !== "in" && Array.isArray(m.value)) invalid("spec.match", `\`${op}\` on \`${p}\` needs one value`);
      rules.push({ path: p, op, value: m.value });
    }
    const out: TriggerSpec = { kind: "on", kinds, ...(rules.length ? { match: rules } : {}), max_per_hour: spec.max_per_hour ?? 30 };
    if (spec.dedupe) {
      if (!spec.dedupe.key?.trim()) invalid("spec.dedupe.key", "the dedupe key is empty");
      template("spec.dedupe.key", spec.dedupe.key);
      const sec = parseEvery(spec.dedupe.window ?? "");
      if (sec === null) invalid("spec.dedupe.window", `\`${String(spec.dedupe.window)}\` is not a duration such as 10m`);
      if (sec! < 1 || sec! > 7 * 86400) invalid("spec.dedupe.window", `\`${spec.dedupe.window}\` is not 1 s to 7 d`);
      out.dedupe = { key: spec.dedupe.key, window: spec.dedupe.window };
    }
    if (spec.escalate) {
      const e = spec.escalate;
      if (!e.key?.trim()) invalid("spec.escalate.key", "the escalation key is empty");
      template("spec.escalate.key", e.key);
      if (!Number.isInteger(e.after) || e.after < 1) invalid("spec.escalate.after", "use 1 or more");
      if ((e.reset_on ?? []).some((k) => !String(k).trim())) invalid("spec.escalate.reset_on", "a kind glob is empty");
      const to = e.to ?? "operator";
      if (!to.trim()) invalid("spec.escalate.to", "name an address");
      out.escalate = { key: e.key, after: e.after, ...(e.reset_on?.length ? { reset_on: e.reset_on } : {}), to };
    }
    const mph = out.max_per_hour!;
    if (!Number.isInteger(mph) || mph < 1 || mph > 600) invalid("spec.max_per_hour", `${String(mph)} is not 1-600`);
    return out;
  };

  /** The daemon's `template::check`: only the `{{event.*}}` placeholders. */
  const template = (field: string, text: string | undefined) => {
    for (const m of (text ?? "").matchAll(/\{\{\s*([^}]*?)\s*\}\}/g)) {
      const name = m[1]!;
      if (!/^event\.(kind|summary|project|seq|actor|data\.[A-Za-z0-9_.-]+)$/.test(name)) invalid(field, `unknown placeholder \`{{${name}}}\`: use event.kind, event.summary, event.project, event.seq, event.actor, or event.data.<path>`);
    }
  };

  /** A trigger runs only mail, exec, card, and webhook; its strings may use templates. */
  const checkTrigger = (spec: ScheduleSpec, action: ScheduleAction) => {
    if (spec.kind !== "on") return;
    if (action.type !== "mail" && action.type !== "exec" && action.type !== "card" && action.type !== "webhook") invalid("action", `a trigger runs a mail, exec, card, or webhook action, not \`${action.type}\`; to notify an agent, mail it`);
    if (action.type === "mail") {
      template("action.subject", action.subject);
      template("action.body", action.body);
    } else if (action.type === "exec") action.cmd.forEach((a) => template("action.cmd", a));
    else if (action.type === "card") {
      template("action.key", action.key);
      template("action.title", action.title);
      template("action.body", action.body);
    }
    if (spec.escalate?.to && spec.escalate.to !== "operator" && !spec.escalate.to.startsWith("project:")) {
      try {
        agentIn(spec.escalate.to, null, "spec.escalate.to");
      } catch {
        invalid("spec.escalate.to", `no agent \`${spec.escalate.to}\``);
      }
    }
  };

  /** An agent the action names, as `project/name`. A bare name is an agent of the schedule's project. */
  const agentIn = (key: string | undefined, project: string | null, field: string): string => {
    if (!key?.trim()) return invalid(field, "name the agent");
    const k = key.trim();
    let a: Agent;
    try {
      a = c.agent(project && !k.includes("/") && w.agents.some((x) => x.project === project && x.name === k) ? `${project}/${k}` : k);
    } catch {
      return invalid(field, `no agent \`${key}\``);
    }
    return `${a.project}/${a.name}`;
  };

  const checkAction = (action: ScheduleAction | undefined, project: string | null): ScheduleAction => {
    if (!action || typeof action !== "object") return invalid("action", "a schedule needs `action`");
    switch (action.type) {
      case "mail": {
        const to = action.to?.trim();
        if (!to) return invalid("action.to", "name the recipient");
        if (to.startsWith("project:")) {
          if (!w.projects.some((p) => `project:${p.name}` === to)) invalid("action.to", `no project inbox \`${to}\``);
        } else if (to !== "operator") agentIn(to, null, "action.to");
        return { type: "mail", to, subject: (action.subject ?? "").trim(), body: action.body ?? "", ...(action.thread ? { thread: action.thread } : {}) };
      }
      case "turn":
        if (!action.text?.trim()) invalid("action.text", "the text is empty");
        return { type: "turn", agent: agentIn(action.agent, project, "action.agent"), text: action.text, start_if_stopped: !!action.start_if_stopped };
      case "exec": {
        let t: Target;
        try {
          t = c.target(action.target ?? "");
        } catch {
          return invalid("action.target", `no target \`${String(action.target)}\``);
        }
        if (!Array.isArray(action.cmd) || !action.cmd.length || !action.cmd[0]?.trim()) invalid("action.cmd", "the command is empty");
        if (action.timeout_secs !== undefined && (!Number.isInteger(action.timeout_secs) || action.timeout_secs < 1 || action.timeout_secs > 3600)) invalid("action.timeout_secs", "the timeout is 1 to 3600 seconds");
        return { type: "exec", target: t.name, cmd: [...action.cmd], ...(action.cwd?.trim() ? { cwd: action.cwd.trim() } : {}), ...(action.timeout_secs ? { timeout_secs: action.timeout_secs } : {}) };
      }
      case "agent":
        if (!["start", "stop", "restart"].includes(action.verb)) invalid("action.verb", `\`${String(action.verb)}\` is not start, stop, or restart`);
        return { type: "agent", agent: agentIn(action.agent, project, "action.agent"), verb: action.verb };
      case "probe":
        try {
          c.sub(action.subscription ?? "");
        } catch {
          return invalid("action.subscription", `no subscription \`${String(action.subscription)}\``);
        }
        return { type: "probe", subscription: action.subscription };
      case "card": {
        if (!project) return invalid("action", "a card action needs a project schedule: the card goes on its board");
        if (action.op !== "open" && action.op !== "close") invalid("action.op", `\`${String(action.op)}\` is not open or close`);
        if (!action.key?.trim()) invalid("action.key", "name the card key");
        const column = action.column?.trim();
        if (column) {
          const cols = w.columns[project] ?? [];
          if (!cols.some((x) => x.name === column)) invalid("action.column", `project \`${project}\` has no column \`${column}\``);
          if (action.op === "open" && column === "done") invalid("action.column", "a card does not open in done");
        }
        return { type: "card", op: action.op, key: action.key, ...(action.title ? { title: action.title } : {}), ...(action.body ? { body: action.body } : {}), ...(column ? { column } : {}) };
      }
      case "webhook":
        return checkWebhook(action, project);
      default:
        return invalid("action.type", `\`${String((action as { type?: unknown }).type)}\` is not mail, turn, exec, agent, probe, card, or webhook`);
    }
  };

  /** The daemon's `WebhookAction::check`, `net::check_url`, and the secret rights (`webhooks::check_secrets`). */
  const checkWebhook = (a: WebhookAction, project: string | null): WebhookAction => {
    const hasUrl = !!a.url?.trim();
    if (!hasUrl && !a.url_secret) invalid("action.url", "give a URL or a url_secret");
    if (hasUrl && a.url_secret) invalid("action.url", "give a URL or a url_secret, not both");
    if (hasUrl) {
      const p = urlProblem(a.url!);
      if (p) invalid("action.url", p);
    }
    if (a.method && a.method !== "POST" && a.method !== "PUT") invalid("action", `unknown variant \`${String(a.method)}\`, expected \`POST\` or \`PUT\``);
    const format = a.format ?? "json";
    if (!["json", "slack", "discord", "ntfy", "template"].includes(format)) invalid("action", `unknown variant \`${String(format)}\``);
    if (a.timeout_secs !== undefined && (!Number.isInteger(a.timeout_secs) || a.timeout_secs < 1 || a.timeout_secs > MAX_TIMEOUT)) invalid("action.timeout_secs", `${String(a.timeout_secs)} is not 1-${MAX_TIMEOUT}`);
    if (format === "template") {
      if (!a.template) invalid("action.template", "format `template` needs a template");
      const p = templateProblem(a.template!);
      if (p) invalid("action.template", p);
    } else if (a.template) invalid("action.template", "a template needs format `template`");
    for (const h of a.headers ?? []) {
      const p = headerNameProblem(h.name);
      if (p) invalid("action.headers", p);
      const hasValue = h.value !== undefined;
      const hasSecret = h.secret !== undefined;
      if (hasValue === hasSecret || (hasSecret && !h.secret!.trim())) invalid("action.headers", `header \`${h.name}\`: give a value or a secret, not both`);
    }
    if (a.signing && !a.signing.secret?.trim()) invalid("action.signing.secret", "name a secret");
    const out: WebhookAction = { type: "webhook", ...(hasUrl ? { url: a.url!.trim() } : { url_secret: a.url_secret }), method: a.method ?? "POST", format, ...(a.template ? { template: a.template } : {}), ...(a.content_type ? { content_type: a.content_type } : {}), ...(a.headers?.length ? { headers: a.headers.map((h) => ({ ...h })) } : {}), ...(a.signing ? { signing: { secret: a.signing.secret, header: a.signing.header || "X-Terrarium-Signature", algo: "sha256" as const } } : {}), ...(a.timeout_secs ? { timeout_secs: a.timeout_secs } : {}) };
    // A template with a JSON content type must render JSON for a sample event.
    if (format === "template") {
      const r = previewRequest(out, sampleEvent("monitor.status", project, c.iso()), "sample", project);
      if (r.problem) invalid("action.template", r.problem);
    }
    const names: [string, string][] = [...(a.url_secret ? [["action.url_secret", a.url_secret] as [string, string]] : []), ...(a.headers ?? []).filter((h) => h.secret).map((h) => ["action.headers", h.secret!] as [string, string]), ...(a.signing?.secret ? [["action.signing.secret", a.signing.secret] as [string, string]] : [])];
    for (const [field, name] of names) {
      if (!c.webhookSecret) continue;
      const s = c.webhookSecret(project, name);
      if (!s) invalid(field, `no secret \`${name}\` in ${project ? `project \`${project}\` or the global secrets` : "the global secrets"}`);
      const who = caller();
      if (who.role === "project_admin" && !s!.usable)
        fail(403, "forbidden", `\`${name}\` is a global secret with no grant to your project: a webhook header or signature uses a global secret only with a \`use\` grant (terrarium secret grant ${name} project:${project})`);
    }
    return out;
  };

  /** A delivery of a webhook action now, answered from the URL (see the file comment). */
  const deliver = (s: Schedule, a: WebhookAction, opts: { seq?: number | null; kind?: string; test?: boolean; of?: WebhookDelivery }): { delivery: WebhookDelivery; request: WebhookTestResult["request"] } => {
    const kind = opts.kind ?? (s.spec.kind === "on" ? (s.spec.kinds[0] ?? "monitor.status") : "schedule.tick");
    const e = sampleEvent(kind, s.project, c.iso());
    const r = previewRequest(a, e, s.name, s.project);
    const url = a.url ?? "";
    const [status, http, error, response]: [WebhookDelivery["status"], number, string | null, string] = /redirect/.test(url)
      ? ["failed", 302, "status 302 (a redirect); Terrarium does not follow redirects, so give the final URL", ""]
      : /404/.test(url)
        ? ["failed", 404, "status 404", "not found"]
        : /fail/.test(url)
          ? [opts.test ? "failed" : "pending", 503, "status 503", "busy"]
          : ["ok", 200, null, a.format === "ntfy" ? '{"id":"mock","event":"message"}' : "ok"];
    const d: WebhookDelivery = {
      id: c.ulid("dlv"),
      schedule: s.id,
      event_seq: opts.seq ?? null,
      event_kind: opts.of?.event_kind ?? e.kind,
      test: !!opts.test,
      ...(opts.of ? { redelivery_of: opts.of.id } : {}),
      url: a.url_secret ? `(secret ${a.url_secret})` : url.replace(/\?.*$/, (q) => (q ? "?…" : "")),
      method: a.method ?? "POST",
      content_type: contentTypeOf(a),
      attempt: 1,
      status,
      http_status: http,
      duration_ms: 40 + Math.round(Math.random() * 80),
      response: response || null,
      error,
      next_attempt_at: status === "pending" ? new Date(c.now() + 10_000).toISOString() : null,
      created_at: c.iso(),
      updated_at: c.iso(),
      request: opts.of?.request ?? r.body,
      request_bytes: new TextEncoder().encode(opts.of?.request ?? r.body).length,
    };
    w.webhookDeliveries.push(d);
    s.webhook_failures = status === "ok" ? 0 : (s.webhook_failures ?? 0) + (d.test ? 0 : 1);
    c.emit({ kind: "webhook.delivery", actor: `schedule:${s.id}`, ...(s.project ? { project: s.project } : {}), summary: `webhook ${key(s)}: ${d.status} (${http})`, data: { trigger: s.id, name: s.name, delivery: d.id, status: d.status, http_status: http, attempt: d.attempt, test: d.test, event_seq: d.event_seq } });
    return { delivery: d, request: { method: r.method, url: d.url, headers: r.headers, body: r.body } };
  };

  const webhookOf = (s: Schedule): WebhookAction => (s.action.type === "webhook" ? s.action : fail(422, "invalid", `schedule \`${key(s)}\` runs a ${s.action.type} action, not a webhook`));

  const checkName = (name: unknown, project: string | null, self?: string) => {
    if (typeof name !== "string" || !NAME_RE.test(name)) invalid("name", `\`${String(name)}\` is not a valid name: lowercase letters, digits, - and _`);
    if (w.schedules.some((s) => s.name === name && s.project === project && s.id !== self)) fail(409, "conflict", `schedule \`${String(name)}\` exists in ${project ? `project \`${project}\`` : "the global scope"}`);
  };

  const checkProject = (p: unknown): string | null => {
    if (p === undefined || p === null || p === "") return null;
    if (typeof p !== "string" || !w.projects.some((x) => x.name === p)) return invalid("project", `no project \`${String(p)}\``);
    return p;
  };

  const key = (s: Schedule) => (s.project ? `${s.project}/${s.name}` : s.name);
  /** A disabled project action does not run: not by run, test, or redeliver (docs/api.md §19.10). */
  const actionOff = (s: Schedule) => {
    if (s.spec.kind === "manual" && !s.enabled) fail(409, "conflict", `action \`${key(s)}\` is disabled; enable it first (terrarium schedule enable ${key(s)})`);
  };
  /** The daemon's `emit`: IDs only. */
  const event = (s: Schedule, change: string, r?: ScheduleRun, actor = "operator") =>
    c.emit({
      kind: "schedule",
      actor,
      ...(s.project ? { project: s.project } : {}),
      summary: change === "run_finished" && r ? `schedule ${key(s)}: ${r.status} (${r.summary})` : `schedule ${key(s)}: ${change}`,
      data: { change, schedule: s.id, name: s.name, project: s.project, ...(r ? { run: r.id, status: r.status, trigger: r.trigger } : {}) },
    });

  /** What a run does, in the mock: the end, the summary, the output (a JSON value), and the error. */
  const execute = (s: Schedule): Pick<ScheduleRun, "status" | "summary" | "output" | "error"> => {
    const a = s.action;
    const failed = (summary: string, error: string, output: unknown = {}) => ({ status: "failed" as const, summary, output, error });
    try {
      switch (a.type) {
        case "mail": {
          const m = c.sendMail(a.to, s.project ? `project:${s.project}` : "operator", a.subject, a.body);
          return { status: "ok", summary: `mail to ${m.to_name}`, output: { message_id: m.id, thread_id: m.thread_id, to: m.to_name }, error: null };
        }
        case "turn": {
          const ag = c.agent(a.agent);
          // A stopped agent gets the turn as a send gives it: queued, and a wake starts it (TER-210, docs/api.md §19 turn).
          if (ag.status === "stopped" && !a.start_if_stopped) return { status: "ok", summary: `woke ${a.agent} and sent the turn`, output: { agent: a.agent, started: true, delivery: "queued" }, error: null };
          return { status: "ok", summary: `turn to ${a.agent}`, output: { agent: a.agent, started: ag.status === "stopped", delivery: "accepted" }, error: null };
        }
        case "exec": {
          const t = c.target(a.target);
          const line = a.cmd.join(" ");
          const offline = t.kind !== "ssh" && w.hosts.find((h) => h.name === t.host)?.status !== "online";
          if (offline) return failed(`exec on ${t.name}: host agent ${t.host ?? "?"} is not connected`, `host_offline: host agent \`${t.host ?? "?"}\` is not connected`, { target: t.name, cmd: line });
          if (a.cmd[0] === "false" || /\bfail\b/.test(line))
            return failed(`exec on ${t.name}: exit code 1`, "exit code 1", { target: t.name, cmd: line, exit_code: 1, end: { kind: "exit", code: 1 }, stdout: "", stderr: "(mock) the command failed\n" });
          return { status: "ok", summary: `exec on ${t.name}: exit 0`, output: { target: t.name, cmd: line, exit_code: 0, end: { kind: "exit", code: 0 }, stdout: `(mock) ran ${line}${a.cwd ? ` in ${a.cwd}` : ""}\nok\n`, stderr: "" }, error: null };
        }
        case "agent": {
          const ag = c.agent(a.agent);
          return { status: "ok", summary: `${a.verb} ${a.agent}`, output: { agent: a.agent, verb: a.verb, status: ag.status }, error: null };
        }
        case "card":
          // The mock does not touch the board: a run-now of a trigger fills each template with nothing.
          return { status: "ok", summary: `card ${a.op} trigger:${a.key.replace(/\{\{[^}]*\}\}/g, "")}`, output: { card: null, op: a.op, ...(a.op === "close" ? { column: a.column ?? "done" } : {}) }, error: null };
        case "webhook": {
          const { delivery: d } = deliver(s, a, {});
          return { status: "ok", summary: `webhook delivery ${d.id} queued (${d.url})`, output: { delivery: d.id, url: d.url }, error: null };
        }
        case "probe": {
          const sub = c.sub(a.subscription);
          return { status: "ok", summary: `probe ${sub.name}: ok`, output: { subscription: sub.name, windows: (sub.windows ?? []).map((x) => ({ window: x.window, used_pct: x.used_pct })) }, error: null };
        }
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      return failed(`failed: ${msg}`, msg);
    }
  };

  const view = (s: Schedule): Schedule => s;

  return {
    list: (q = {}) =>
      c.reply(() => {
        if (q.project) {
          c.project(q.project);
          mayRead(q.project);
        }
        const enabled = q.enabled === undefined ? undefined : String(q.enabled) === "true";
        const kind = q.kind as string | undefined;
        if (kind !== undefined && kind !== "time" && kind !== "trigger" && kind !== "action") fail(400, "bad_request", `kind: \`${kind}\` is not time, trigger, or action`);
        const group = (sp: ScheduleSpec) => (sp.kind === "on" ? "trigger" : sp.kind === "manual" ? "action" : "time");
        return w.schedules
          .filter((s) => readable(s) && (!q.project || s.project === null || s.project === q.project) && (enabled === undefined || s.enabled === enabled) && (!kind || group(s.spec) === kind))
          .slice()
          .sort((a, b) => a.name.localeCompare(b.name))
          .map(view);
      }),
    get: (id) =>
      c.reply(() => {
        const s = schedule(id);
        mayRead(s.project);
        return view(s);
      }),
    create: (body) =>
      c.reply(() => {
        const project = checkProject(body?.project);
        mayWrite(project);
        checkName(body?.name, project);
        const spec = checkSpec(body.spec);
        const action = checkAction(body.action, project);
        checkTrigger(spec, action);
        mayReach(project, action, spec);
        const catchUp = body.catch_up ?? "skip";
        if (catchUp !== "skip" && catchUp !== "once") invalid("catch_up", `\`${String(catchUp)}\` is not skip or once`);
        const s: Schedule = {
          id: c.ulid("sch"),
          name: body.name,
          project,
          enabled: body.enabled ?? true,
          spec,
          action,
          catch_up: catchUp,
          max_concurrency: 1,
          created_by: "operator",
          run_as: "operator",
          next_run_at: null,
          last_run: null,
          runs_count: 0,
          source: "api",
          consecutive_skips: 0,
          created_at: c.iso(),
          updated_at: c.iso(),
        };
        s.next_run_at = computeNext(s, c.now());
        w.schedules.push(s);
        c.audit("schedule.create", s.name, s.project ?? undefined, { id: s.id, spec, action: action.type });
        event(s, "created");
        return view(s);
      }),
    update: (id, patch) =>
      c.reply(() => {
        const s = schedule(id);
        mayRead(s.project);
        notFile(s);
        mayWrite(s.project);
        // The daemon's PATCH body has no `project` (deny_unknown_fields): the scope does not change.
        if ("project" in (patch as object)) fail(422, "invalid", "unknown field `project`, expected one of `name`, `spec`, `action`, `enabled`, `catch_up`");
        const project = s.project;
        const name = patch.name ?? s.name;
        checkName(name, project, s.id);
        const spec = patch.spec ? checkSpec(patch.spec) : s.spec;
        const action = checkAction(patch.action ?? s.action, project);
        checkTrigger(spec, action);
        mayReach(project, action, spec);
        const catchUp = patch.catch_up ?? s.catch_up;
        if (catchUp !== "skip" && catchUp !== "once") invalid("catch_up", `\`${String(catchUp)}\` is not skip or once`);
        Object.assign(s, { name, spec, action, catch_up: catchUp, enabled: patch.enabled ?? s.enabled, updated_at: c.iso() } satisfies Partial<NewSchedule> & { updated_at: string });
        s.next_run_at = computeNext(s, c.now());
        c.audit("schedule.update", s.name, s.project ?? undefined, { id: s.id, fields: Object.keys(patch) });
        event(s, "updated");
        return view(s);
      }),
    remove: (id) =>
      c.reply(() => {
        const s = schedule(id);
        mayRead(s.project);
        notFile(s);
        mayWrite(s.project);
        w.schedules.splice(w.schedules.indexOf(s), 1);
        w.scheduleRuns = w.scheduleRuns.filter((r) => r.schedule !== s.id);
        c.audit("schedule.delete", s.name, s.project ?? undefined, { id: s.id });
        event(s, "deleted");
      }),
    enable: (id) => c.reply(() => toggle(id, true)),
    disable: (id) => c.reply(() => toggle(id, false)),
    // `POST .../run` answers with the run when it ends; `run_started` comes at once.
    run: (id) =>
      c
        .reply(() => {
          const s = schedule(id);
          mayRead(s.project);
          actionOff(s);
          mayWrite(s.project);
          mayReach(s.project, s.action);
          const r: ScheduleRun = { id: c.ulid("run"), schedule: s.id, started_at: c.iso(), finished_at: null, status: "running", trigger: "manual", summary: "running", output: {}, error: null, actor: "operator", event_seq: null };
          w.scheduleRuns.push(r);
          const max = s.max_concurrency ?? 1;
          if (w.scheduleRuns.filter((x) => x.schedule === s.id && x.status === "running").length > max) {
            // `Begin::Overlap`: the run is skipped at once.
            Object.assign(r, { status: "skipped", summary: "skipped: another run is still running", finished_at: c.iso() });
            done(s, r);
            return { run: r, wait: false };
          }
          c.audit("schedule.run", s.name, s.project ?? undefined, { id: s.id, run: r.id });
          event(s, "run_started", r);
          return { run: r, wait: true };
        })
        .then(
          ({ run: r, wait }) =>
            new Promise<ScheduleRun>((resolve) => {
              if (!wait) return resolve(structuredClone(r));
              setTimeout(() => {
                const live = w.schedules.find((x) => x.id === r.schedule);
                const stored = w.scheduleRuns.find((x) => x.id === r.id);
                if (stored && live) {
                  Object.assign(stored, execute(live), { finished_at: c.iso() });
                  done(live, stored);
                }
                resolve(structuredClone(stored ?? r));
              }, c.runMs);
            }),
        ),
    runs: (id, q = {}) =>
      c.reply(() => {
        const s = schedule(id);
        const list = w.scheduleRuns.filter((r) => r.schedule === s.id).reverse();
        return paginate(list, { limit: q.limit ?? 20, cursor: q.cursor }, (r) => r.id);
      }),
    deliveries: (id, q = {}) =>
      c.reply(() => {
        const s = schedule(id);
        mayRead(s.project);
        const list = w.webhookDeliveries.filter((d) => d.schedule === s.id).reverse();
        return paginate(structuredClone(list), { limit: q.limit ?? 100, cursor: q.cursor }, (d) => d.id);
      }),
    // `202`: a copy, due now. The mock ends it after `runMs`.
    redeliver: (id, delivery) =>
      c.reply(() => {
        const s = schedule(id);
        mayRead(s.project);
        actionOff(s);
        mayWrite(s.project);
        const a = webhookOf(s);
        const old = w.webhookDeliveries.find((d) => d.id === delivery && d.schedule === s.id) ?? fail(404, "not_found", `no webhook delivery \`${delivery}\` of schedule \`${key(s)}\``);
        if (old.status === "dropped") fail(422, "invalid", "a `dropped` row is a rate-limit note: it has no request to send again");
        const { delivery: d } = deliver(s, a, { seq: old.event_seq, of: old });
        const done = { ...d };
        Object.assign(d, { status: "pending", attempt: 0, http_status: null, duration_ms: null, response: null, error: null, next_attempt_at: c.iso() });
        const answer = structuredClone(d);
        setTimeout(() => Object.assign(d, done, { updated_at: c.iso() }), c.runMs);
        c.audit("schedule.webhook.redeliver", key(s), s.project ?? undefined, { id: s.id, delivery: d.id, of: old.id });
        return answer;
      }),
    test: (id) =>
      c.reply(() => {
        const s = schedule(id);
        mayRead(s.project);
        actionOff(s);
        mayWrite(s.project);
        const a = webhookOf(s);
        checkWebhook(a, s.project);
        const r = deliver(s, a, { test: true });
        return structuredClone({ test: true as const, action: "webhook" as const, matched: null, status: r.delivery.status === "ok" ? ("ok" as const) : ("failed" as const), summary: `webhook delivery ${r.delivery.id}: ${r.delivery.status}`, ...r });
      }),
  };

  /** The end of a run: the last run, the count, the next run, and `run_finished`. */
  function done(s: Schedule, r: ScheduleRun) {
    s.runs_count++;
    s.last_run = { at: r.finished_at ?? r.started_at, status: r.status as "ok", summary: r.summary, run_id: r.id };
    s.next_run_at = computeNext(s, c.now());
    event(s, "run_finished", r, "system");
  }

  function toggle(id: string, on: boolean): Schedule {
    const s = schedule(id);
    mayRead(s.project);
    notFile(s);
    if (on) mayReach(s.project, s.action);
    mayWrite(s.project);
    s.enabled = on;
    s.updated_at = c.iso();
    s.next_run_at = computeNext(s, c.now());
    c.audit(on ? "schedule.enable" : "schedule.disable", s.name, s.project ?? undefined, { id: s.id });
    event(s, on ? "enabled" : "disabled");
    return view(s);
  }
}

// In-memory implementation of `Api` for VITE_MOCK=1 and for tests.
// Every endpoint works. State lives in one `MockWorld` per instance.
//
// The mock follows docs/api.md: the same shapes, the same error bodies and
// codes, the same list order and cursors, and the same event frames, so a
// view that works on the mock works on the daemon. Subscriptions, quota,
// usage, and login sessions follow docs/api.md §16. Hosts, target actions,
// and exec sessions follow the daemon's new `api/hosts.rs` and
// `api/targets.rs`, which docs/api.md does not describe yet.
//
// Auth (docs/api.md §1.1). Without `client`, the mock lets every call in,
// as the Unix socket does; the unit tests use that. With `client` (the
// browser mock), the mock acts as the TCP listener: `session.create` sets
// an in-memory "cookie" (the browser keeps the real one), every call needs
// that cookie or the bearer token, a mutating call with the cookie needs
// `X-CSRF-Token` (else 403), and a socket opens without a subprotocol when
// the cookie is good.

import type { Api } from "../client";
import { ApiError, AuthError, isMutating, type Page } from "../http";
import { createWsEventStream, type EventStream, type RawSocket, type SocketEvent } from "../sockets";
import { MOCK_TOKEN, mockSocket, type MockSocketAuth } from "./ws";
import { httpVerbs } from "./verbs";
import { splitFrontMatter } from "../../lib/markdown";
import type {
  Agent,
  AgentEvent,
  AgentStatus,
  Artifact,
  ArtifactAttach,
  ArtifactLinkKind,
  Memory,
  WikiPageSummary,
  AuditEntry,
  Card,
  CardComment,
  ColumnEdit,
  EventQuery,
  Host,
  LoginKind,
  LoginSession,
  Message,
  Project,
  Repo,
  Role,
  Subscription,
  Lease,
  NewTarget,
  Target,
  TargetReport,
  UsagePoint,
  UsageQuery,
  UsageTotals,
  Version,
  WikiPage,
} from "../types";
import { paginate, type PageQuery } from "../../lib/paging";
import { fakeUlid, seed, type MockWorld, type UsageRow } from "./data";
import { FakePty } from "./pty";

export interface MockOptions {
  /** Artificial latency for every call. Default 120 ms; tests use 0. */
  latencyMs?: number;
  /** Interval of the fake activity (tool calls, turns). 0 turns it off. Default 3000 ms. */
  eventsIntervalMs?: number;
  /** Delay before a device-code login completes by itself. Default 8000 ms. 0: never. */
  autoLoginMs?: number;
  /** Delay before a login session is `waiting` (a URL is on screen). Default 800 ms; 0 with no latency. */
  loginStepMs?: number;
  /** How long a login session lives. Default ten minutes (docs/api.md §16.4). */
  loginTtlMs?: number;
  /** Delay of each fake supervisor step (`starting` -> `idle`). Default 300 ms, like `TERRARIUM_FAKE_DELAY_MS`. */
  stepMs?: number;
  world?: MockWorld;
  now?: () => number;
  /**
   * The browser side: what the client would send. With it, the mock checks
   * auth like the TCP listener. Without it, every call is in.
   */
  client?: MockClient;
  /** False: a daemon without sessions. `session.create` gets 404, so the client answers null. Default true. */
  sessions?: boolean;
  /** How long a session lives. Default 12 hours. */
  sessionTtlMs?: number;
  /**
   * Where the mock keeps its "cookie", so that a reload keeps the session as
   * a browser does. The browser mock passes sessionStorage. Default: memory only.
   */
  cookieJar?: { getItem(k: string): string | null; setItem(k: string, v: string): void; removeItem(k: string): void } | null;
  /**
   * Act as a caller that is not a sysadmin when it defines a target: a
   * `POST /api/targets` without `project` is `403 forbidden`, as the daemon
   * answers a project_admin (docs/api.md §17.2). Default false: the mock
   * caller is the operator, who may define every target.
   */
  denyGlobalTargets?: boolean;
  /** `TERRARIUM_MAX_TARGETS_PER_PROJECT` (docs/api.md §17.2). Default 8. */
  maxTargetsPerProject?: number;
}

/** What the mock reads from the client: the headers that the HTTP transport would send. */
export interface MockClient {
  /** `Authorization: Bearer`, in bearer mode. */
  token(): string | null;
  /** `X-CSRF-Token`, with a session. */
  csrf(): string | null;
  /** The transport's 401 hook. */
  onUnauthorized?(): void;
}

/** The mock's "cookie jar" key. Mock only: a real session cookie is `HttpOnly`. */
export const MOCK_COOKIE_KEY = "terrarium.mock.cookie";

const clone = <T>(v: T): T => structuredClone(v);

/** The daemon's error body and status (docs/api.md §2.5). */
export function fail(status: number, code: string, message: string, details?: unknown): never {
  const error = details === undefined ? { code, message } : { code, message, details };
  throw new ApiError(status, message, { error }, code, details);
}

/** A small line diff for the wiki mock: `-` lines left, `+` lines added. */
export function lineDiff(a: string, b: string): string {
  const la = a.split("\n");
  const lb = b.split("\n");
  return [...la.filter((l) => !lb.includes(l)).map((l) => `- ${l}`), ...lb.filter((l) => !la.includes(l)).map((l) => `+ ${l}`)].join("\n");
}

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const PREFIX_RE = /^[A-Z][A-Z0-9]{0,9}$/;
const REQUIRED_COLUMNS = ["backlog", "in_progress", "blocked", "done"];
const LIVE: AgentStatus[] = ["starting", "idle", "working", "blocked", "restarting"];

/** Test and demo hooks: drive the quota policy without a timer. */
export interface MockSim {
  /** Set a window's used percent, as a probe would read it, and run the quota policy. */
  setWindow(subscription: string, window: string, usedPct: number): void;
  /** Emit `subscription.offer` for each detected login with no subscription, as the daemon does at start. */
  offerLogins(): void;
}

export function createMockApi(opts: MockOptions = {}): Api & { world: MockWorld; sim: MockSim } {
  const now = opts.now ?? Date.now;
  const latency = opts.latencyMs ?? 120;
  const step = opts.stepMs ?? (latency > 0 ? 300 : 0);
  const w = opts.world ?? seed(now());
  const iso = () => new Date(now()).toISOString();
  let counter = 1000;
  const ulid = (tag: string) => fakeUlid(tag, ++counter);
  const startedAt = now();
  const later = (fn: () => void, ms = step) => setTimeout(fn, ms);

  const reply = <T>(fn: () => T): Promise<T> =>
    new Promise<T>((resolve, reject) => {
      const run = () => {
        try {
          resolve(clone(fn()));
        } catch (e) {
          reject(e);
        }
      };
      if (latency > 0) setTimeout(run, latency);
      else queueMicrotask(run);
    });

  const notFound = (what: string): never => fail(404, "not_found", `${what} not found`);
  const noRoute = (): never => fail(404, "not_found", "no such API route");
  const find = <T>(list: T[], pred: (x: T) => boolean, what: string): T => list.find(pred) ?? notFound(what);
  const project = (name: string) => find(w.projects, (p) => p.name === name, `project \`${name}\``);
  // The target as the API shows it (docs/api.md §17.2). The mock caller is the operator: `access` is `operator`, and the operator holds no lease.
  const targetView = (t: Target): Target => ({
    ...t,
    scope: t.project ? "project" : "global",
    host_online: t.kind === "ssh" ? null : w.hosts.some((h) => h.name === t.host && h.status === "online"),
    access: "operator",
    lease: null,
    runtime: t.runtime ?? {},
  });
  // The checks of the daemon's `store/targets.rs` validate (docs/api.md §17.2): a bad value is `422 invalid`.
  const targetDef = (b: NewTarget): NewTarget => {
    if (!b || typeof b.name !== "string" || !/^[a-z0-9][a-z0-9._-]*$/i.test(b.name)) fail(422, "invalid", "bad target name");
    if (!["host", "ssh", "docker", "vmlab"].includes(b.kind)) fail(422, "invalid", "`kind` must be host, ssh, docker, or vmlab");
    for (const f of ["host", "address", "credential", "image", "container", "machine", "lab_ref"] as const) {
      const v = b[f];
      if (v !== undefined && (typeof v !== "string" || !v || /\s/.test(v) || v.startsWith("-"))) fail(422, "invalid", `bad \`${f}\` \`${String(v)}\`: not empty, no spaces, no leading \`-\``);
    }
    const p = b.lab_path;
    if (p !== undefined && (typeof p !== "string" || p.startsWith("/") || p.split("/").some((x) => x === ".." || x.startsWith("-")) || /\s/.test(p)))
      fail(422, "invalid", `bad \`lab_path\` \`${String(p)}\`: a relative directory with no \`..\``);
    const need = (f: string, ok: boolean) => ok || fail(422, "invalid", `a \`${b.kind}\` target needs \`${f}\``);
    if (b.kind === "host") need("host", !!b.host);
    if (b.kind === "ssh") need("address", !!b.address);
    if (b.kind === "docker") {
      need("host", !!b.host);
      need("image` or `container", !!(b.image || b.container));
    }
    if (b.kind === "vmlab") {
      need("host", !!b.host);
      // docs/api.md §17.2: one of `lab`, `lab_repo`, and `lab_from_workspace`.
      if ([!!b.lab, !!b.lab_repo, !!b.lab_from_workspace].filter(Boolean).length !== 1)
        fail(422, "invalid", "a `vmlab` target needs one of `lab` (a lab dir on the host), `lab_repo` (a repo of the project), and `lab_from_workspace = true`");
      if ((b.lab_repo || b.lab_from_workspace) && !b.project) fail(422, "invalid", "`lab_repo` and `lab_from_workspace` need a project target");
      if (b.lab_from_workspace && b.snapshot_before_task) fail(422, "invalid", "`snapshot_before_task` does not work with `lab_from_workspace`: each holder has its own instance");
      if (!b.lab && !b.machine) fail(422, "invalid", "`lab_repo` and `lab_from_workspace` need `machine`: the machine that runs the commands and gets the workspace");
      if (b.lab && (b.lab_ref || b.lab_path)) fail(422, "invalid", "`lab_ref` and `lab_path` go with `lab_repo` or `lab_from_workspace`");
      if (b.lab && !b.lab.startsWith("/")) fail(422, "invalid", "`lab` must be an absolute path");
    } else if (b.lab_repo || b.lab_ref || b.lab_path || b.lab_from_workspace) {
      fail(422, "invalid", `\`lab_repo\`, \`lab_ref\`, \`lab_path\`, and \`lab_from_workspace\` are for a vmlab target, not \`${b.kind}\``);
    }
    if (b.ephemeral && !(b.kind === "vmlab" || (b.kind === "docker" && b.image))) fail(422, "invalid", "an ephemeral target must be docker with `image`, or vmlab");
    const def: NewTarget = { name: b.name, kind: b.kind };
    for (const [k, v] of Object.entries(b) as [keyof NewTarget, unknown][]) if (v !== undefined && v !== null && v !== "" && v !== false) (def as unknown as Record<string, unknown>)[k] = v;
    return def;
  };
  // docs/api.md §17.3, §17.8: a verb on a `lab_from_workspace` target acts on the instance of one holder (`agent`).
  const instanceVerb = (t: Target, verb: string, key: string | undefined, online: boolean): TargetReport => {
    if (!key) fail(422, "invalid", `\`${t.name}\` gives each holder its own lab instance: name the holder with \`agent\``);
    const a = agent(key!);
    if (!online) fail(409, "conflict", `host agent \`${t.host ?? "?"}\` of target \`${t.name}\` is offline`);
    const instances = { ...(t.runtime?.instances ?? {}) };
    const cur = instances[a.id];
    if (verb === "snapshot" || verb === "restore") {
      if (!cur || cur.state !== "running") fail(409, "conflict", `\`${t.name}\` has no instance of \`${a.project}/${a.name}\` on the host: bring it up first`);
      fail(422, "invalid", "the mock has no snapshots of a lab instance");
    }
    if (verb === "up" && cur?.state !== "running") {
      checkRunningCap(t, a.id);
      const ws = `${a.project}/${a.name}`;
      instances[a.id] = { agent_name: ws, lab: `/var/lib/terrarium-host/instances/${t.name}.${a.id}/lab`, state: "running", attached: [ws] };
    } else if (verb === "down" && cur) {
      if (t.ephemeral && !t.keep) delete instances[a.id];
      else instances[a.id] = { ...cur, state: "stopped" };
    }
    t.runtime = { ...t.runtime, instances };
    t.state_at = iso();
    const state = instances[a.id]?.state ?? "absent";
    const r: TargetReport = { target: t.name, verb, state };
    audit(`target.${verb}`, t.name, t.project, { ...r, agent: a.id });
    return r;
  };
  // docs/api.md §17.3: the docker and vmlab targets (and lab instances) that run on a host, without the one that starts.
  const runningOn = (host: string, except: { target: string; agent?: string }) => {
    let n = 0;
    for (const t of w.targets) {
      if (t.host !== host || (t.kind !== "docker" && t.kind !== "vmlab")) continue;
      if (t.lab_from_workspace) n += Object.entries(t.runtime?.instances ?? {}).filter(([k, i]) => i.state === "running" && !(t.name === except.target && k === except.agent)).length;
      else if (t.state === "running" && t.name !== except.target) n += 1;
    }
    return n;
  };
  const checkRunningCap = (t: Target, agent?: string) => {
    const h = w.hosts.find((x) => x.name === t.host);
    const max = h?.max_running_targets;
    if (!h || max === undefined) return;
    const running = runningOn(h.name, { target: t.name, agent });
    if (running >= max) fail(429, "rate_limited", `host \`${h.name}\` runs ${running} targets, and its max_running_targets is ${max}; bring one down first`);
  };

  /** An agent key: a ULID, `project/name`, or a unique bare name (docs/api.md §2.2). */
  const agent = (key: string): Agent => {
    const byId = w.agents.find((a) => a.id === key);
    if (byId) return byId;
    if (key.includes("/")) {
      const [p, n] = key.split("/", 2);
      return find(w.agents, (a) => a.project === p && a.name === n, `agent \`${key}\``);
    }
    const named = w.agents.filter((a) => a.name === key);
    if (named.length > 1) fail(400, "bad_request", `agent name \`${key}\` is in more than one project; use project/name`);
    return named[0] ?? notFound(`agent \`${key}\``);
  };
  const cardById = (id: string) => find(w.cards, (c) => c.id.toUpperCase() === id.toUpperCase(), `card ${id}`);
  const version = (id: string) => find(w.versions, (v) => v.id === id || `${v.project}/${v.name}` === id || v.name === id, `version \`${id}\``);
  const sub = (name: string) => find(w.subscriptions, (s) => s.name === name, `subscription ${name}`);

  // ----- addresses (docs/api.md §2.3) -----
  type Addr = { addr: string; kind: Message["from_kind"]; label: string; project?: string };
  const address = (raw: string, senderProject?: string): Addr => {
    if (raw === "operator") return { addr: "operator", kind: "operator", label: "operator" };
    if (raw.startsWith("project:")) {
      const p = project(raw.slice(8));
      return { addr: `project:${p.name}`, kind: "project", label: `project:${p.name}`, project: p.name };
    }
    const local = senderProject && !raw.includes("/") ? w.agents.find((a) => a.project === senderProject && a.name === raw) : undefined;
    const a = local ?? agent(raw);
    return { addr: a.id, kind: "agent", label: `${a.project}/${a.name}`, project: a.project };
  };

  // ----- audit (docs/api.md §4) -----
  const audit = (action: string, target: string, proj: string | undefined, detail: Record<string, unknown> = {}, actor = "operator") => {
    const seq = (w.audit.at(-1)?.seq ?? 0) + 1;
    const row: AuditEntry = { id: ulid("aud"), seq, at: iso(), actor, action, target, ...(proj ? { project: proj } : {}), detail };
    w.audit.push(row);
    return row;
  };

  // ----- event log and live hub (docs/api.md §9) -----
  const hub = new Set<(e: AgentEvent) => void>();
  type Draft = Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> };
  const emit = (d: Draft): AgentEvent => {
    w.seq++;
    const data = { ...(d.data ?? {}) };
    if (d.agent_id) data.agent_id ??= d.agent_id;
    if (d.card) data.card_id ??= d.card;
    const e: AgentEvent = { seq: w.seq, id: fakeUlid("ev", w.seq), at: iso(), actor: "system", ...d, data };
    w.events.push(e);
    if (w.events.length > 2000) w.events.splice(0, w.events.length - 2000);
    hub.forEach((fn) => fn(clone(e)));
    return e;
  };
  const agentEvent = (a: Agent, kind: AgentEvent["kind"], summary: string, data: Record<string, unknown> = {}, actor = "system") =>
    emit({ kind, actor, agent: a.name, agent_id: a.id, project: a.project, session: w.sessions.find((s) => s.agent === a.id && s.current)?.id, summary, data });
  const setStatus = (a: Agent, status: AgentStatus, reason: string | null = null) => {
    const from = a.status;
    if (from === status) return;
    a.status = status;
    a.updated_at = iso();
    agentEvent(a, "status", `${a.name}: ${from} -> ${status}`, { status, from, reason });
  };
  const openSession = (a: Agent) => {
    for (const s of w.sessions.filter((x) => x.agent === a.id && x.current)) {
      s.current = false;
      s.ended_at = iso();
      s.outcome = "restarted";
    }
    const harness = `fake-${ulid("hs")}`;
    w.sessions.unshift({ id: ulid("ses"), agent: a.id, harness_session_id: harness, started_at: iso(), turns: 0, current: true });
    a.session = harness;
    agentEvent(a, "session_started", `${a.name}: session started`, { harness_session: harness });
  };
  const endSession = (a: Agent, outcome: string) => {
    for (const s of w.sessions.filter((x) => x.agent === a.id && x.current)) {
      s.current = false;
      s.ended_at = iso();
      s.outcome = outcome;
    }
    delete a.session;
  };

  const eventsAfter = (q: EventQuery, base: (e: AgentEvent) => boolean) => {
    const since = typeof q.since === "string" && !/^\d+$/.test(q.since) ? (w.events.find((e) => e.id === q.since)?.seq ?? fail(400, "bad_request", `no event \`${q.since}\` for since`)) : Number(q.since ?? 0);
    const after = q.cursor ? Number(q.cursor) : since;
    const rows = w.events.filter((e) => e.seq > after && base(e) && (!q.kind || e.kind === q.kind) && (!q.project || e.project === q.project));
    return paginate(rows, { limit: q.limit }, (e) => String(e.seq));
  };

  /** A fake WebSocket to `/api/events`: the backlog after `?since=`, then live frames. */
  class MockEventSocket implements RawSocket {
    private handlers: Record<string, ((ev: SocketEvent) => void)[]> = {};
    private off: (() => void) | null = null;
    readyState = 0;
    binaryType: BinaryType = "blob";
    readonly protocol: string;
    /** `protocol` is what the "daemon" echoes (./ws); null refuses the upgrade. */
    constructor(url: string, protocol: string | null = "") {
      this.protocol = protocol ?? "";
      const since = Number(new URL(url, "http://mock").searchParams.get("since") ?? 0);
      queueMicrotask(() => {
        if (this.readyState !== 0) return;
        if (protocol === null) {
          this.readyState = 3;
          this.fire("error", {});
          this.fire("close", { code: 1006 });
          return;
        }
        this.readyState = 1;
        this.fire("open", {});
        let last = since;
        for (const e of w.events.filter((x) => x.seq > since)) {
          this.fire("message", { data: JSON.stringify(e) });
          last = e.seq;
        }
        const fn = (e: AgentEvent) => {
          if (e.seq > last) {
            last = e.seq;
            this.fire("message", { data: JSON.stringify(e) });
          }
        };
        hub.add(fn);
        this.off = () => hub.delete(fn);
      });
    }
    addEventListener(type: string, fn: (ev: SocketEvent) => void) {
      (this.handlers[type] ??= []).push(fn);
    }
    removeEventListener(type: string, fn: (ev: SocketEvent) => void) {
      this.handlers[type] = (this.handlers[type] ?? []).filter((f) => f !== fn);
    }
    send() {}
    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      this.off?.();
      this.fire("close", { code: 1000 });
    }
    private fire(type: string, ev: SocketEvent) {
      for (const fn of this.handlers[type] ?? []) fn(ev);
    }
  }

  // ----- fake activity -----
  const TOOLS = ["Read", "Edit", "Bash", "Grep", "Write"];
  const FILES = ["crates/terrariumd/src/runner.rs", "crates/terrarium-core/src/event.rs", "dashboard/src/api/client.ts", "Cargo.toml"];
  let tick = 0;
  const activity = () => {
    tick++;
    const working = w.agents.filter((a) => a.status === "working");
    const a = working[tick % Math.max(working.length, 1)];
    if (a && tick % 5 !== 0) {
      const tool = TOOLS[tick % TOOLS.length]!;
      const target = tool === "Bash" ? "cargo test -p terrariumd" : FILES[tick % FILES.length]!;
      agentEvent(a, "tool_use", `${tool} ${target}`, { tool, input: target, tool_id: `toolu_mock${tick}` });
    }
    if (tick % 5 === 0) {
      const lead = w.agents.find((x) => x.name === "lead");
      if (lead?.status === "idle") {
        agentEvent(lead, "turn_start", "Turn started: triage inbox");
        setStatus(lead, "working");
      } else if (lead?.status === "working") {
        agentEvent(lead, "turn_end", "Turn ended after 12s");
        setStatus(lead, "idle");
      }
    }
    if (tick % 7 === 0) {
      const reviewer = w.agents.find((x) => x.name === "reviewer");
      if (reviewer?.status === "starting") setStatus(reviewer, "idle");
    }
    if (tick % 3 === 0) {
      for (const s of w.subscriptions) for (const win of s.windows) setWindow(s.name, win.window, Math.min(100, win.used_pct + 1));
    }
    // A plan at 100% resets after a while, so the demo shows `quota.resumed`.
    for (const [name, at] of exhaustedAt) if (tick - at >= 6) resetPlan(name);
    if (tick % 2 === 0) usageTick();
  };
  let timer: ReturnType<typeof setInterval> | undefined;
  const ensureActivity = () => {
    const ms = opts.eventsIntervalMs ?? 3000;
    if (!timer && ms > 0) timer = setInterval(activity, ms);
  };

  // ----- board (docs/api.md §8) -----
  const blockedBy = (c: Card) => c.depends_on.filter((d) => {
    const dep = w.cards.find((x) => x.id === d);
    return dep && dep.column !== "done" && !dep.archived;
  });
  const view = (c: Card): Card => ({ ...c, blocked_by: blockedBy(c) });
  const activeIn = (proj: string, column: string) => w.cards.filter((c) => c.project === proj && c.column === column && !c.archived);
  const boardOf = (name: string) => {
    const p = project(name);
    const cols = w.columns[name] ?? [];
    const order = new Map(cols.map((c, i) => [c.name, i]));
    return {
      project: name,
      card_prefix: p.card_prefix,
      columns: cols.map((c, i) => ({ ...c, position: i, count: activeIn(name, c.name).length })),
      cards: w.cards
        .filter((c) => c.project === name && !c.archived)
        .sort((a, b) => (order.get(a.column) ?? 99) - (order.get(b.column) ?? 99) || a.position - b.position)
        .map(view),
    };
  };
  const reindex = (proj: string, column: string) =>
    activeIn(proj, column)
      .sort((a, b) => a.position - b.position)
      .forEach((c, i) => (c.position = i));
  const cardEvent = (c: Card, change: string, summary: string, data: Record<string, unknown> = {}) => {
    const as = c.assignee ? w.agents.find((a) => a.id === c.assignee) : undefined;
    emit({ kind: "card", actor: "operator", ...(as ? { agent: as.name, agent_id: as.id } : {}), project: c.project, card: c.id, summary, data: { card_id: c.id, change, title: c.title, column: c.column, ...data } });
  };
  const comment = (c: Card, body: string) => {
    const cm: CardComment = { id: ulid("cmt"), author: "operator", author_name: "operator", body, created_at: iso() };
    c.comments.push(cm);
    return cm;
  };
  /** Move with the rules of docs/api.md §8.2. */
  const move = (c: Card, column: string, position: number | undefined, note?: string) => {
    if (c.archived) fail(409, "conflict", `${c.id} is archived`);
    const cols = w.columns[c.project] ?? [];
    const col = cols.find((x) => x.name === column) ?? fail(422, "invalid", `no column \`${column}\` on the board of \`${c.project}\``);
    const from = c.column;
    if (column !== from) {
      if (col.wip_limit !== undefined && activeIn(c.project, column).length >= col.wip_limit)
        fail(409, "wip_limit", `column \`${column}\` is at its WIP limit (${col.wip_limit})`, { column, wip_limit: col.wip_limit });
      if (column === "in_progress") {
        const unmet = blockedBy(c);
        if (unmet.length) fail(409, "dependencies_unmet", `${c.id} depends on ${unmet.join(", ")} that are not done`, { blocked_by: unmet });
      }
      if (column === "blocked" && !note?.trim()) fail(422, "comment_required", "a move to `blocked` needs a comment");
    }
    if (note?.trim()) comment(c, note.trim());
    c.column = column;
    const target = position ?? Number.MAX_SAFE_INTEGER;
    activeIn(c.project, column)
      .filter((x) => x !== c && x.position >= target)
      .forEach((x) => x.position++);
    c.position = target === Number.MAX_SAFE_INTEGER ? 1e9 : target;
    reindex(c.project, column);
    if (from !== column) reindex(c.project, from);
    c.updated_at = iso();
    if (from !== column) {
      cardEvent(c, "moved", `operator moved ${c.id} to ${column}`, { from, to: column, comment: note ?? null });
      if (column === "blocked") {
        const thread = ulid("th");
        sendMail(`project:${c.project}`, "operator", `${c.id} blocked: ${c.title}`, `${c.id} moved to blocked.\n\n${note ?? ""}`, thread);
      }
      if (column === "done") {
        for (const d of w.cards.filter((x) => x.depends_on.includes(c.id) && blockedBy(x).length === 0)) cardEvent(d, "unblocked", `${d.id} unblocked by ${c.id}`, { by: c.id });
      }
    } else cardEvent(c, "reordered", `operator reordered ${c.id}`);
  };
  const assign = (c: Card, assignee: string | null) => {
    const previous = c.assignee ?? null;
    if (assignee === null || assignee === "") {
      delete c.assignee;
      delete c.assignee_name;
    } else if (assignee === "operator" || assignee === "me") {
      c.assignee = "operator";
      c.assignee_name = "operator";
    } else {
      const local = w.agents.find((a) => a.project === c.project && (a.name === assignee || a.id === assignee || `${a.project}/${a.name}` === assignee));
      const a = local ?? fail(422, "invalid", `\`${assignee}\` is not an agent of project \`${c.project}\``);
      c.assignee = a.id;
      c.assignee_name = `${a.project}/${a.name}`;
    }
    c.updated_at = iso();
    cardEvent(c, "assigned", `operator assigned ${c.id} to ${c.assignee_name ?? "nobody"}`, { assignee: c.assignee ?? null, previous });
  };

  // ----- mail (docs/api.md §7) -----
  const sendMail = (to: string, from: string, subject: string, body: string, thread?: string, inReplyTo?: string): Message => {
    const t = address(to);
    const f = address(from);
    const m: Message = {
      id: ulid("msg"),
      thread_id: thread ?? ulid("th"),
      from: f.addr,
      from_kind: f.kind,
      from_name: f.label,
      to: t.addr,
      to_kind: t.kind,
      to_name: t.label,
      subject,
      body,
      state: t.kind === "agent" ? "queued" : "delivered",
      ...(inReplyTo ? { in_reply_to: inReplyTo } : {}),
      created_at: iso(),
      ...(t.kind === "agent" ? {} : { delivered_at: iso() }),
    };
    w.mail.push(m);
    const recipient = t.kind === "agent" ? w.agents.find((a) => a.id === t.addr) : undefined;
    emit({
      kind: "mail",
      actor: "operator",
      ...(recipient ? { agent: recipient.name, agent_id: recipient.id } : {}),
      project: t.project,
      summary: `Mail from ${m.from_name} to ${m.to_name}: ${subject}`,
      data: { message_id: m.id, thread_id: m.thread_id, from: m.from, from_name: m.from_name, to: m.to, to_name: m.to_name, subject },
    });
    return m;
  };
  const replyTo = (id: string, body: string, subject: string | undefined, as?: string): Message[] => {
    const orig = find(w.mail, (m) => m.id === id, `message ${id}`);
    if (!body.trim()) fail(422, "invalid", "the body is empty");
    // The operator answers for `operator` and every project inbox.
    const from = as ?? (orig.to_kind === "agent" ? "operator" : orig.to);
    const participants = [...new Set(w.mail.filter((m) => m.thread_id === orig.thread_id).flatMap((m) => [m.from, m.to]))].filter((p) => p !== from);
    const first = w.mail.find((m) => m.thread_id === orig.thread_id);
    const subj = subject ?? (orig.subject || first?.subject ? `Re: ${(orig.subject || first!.subject).replace(/^Re:\s*/i, "")}` : "");
    const out = participants.map((p) => sendMail(p, from, subj, body, orig.thread_id, orig.id));
    orig.state = "replied";
    return out;
  };
  const listMail = (pred: (m: Message) => boolean, q: { state?: string; thread?: string; unread?: boolean } & PageQuery): Page<Message> =>
    paginate(
      w.mail.filter(
        (m) =>
          pred(m) &&
          (!q.thread || m.thread_id === q.thread) &&
          (!q.unread || m.state === "queued" || m.state === "delivered") &&
          (!q.state || (q.state === "unread" ? m.state === "queued" || m.state === "delivered" : m.state === q.state)),
      ),
      q,
      (m) => m.id,
    );
  const markRead = (id: string) => {
    const m = find(w.mail, (x) => x.id === id, `message ${id}`);
    if (m.state === "queued" || m.state === "delivered") {
      m.state = "read";
      m.read_at = iso();
    }
    return m;
  };

  // ----- artifact upload -----
  const addArtifact = (proj: string, name: string, content_type: string, size: number, kind: Artifact["kind"], attach?: ArtifactAttach): Artifact => {
    project(proj);
    const n = Math.max(0, ...w.artifacts.filter((x) => x.project === proj).map((x) => Number(x.id.split("-")[1]) || 0)) + 1;
    const links: Artifact["links"] = attach ? [{ kind: attach.kind, ref: attach.ref, created_by: "operator", created_at: iso() }] : [];
    const a: Artifact = {
      id: `ART-${n}`, key: `${proj}/ART-${n}`, uid: ulid("art"), project: proj, name, content_type, size, sha256: "0".repeat(64), kind,
      renders: kind === "bundle" || /^(image|text)\//.test(content_type) || content_type === "application/pdf",
      created_by: "operator", created_by_name: "operator", created_at: iso(), links, embedded_in: [],
    };
    w.artifacts.push(a);
    emit({ kind: "artifact.created", actor: "operator", project: proj, summary: `operator uploaded ${a.id} ${name}`, data: { artifact_id: a.id, key: a.key, uid: a.uid, name, kind, size, content_type } });
    return a;
  };

  // ----- subscriptions and the quota policy (docs/api.md §16) -----
  const PROBES = ["anthropic-oauth-usage", "openai-codex-usage", "codex-rate-limits", "github-copilot-quota", "copilot-premium-requests"];
  const KEY_ENV: Record<string, string> = {
    anthropic: "ANTHROPIC_API_KEY",
    openai: "OPENAI_API_KEY",
    "openai-compatible": "OPENAI_API_KEY",
    google: "GEMINI_API_KEY",
    gemini: "GEMINI_API_KEY",
    openrouter: "OPENROUTER_API_KEY",
    xai: "XAI_API_KEY",
    groq: "GROQ_API_KEY",
    github: "COPILOT_GITHUB_TOKEN",
  };
  /** The subscription as the API shows it (docs/api.md §16.1). */
  const subView = (s: Subscription): Subscription => ({
    ...s,
    local_estimate_only: !s.probe,
    has_api_key: !!s.env_var && !!w.apiKeys[s.name],
    held_agents: w.holds.filter((h) => h.cause === `quota:${s.name}`).length,
  });
  const checkSubName = (name: string) => {
    if (!NAME_RE.test(name)) fail(422, "invalid", `\`${name}\` is not a valid name: lowercase letters, digits, - and _`);
    if (name === "login" || name === "detect") fail(422, "invalid", `\`${name}\` is a reserved name`);
  };
  const alerted = new Set<string>();
  const exhaustedAt = new Map<string, number>();
  const latestReset = (s: Subscription) => s.windows.map((x) => x.resets_at ?? "").sort().at(-1) || undefined;
  /** The quota policy (docs/api.md §16.6) after a new reading of one window. */
  const policy = (s: Subscription, win: Subscription["windows"][number]) => {
    const period = `${s.name}/${win.window}/${win.resets_at ?? ""}`;
    const alert = (level: "alert" | "exhausted") => {
      if (alerted.has(`${period}/${level}`)) return;
      alerted.add(`${period}/${level}`);
      const subject = level === "alert" ? `${s.name}: window ${win.window} at ${win.used_pct}% (alert at ${s.alert_at}%)` : `${s.name}: window ${win.window} is used up`;
      sendMail("operator", "operator", subject, `${subject}. Resets at ${win.resets_at ?? "an unknown time"}.`);
      emit({ kind: "quota.alert", actor: "system", summary: subject, data: { subscription: s.name, window: win.window, used_pct: win.used_pct, resets_at: win.resets_at ?? null, level, alert_at: s.alert_at } });
    };
    if (win.used_pct >= s.alert_at) alert("alert");
    if (win.used_pct >= 100) {
      alert("exhausted");
      exhaustedAt.set(s.name, tick);
      const cause = `quota:${s.name}`;
      const until = latestReset(s);
      for (const a of w.agents.filter((x) => x.subscription === s.name)) {
        if (w.holds.some((h) => h.agent === a.id && h.cause === cause)) continue;
        const reason = `${s.name} window ${win.window} is at 100%`;
        w.holds.push({ agent: a.id, cause, until, reason, enforced: false, created_at: iso() });
        agentEvent(a, "quota.blocked", `${a.name}: turns held until ${until ?? "further notice"} (${reason})`, { cause, until: until ?? null, reason, enforced: false });
      }
    } else if (s.windows.every((x) => x.used_pct < 100)) {
      release(s);
    }
  };
  const release = (s: Subscription) => {
    const cause = `quota:${s.name}`;
    exhaustedAt.delete(s.name);
    for (const h of w.holds.filter((x) => x.cause === cause)) {
      w.holds.splice(w.holds.indexOf(h), 1);
      const a = w.agents.find((x) => x.id === h.agent);
      const left = w.holds.filter((x) => x.agent === h.agent).length;
      if (a) agentEvent(a, "quota.resumed", `${a.name}: hold \`${cause}\` ended`, { cause, holds_left: left });
    }
  };
  /** A probe reading: store the samples, emit `quota.sample`, and run the policy. */
  const reading = (s: Subscription) => {
    const at = iso();
    for (const win of s.windows) {
      win.at = at;
      w.quota.push({ subscription: s.name, window: win.window, used_pct: win.used_pct, resets_at: win.resets_at, raw: { utilization: win.used_pct, resets_at: win.resets_at }, at });
    }
    s.probe_status = "ok";
    delete s.probe_error;
    s.last_probe_at = at;
    s.last_success_at = at;
    emit({ kind: "quota.sample", actor: "system", summary: `${s.name}: ${s.windows.map((x) => `${x.window} ${x.used_pct}%`).join(", ")}`, data: { subscription: s.name, windows: s.windows.map((x) => ({ window: x.window, used_pct: x.used_pct, resets_at: x.resets_at ?? null })) } });
    for (const win of s.windows) policy(s, win);
  };
  const setWindow = (name: string, window: string, usedPct: number) => {
    const s = sub(name);
    const win = s.windows.find((x) => x.window === window);
    if (!win) s.windows.push({ window, used_pct: usedPct, resets_at: new Date(now() + 5 * 3600_000).toISOString(), at: iso() });
    else win.used_pct = usedPct;
    reading(s);
  };
  const resetPlan = (name: string) => {
    const s = sub(name);
    for (const win of s.windows.filter((x) => x.used_pct >= 100)) {
      win.used_pct = 4;
      win.resets_at = new Date(now() + 5 * 3600_000).toISOString();
    }
    reading(s);
  };
  /** Fake local accounting while agents work: one `usage.sample` per working agent. */
  const usageTick = () => {
    for (const a of w.agents.filter((x) => x.status === "working" && x.subscription)) {
      const card = currentCard(a.id);
      const row: UsageRow = { at: iso(), agent: a.id, project: a.project, subscription: a.subscription!, model: a.model ?? "claude-opus-5", input_tokens: 800 + (tick % 5) * 120, output_tokens: 140 + (tick % 3) * 40, cache_read: 21000, cache_write: 900, messages: 2, turn: w.seq, ...(card ? { card } : {}) };
      w.usage.push(row);
      agentEvent(a, "usage.sample", `${a.name}: ${row.input_tokens + row.output_tokens} tokens on ${row.subscription}`, {
        subscription: row.subscription, harness: a.harness, model: row.model, input_tokens: row.input_tokens, output_tokens: row.output_tokens, cache_read: row.cache_read, cache_write: row.cache_write, messages: row.messages,
      });
    }
  };

  /** The logins in the harness homes (what the status commands would find). */
  const homeLogins = () => [
    ...w.subscriptions.filter((s) => s.login_status === "logged_in" || s.login_status === "expiring").map((s) => ({ provider: s.provider, harness: s.harnesses[0] ?? "pi" })),
    { provider: "openai-codex", harness: "pi" as const },
  ];
  const detected = () =>
    homeLogins()
      .filter((l) => !w.subscriptions.some((s) => s.provider === l.provider && s.harnesses.includes(l.harness)))
      .map((l) => ({
        kind: l.harness,
        harness: l.harness,
        provider: l.provider,
        ...(l.harness === "pi" ? { pi_provider: l.provider } : {}),
        logged_in: true,
        detail: l.harness === "pi" ? `pi auth check: ${l.provider} ready` : `${l.harness}: logged in`,
        suggested: { name: `pi-${l.provider}`, provider: l.provider, harnesses: [l.harness], alert_at: 80 },
      }));

  // ----- local accounting (docs/api.md §16.3 `GET /api/usage`) -----
  /** The attribution rule of docs/api.md §16.1: the agent's card in `in_progress`, newest `updated_at` first. */
  const currentCard = (agentId: string): string | undefined =>
    w.cards
      .filter((c) => c.assignee === agentId && c.column === "in_progress" && !c.archived)
      .sort((x, y) => y.updated_at.localeCompare(x.updated_at) || x.id.localeCompare(y.id))[0]?.id;
  const cardTitle = (id: string) => w.cards.find((c) => c.id === id)?.title;
  const sumRows = (rows: UsageRow[]): UsageTotals => {
    const t: UsageTotals = { input_tokens: 0, output_tokens: 0, cache_read: 0, cache_write: 0, messages: 0, turns: 0 };
    const turns = new Set<string>();
    for (const r of rows) {
      t.input_tokens += r.input_tokens;
      t.output_tokens += r.output_tokens;
      t.cache_read += r.cache_read;
      t.cache_write += r.cache_write;
      t.messages += r.messages;
      turns.add(`${r.agent}/${r.turn}`);
    }
    t.turns = turns.size;
    return t;
  };
  const checkTime = (what: string, v?: string) => {
    if (v && Number.isNaN(Date.parse(v))) fail(400, "bad_request", `\`${what}\` is not an RFC 3339 time`);
    return v ? new Date(v).toISOString() : undefined;
  };
  const usageSeries = (q: UsageQuery): UsagePoint[] => {
    const since = checkTime("since", q.since) ?? new Date(now() - 24 * 3600_000).toISOString();
    const until = checkTime("until", q.until);
    const agentId = q.agent ? agent(q.agent).id : undefined;
    const card = q.card?.trim().toUpperCase();
    const rows = w.usage.filter(
      (r) =>
        r.at >= since &&
        (!until || r.at < until) &&
        (!q.subscription || r.subscription === q.subscription) &&
        (!q.project || r.project === q.project) &&
        (!agentId || r.agent === agentId) &&
        (!q.model || r.model === q.model) &&
        (!card || r.card === card),
    );
    if (q.group && !["subscription", "project", "agent", "model", "card"].includes(q.group)) fail(400, "bad_request", `unknown group \`${q.group}\``);
    const bucketOf = (at: string) => (q.bucket === "day" ? `${at.slice(0, 10)}T00:00:00Z` : `${at.slice(0, 13)}:00:00Z`);
    const groups = new Map<string, UsageRow[]>();
    for (const r of rows) {
      const k = `${bucketOf(r.at)}\u0000${q.group ? (r[q.group] ?? "") : ""}`;
      groups.set(k, [...(groups.get(k) ?? []), r]);
    }
    return [...groups.entries()]
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([k, rs]) => {
        const [bucket, key] = k.split("\u0000") as [string, string];
        // A sample with no card has no key (the daemon's null).
        const title = q.group === "card" && key ? cardTitle(key) : undefined;
        return { bucket, ...(q.group && key ? { key } : {}), ...(title ? { title } : {}), ...sumRows(rs) };
      });
  };
  const execSessions = new Map<string, string>();
  const target = (id: string): Target => find(w.targets, (t) => t.id === id || t.name === id, `target \`${id}\``);

  // ----- target leases (docs/api.md §17.7) -----
  w.leases ??= [];
  const leaseEvent = (l: Lease, change: "granted" | "extended" | "ended", actor: string) => {
    const a = w.agents.find((x) => x.id === l.agent);
    const holder = l.agent_name ?? l.agent;
    const summary = change === "ended" ? `lease on ${l.target} ended for ${holder}` : `${holder} ${change} target ${l.target} ${l.expires_at ? `until ${l.expires_at}` : "until revoked"}`;
    emit({ kind: "lease", actor, ...(a ? { agent: a.name } : {}), agent_id: l.agent, project: l.project, summary, data: { change, target: l.target, lease_id: l.id, expires_at: l.expires_at, exclusive: l.exclusive, state: l.state } });
  };
  /** The expiry timer: end each active lease past its `expires_at`, as `system`. */
  const sweepLeases = () => {
    for (const l of w.leases) {
      if (l.state !== "active" || !l.expires_at || Date.parse(l.expires_at) > now()) continue;
      l.state = "expired";
      l.revoked_at = iso();
      l.revoked_by = "system";
      audit("target.lease_expired", l.target, l.project, { lease: l.id, agent: l.agent, agent_name: l.agent_name }, "system");
      leaseEvent(l, "ended", "system");
    }
  };
  const armExpiry = (l: Lease) => {
    if (!l.expires_at) return;
    const ms = Date.parse(l.expires_at) - now();
    if (ms < 2 ** 31 - 1) later(sweepLeases, Math.max(0, ms) + 20);
  };
  w.leases.forEach(armExpiry);
  /** `until`: an RFC 3339 time in the future, or a duration from now (`90s`, `30m`, `2h`, `1d`). */
  const parseUntil = (u: string | null | undefined): string | null => {
    const v = u?.trim();
    if (!v) return null;
    const d = /^(\d+)\s*(s|m|h|d)$/.exec(v);
    const UNIT = { s: 1000, m: 60_000, h: 3_600_000, d: 86_400_000 } as const;
    const at = d ? now() + Number(d[1]) * UNIT[d[2] as keyof typeof UNIT] : /^\d{4}-\d{2}-\d{2}T/.test(v) ? Date.parse(v) : NaN;
    if (Number.isNaN(at)) fail(422, "invalid", `bad \`until\` \`${v}\`: an RFC 3339 time or a duration such as 30m`);
    if (at <= now()) fail(422, "invalid", `\`until\` \`${v}\` is not in the future`);
    return new Date(at).toISOString();
  };
  const leasesWhere = (pred: (l: Lease) => boolean, all?: boolean) => {
    sweepLeases();
    const rows = w.leases.filter(pred).reverse();
    return all ? rows.filter((l) => l.state === "active").concat(rows.filter((l) => l.state !== "active").slice(0, 100)) : rows.filter((l) => l.state === "active");
  };
  const activeLease = (t: Target, a: Agent): Lease => {
    sweepLeases();
    return w.leases.find((l) => l.target === t.name && l.agent === a.id && l.state === "active") ?? notFound(`active lease of \`${a.project}/${a.name}\` on \`${t.name}\``);
  };
  const until = (l: Lease) => (l.expires_at ? `until ${l.expires_at}` : "until revoked");

  // ----- login sessions (docs/api.md §16.4) -----
  const logins = new Map<string, LoginSession & { name?: string }>();
  const LOGIN_URL: Record<LoginKind, string> = {
    claude: "https://claude.ai/oauth/authorize?code=true&client_id=mock&response_type=code&state=8f2c",
    codex: "https://auth.openai.com/codex/device",
    copilot: "https://github.com/login/device",
    pi: "https://auth.openai.com/oauth/authorize?client_id=mock-pi&state=77aa",
  };
  const DEVICE_CODE: Partial<Record<LoginKind, string>> = { codex: "QX7M-4KRP9", copilot: "8F3D-21AC" };
  const LOGIN_CMD: Record<LoginKind, string> = { claude: "claude auth login", codex: "codex login --device-auth", copilot: "copilot login --device-code", pi: "pi" };
  const INSTRUCTIONS = (kind: LoginKind, provider?: string) =>
    kind === "claude"
      ? "Open the URL, sign in, and paste the code that the page shows."
      : kind === "pi"
        ? `In the terminal, type /login, choose "Sign in with an account", and pick ${provider ?? "the provider"}. Then open the URL and paste the code or the final redirect URL.`
        : "Open the URL, sign in, and enter the device code.";
  const DEFAULTS: Record<LoginKind, (p?: string) => Pick<Subscription, "provider" | "plan" | "harnesses" | "probe"> & { name: string }> = {
    claude: () => ({ name: "claude-pro", provider: "anthropic", plan: "Pro", harnesses: ["claude"], probe: "anthropic-oauth-usage" }),
    codex: () => ({ name: "chatgpt-plus", provider: "openai", plan: "ChatGPT Plus", harnesses: ["codex"], probe: "openai-codex-usage" }),
    copilot: () => ({ name: "copilot", provider: "github", plan: "Copilot Pro", harnesses: ["copilot"], probe: "github-copilot-quota" }),
    pi: (p) => ({ name: `pi-${p ?? "anthropic"}`, provider: p ?? "anthropic", harnesses: ["pi"], probe: undefined }),
  };
  const live = (id: string) => {
    const s = logins.get(id);
    if (!s || s.done) fail(409, "not_running", `login session \`${id}\` is not live`);
    return s!;
  };
  const endLogin = (s: LoginSession, state: "failed" | "cancelled" | "expired" | "done", error?: string) => {
    if (s.done) return;
    s.state = state;
    s.done = true;
    s.finished_at = iso();
    if (error) s.error = error;
    audit("login.end", s.id, undefined, { kind: s.kind, state });
  };
  const toWaiting = (s: LoginSession) => {
    if (s.state !== "starting") return;
    s.state = "waiting";
    s.url = LOGIN_URL[s.kind];
    if (DEVICE_CODE[s.kind]) {
      s.device_code = DEVICE_CODE[s.kind];
      const auto = opts.autoLoginMs ?? 8000;
      if (auto > 0) setTimeout(() => finishLogin(s), auto);
    }
  };
  /** The flow exited 0: verify, make or update the subscription, probe once. */
  const finishLogin = (s: LoginSession & { name?: string }) => {
    if (s.done || s.state === "verifying") return;
    s.state = "verifying";
    later(() => {
      if (s.done) return;
      const d = DEFAULTS[s.kind](s.provider);
      const name = s.name ?? d.name;
      let row = w.subscriptions.find((x) => x.name === name);
      if (!row) {
        row = { ...d, name, alert_at: 80, source: "api", login_status: "logged_in", probe_status: "none", created_at: iso(), updated_at: iso(), windows: [], local_estimate_only: !d.probe, has_api_key: false, held_agents: 0 };
        w.subscriptions.push(row);
        audit("subscription.register", name, undefined, { kind: s.kind });
      }
      row.login_status = "logged_in";
      row.verified_at = iso();
      row.updated_at = iso();
      if (row.probe) {
        if (!row.windows.length) row.windows = [{ window: s.kind === "copilot" ? "monthly" : "5h", used_pct: 3, resets_at: new Date(now() + 3 * 3600_000).toISOString(), at: iso() }];
        reading(row);
      }
      s.subscription = name;
      endLogin(s, "done");
    }, latency > 0 ? 1200 : 0);
  };
  const fakeLoginPty = (s: LoginSession | undefined, protocol: string | null) => {
    if (!s || s.done) return new FakePty({ refuse: true, protocol });
    const k = s.kind;
    const banner = [`\x1b[2m$ ${LOGIN_CMD[k]}\x1b[0m  \x1b[2m(mock login terminal)\x1b[0m`];
    if (k === "codex") banner.push("", "Welcome to Codex [v0.141.0]", "", "Follow these steps to sign in with ChatGPT using device code authorization:", "", `1. Open this link in your browser and sign in to your account`, `   \x1b[94m${LOGIN_URL.codex}\x1b[0m`, "", `2. Enter this one-time code \x1b[90m(expires in 15 minutes)\x1b[0m`, `   \x1b[94m${DEVICE_CODE.codex}\x1b[0m`, "");
    if (k === "copilot") banner.push("", `! First copy your one-time code: \x1b[1m${DEVICE_CODE.copilot}\x1b[0m`, `Then open: ${LOGIN_URL.copilot}`, "Waiting for authorization...");
    if (k === "claude") banner.push("", "Browser didn't open? Use the url below to sign in:", "", LOGIN_URL.claude, "", "Paste code here if prompted >");
    if (k === "pi") banner.push("", "pi (mock). Type /login to sign in to a provider.");
    return new FakePty({
      protocol,
      prompt: k === "pi" ? "> " : "",
      banner,
      onLine: (line) => {
        if (s.done || !line.trim()) return [];
        if (k === "pi" && line.trim() === "/login") {
          toWaiting(s);
          return [`Sign in with an account: ${s.provider ?? "anthropic"}`, `Open: ${LOGIN_URL.pi}`, "Paste the code or the final redirect URL:"];
        }
        finishLogin(s);
        return ["\x1b[32m✓ Login successful.\x1b[0m"];
      },
    });
  };

  // ----- memory (docs/api.md §10) -----
  const memNo = (id: string) => Number(id.split("-")[1]) || 0;
  const memProject = (id: string) => w.memories.find((m) => m.id === id)?.project;
  const memKey = (proj: string, id: string) => `${proj}/${id}`;
  /** `MEM-n` (unique across projects for the operator) or `project/MEM-n`. */
  const memory = (key: string, inProject?: string): Memory => {
    if (key.includes("/")) return find(w.memories, (m) => m.key === key, `memory \`${key}\``);
    const hits = w.memories.filter((m) => m.id === key && (!inProject || m.project === inProject));
    if (hits.length > 1) fail(400, "bad_request", `memory \`${key}\` is in more than one project; use project/${key}`);
    return hits[0] ?? notFound(`memory \`${key}\``);
  };
  const FIRST_REL = ["contradicts", "supersedes"];
  /** A memory with its `links` in both directions, contradicts first, then supersedes. */
  const memView = (m: Memory): Memory => {
    const title = (id: string) => w.memories.find((x) => x.project === m.project && x.id === id);
    const edges = w.links
      .filter((l) => memProject(l.from) === m.project && (l.from === m.id || l.to === m.id))
      .map((l) => {
        const other = l.from === m.id ? l.to : l.from;
        return { ...l, direction: l.from === m.id ? ("out" as const) : ("in" as const), other, other_title: title(other)?.title ?? other, other_superseded: !!title(other)?.superseded_by };
      })
      .sort((a, b) => Number(FIRST_REL.includes(b.relation)) - Number(FIRST_REL.includes(a.relation)) || (FIRST_REL.indexOf(a.relation) - FIRST_REL.indexOf(b.relation)) || a.created_at.localeCompare(b.created_at));
    return { ...m, links: edges };
  };
  const supersede = (old: Memory, next: Memory) => {
    if (old.superseded_by) fail(409, "conflict", `${old.id} is superseded by ${old.superseded_by} already`);
    if (next.superseded_by === old.id) fail(422, "invalid", `${next.id} is superseded by ${old.id}; it cannot supersede it back`);
    old.superseded_by = next.id;
    next.supersedes = [...new Set([...next.supersedes, old.id])];
    w.links.push({ from: next.id, to: old.id, relation: "supersedes", author: "operator", author_name: "operator", created_at: iso() });
    emit({ kind: "memory.superseded", actor: "operator", project: old.project, summary: `${next.id} supersedes ${old.id}`, data: { memory_id: old.id, key: old.key, title: old.title, by: next.id } });
  };

  // ----- wiki (docs/api.md §11) -----
  const page = (key: string): WikiPage => {
    const byId = w.pages.find((p) => p.id === key);
    if (byId) return byId;
    const [proj, ...rest] = key.split("/");
    if (proj && rest.length && w.projects.some((p) => p.name === proj)) {
      if (rest.join("/") === "index") ensureIndex(proj);
      return find(w.pages, (p) => p.project === proj && p.path === rest.join("/") && !p.deleted_at, `page \`${key}\``);
    }
    return find(w.pages, (p) => p.path === key && !p.deleted_at, `page \`${key}\``);
  };
  const ensureIndex = (proj: string) => {
    if (w.pages.some((p) => p.project === proj && p.path === "index" && !p.deleted_at)) return;
    const body = `# ${proj}\n\nThe wiki of project \`${proj}\`. Keep this page as the table of contents.\n`;
    const pg: WikiPage = { id: ulid("pg"), project: proj, path: "index", title: proj, rev: 1, body, author: "system", author_name: "system", summary: "Create the index page", created_by: "system", created_at: iso(), updated_at: iso(), links: [] };
    w.pages.push(pg);
    w.revisions[pg.id] = [{ page: pg.id, rev: 1, author: "system", author_name: "system", summary: pg.summary, created_at: iso(), body }];
  };
  /** Title, status, and reviewer come from the body (docs/api.md §11.1). */
  const applyBody = (pg: WikiPage, body: string, title?: string) => {
    const { meta, body: rest } = splitFrontMatter(body);
    pg.body = body;
    pg.title = title || meta.title || /^#\s+(.+)$/m.exec(rest)?.[1]?.trim() || pg.path.split("/").pop()!;
    const st = meta.status;
    if (st && st !== "draft" && st !== "ready_for_review" && st !== "reviewed") fail(422, "invalid", `\`${st}\` is not a page status`);
    if (st) pg.status = st as WikiPage["status"];
    else delete pg.status;
    if (meta.reviewer) pg.reviewer = meta.reviewer;
    else delete pg.reviewer;
    pg.links = [...body.matchAll(/\[\[([^\]|]+)(?:\|[^\]]*)?\]\]/g)].map((m) => {
      const t = m[1]!;
      if (/^[A-Z][A-Z0-9]*-\d+$/.test(t)) return t.startsWith("MEM-") ? { kind: "memory", target: `${pg.project}/${t}` } : { kind: "card", target: t };
      if (/^v\d/.test(t)) return { kind: "version", target: `${pg.project}/${t}` };
      if (t.startsWith("project:")) return { kind: "page", target: t.slice(8) };
      return { kind: "page", target: `${pg.project}/${t}` };
    });
    for (const m of body.matchAll(/artifact[:\s]+(ART-\d+)/g)) pg.links.push({ kind: "artifact", target: `${pg.project}/${m[1]}` });
  };
  const newRevision = (pg: WikiPage, body: string, summary: string, title?: string) => {
    applyBody(pg, body, title);
    pg.rev++;
    pg.summary = summary;
    pg.author = "operator";
    pg.author_name = "operator";
    pg.updated_at = iso();
    (w.revisions[pg.id] ??= []).push({ page: pg.id, rev: pg.rev, author: "operator", author_name: "operator", summary, created_at: iso(), body });
  };
  const summaryOf = (p: WikiPage, needle?: string): WikiPageSummary => {
    const { body: _b, links: _l, summary: _s, created_by: _c, created_at: _ca, ...rest } = p;
    const i = needle ? p.body.toLowerCase().indexOf(needle) : -1;
    return i >= 0 ? { ...rest, snippet: p.body.slice(Math.max(0, i - 30), i + 60) } : rest;
  };
  const wikiEvent = (kind: AgentEvent["kind"], pg: WikiPage, summary: string, extra: Record<string, unknown> = {}) =>
    emit({ kind, actor: "operator", project: pg.project, summary, data: { page_id: pg.id, path: pg.path, title: pg.title, rev: pg.rev, ...extra } });

  // ----- artifacts and versions (docs/api.md §12, §13) -----
  const artNo = (id: string) => Number(id.split("-")[1]) || 0;
  const artifact = (key: string): Artifact => {
    const hit = w.artifacts.find((a) => a.uid === key || a.key === key);
    if (hit) return hit;
    const ids = w.artifacts.filter((a) => a.id === key);
    if (ids.length > 1) fail(400, "bad_request", `artifact \`${key}\` is in more than one project; use project/${key}`);
    return ids[0] ?? notFound(`artifact \`${key}\``);
  };
  const artView = (a: Artifact): Artifact => ({
    ...a,
    embedded_in: w.pages.filter((p) => p.project === a.project && !p.deleted_at && new RegExp(`artifact[:\\s]+${a.id}\\b`).test(p.body)).map((p) => `${p.project}/${p.path}`),
  });
  const cardRef = (id: string) => {
    const c = cardById(id);
    return { id: c.id, title: c.title, column: c.column };
  };
  const artRef = (id: string) => {
    const a = artifact(id);
    return { id: a.id, uid: a.uid, name: a.name, kind: a.kind, content_type: a.content_type, size: a.size };
  };

  const derivePrefix = (name: string) => {
    let letters = name.replace(/[^a-z0-9]/gi, "").toUpperCase();
    if (!/^[A-Z]/.test(letters)) letters = `P${letters}`;
    letters = letters.slice(0, 10);
    const taken = (p: string) => w.projects.some((x) => x.card_prefix === p);
    for (let len = 3; len <= Math.max(3, letters.length); len++) {
      const c = letters.slice(0, len);
      if (!taken(c)) return c;
    }
    for (let n = 2; ; n++) if (!taken(`${letters.slice(0, 3)}${n}`)) return `${letters.slice(0, 3)}${n}`;
  };

  // ----- sessions (docs/api.md §1.1) -----
  type MockCookie = { csrf: string; exp: number };
  const jar = opts.cookieJar ?? null;
  let cookie: MockCookie | null = (() => {
    try {
      const raw = jar?.getItem(MOCK_COOKIE_KEY);
      return raw ? (JSON.parse(raw) as MockCookie) : null;
    } catch {
      return null;
    }
  })();
  const saveCookie = () => {
    try {
      if (cookie) jar?.setItem(MOCK_COOKIE_KEY, JSON.stringify(cookie));
      else jar?.removeItem(MOCK_COOKIE_KEY);
    } catch {
      // ignore
    }
  };
  /** The cookie the browser still sends: it drops the cookie at its end (`Max-Age`). */
  const liveCookie = (): MockCookie | null => {
    if (cookie && cookie.exp <= now()) {
      cookie = null;
      saveCookie();
    }
    return cookie;
  };
  const unauthorized = (message: string): never => {
    opts.client?.onUnauthorized?.();
    throw new AuthError({ error: { code: "unauthorized", message } });
  };
  const CSRF_REFUSED = "a request with the session cookie must send X-CSRF-Token with the csrf value of POST /api/session";
  /** The TCP listener's check: the bearer token first, then the cookie (with CSRF on a mutating call). */
  const authorize = (verb: string) => {
    const c = opts.client;
    if (!c) return;
    const bearer = c.token();
    if (bearer !== null) {
      if (bearer !== MOCK_TOKEN) unauthorized("missing or wrong bearer token");
      return;
    }
    const ck = liveCookie();
    if (!ck) unauthorized("missing or wrong bearer token");
    if (isMutating(verb) && c.csrf() !== ck!.csrf) fail(403, "forbidden", CSRF_REFUSED);
  };
  /** What the mock "browser" offers on an upgrade. */
  const wsAuth = (): MockSocketAuth | undefined =>
    opts.client ? { token: opts.client.token(), cookie: liveCookie() !== null } : undefined;
  const sock: typeof mockSocket = (path, make) => mockSocket(path, make, wsAuth());

  const api: Api & { world: MockWorld } = {
    world: w,
    session: {
      create: (token) =>
        reply(() => {
          // The daemon answers 404; the HTTP client turns that into null.
          if (opts.sessions === false) return null;
          if (token !== MOCK_TOKEN) throw new AuthError({ error: { code: "unauthorized", message: "missing or wrong bearer token" } });
          const exp = Math.floor((now() + (opts.sessionTtlMs ?? 12 * 3600_000)) / 1000);
          cookie = { csrf: `csrf_${ulid("csrf").toLowerCase()}`, exp: exp * 1000 };
          saveCookie();
          return { csrf: cookie.csrf, exp, expires_at: new Date(exp * 1000).toISOString().replace(/\.\d{3}Z$/, "Z") };
        }),
      remove: () =>
        reply(() => {
          if (opts.sessions === false) return;
          const ck = liveCookie();
          if (ck && opts.client && opts.client.csrf() !== ck.csrf) fail(403, "forbidden", CSRF_REFUSED);
          cookie = null;
          saveCookie();
        }),
    },
    health: () =>
      reply(() => ({
        status: "ok",
        version: "0.1.0-mock",
        uptime: Math.round((now() - startedAt) / 1000) + 3600,
        started_at: new Date(startedAt - 3600_000).toISOString(),
        definitions: { projects: 1, agents: 1, targets: 0, subscriptions: 0 },
      })),

    projects: {
      list: () => reply(() => w.projects.slice().sort((a, b) => a.name.localeCompare(b.name))),
      get: (name) => reply(() => project(name)),
      create: (p) =>
        reply(() => {
          if (!NAME_RE.test(p.name)) fail(422, "invalid", `\`${p.name}\` is not a valid name: 1 to 63 of a-z, 0-9, - and _, starting with a letter or a digit`);
          if (w.projects.some((x) => x.name === p.name)) fail(409, "conflict", `project \`${p.name}\` exists`);
          if (p.card_prefix !== undefined && !PREFIX_RE.test(p.card_prefix)) fail(422, "invalid", `\`${p.card_prefix}\` is not a valid card prefix`);
          if (p.card_prefix && w.projects.some((x) => x.card_prefix === p.card_prefix)) fail(409, "conflict", `card prefix \`${p.card_prefix}\` is taken`);
          const proj: Project = { name: p.name, description: p.description ?? "", card_prefix: p.card_prefix || derivePrefix(p.name), source: "api", created_at: iso(), updated_at: iso() };
          w.projects.push(proj);
          w.columns[p.name] = [
            { name: "backlog", position: 0 },
            { name: "ready", position: 1 },
            { name: "in_progress", position: 2 },
            { name: "review", position: 3 },
            { name: "blocked", position: 4 },
            { name: "done", position: 5 },
          ];
          audit("project.create", proj.name, proj.name, { card_prefix: proj.card_prefix, source: "api" });
          return proj;
        }),
      update: (name, patch) =>
        reply(() => {
          const p = project(name);
          if (p.source === "file") fail(409, "file_managed", `project \`${name}\` is declared in a definition file; edit the file, then POST /api/reload`);
          if (patch.card_prefix !== undefined && patch.card_prefix !== p.card_prefix) {
            if (!PREFIX_RE.test(patch.card_prefix)) fail(422, "invalid", `\`${patch.card_prefix}\` is not a valid card prefix`);
            if (w.cards.some((c) => c.project === name)) fail(409, "conflict", "the card prefix cannot change after the first card");
            if (w.projects.some((x) => x.card_prefix === patch.card_prefix)) fail(409, "conflict", `card prefix \`${patch.card_prefix}\` is taken`);
            p.card_prefix = patch.card_prefix;
          }
          if (patch.description !== undefined) p.description = patch.description;
          p.updated_at = iso();
          audit("project.update", name, name, { ...patch });
          return p;
        }),
      remove: (name) =>
        reply(() => {
          const p = project(name);
          if (p.source === "file") fail(409, "file_managed", `project \`${name}\` is declared in a definition file`);
          if (w.agents.some((a) => a.project === name)) fail(409, "conflict", `project \`${name}\` has agents; delete them first`);
          w.projects = w.projects.filter((x) => x.name !== name);
          w.repos = w.repos.filter((r) => r.project !== name);
          w.cards = w.cards.filter((c) => c.project !== name);
          delete w.columns[name];
          audit("project.delete", name, name, {});
        }),
      agents: (name) => reply(() => (project(name), w.agents.filter((a) => a.project === name).sort((a, b) => a.id.localeCompare(b.id)))),
      targets: (name) => reply(() => (project(name), w.targets.filter((t) => !t.project || t.project === name).map(targetView))),
      repos: (name) => reply(() => (project(name), w.repos.filter((r) => r.project === name).sort((a, b) => a.name.localeCompare(b.name)))),
      attachRepo: (name, r) =>
        reply(() => {
          project(name);
          if (!r.url?.trim()) fail(422, "invalid", "a repo needs a url");
          const repoName = r.name || r.url.replace(/\/+$/, "").split(/[/:]/).pop()!.replace(/\.git$/, "");
          if (w.repos.some((x) => x.project === name && x.name === repoName)) fail(409, "conflict", `repo \`${repoName}\` exists in \`${name}\``);
          const repo: Repo = { id: ulid("repo"), project: name, name: repoName, url: r.url, ...(r.default_branch ? { default_branch: r.default_branch } : {}), ...(r.credential ? { credential: r.credential } : {}), path: r.path || repoName, source: "api", created_at: iso() };
          w.repos.push(repo);
          audit("repo.attach", repo.id, name, { name: repoName, url: r.url });
          return repo;
        }),
    },

    // Not in the daemon yet (docs/api.md §10): the same 404.
    repos: { checkout: () => reply(noRoute) },

    agents: {
      list: (proj) => reply(() => w.agents.filter((a) => !proj || a.project === proj).sort((a, b) => a.id.localeCompare(b.id))),
      get: (id) => reply(() => agent(id)),
      create: (def) =>
        reply(() => {
          project(def.project);
          if (!NAME_RE.test(def.name)) fail(422, "invalid", `\`${def.name}\` is not a valid name`);
          if (w.agents.some((a) => a.project === def.project && a.name === def.name)) fail(409, "conflict", `agent \`${def.project}/${def.name}\` exists`);
          if (def.policy?.mail_to.some((m) => !m.trim())) fail(422, "invalid", "policy.mail_to has an empty entry");
          for (const x of def.policy?.cross_project ?? []) if (!/^[^/]+\/[^/]+$/.test(x)) fail(422, "invalid", `policy.cross_project entry \`${x}\` is not project/agent`);
          const id = ulid("agt");
          const a: Agent = {
            id,
            name: def.name,
            project: def.project,
            harness: def.harness,
            ...(def.subscription ? { subscription: def.subscription } : {}),
            ...(def.model ? { model: def.model } : {}),
            ...(def.prompt ? { prompt: def.prompt } : {}),
            workspace: `/state/agents/${id}/workspace`,
            roles: (def.roles ?? []).filter((r): r is Role => r !== "member"),
            // Like the daemon: `cross_project` is left out when empty.
            policy: { targets: def.policy?.targets ?? [], mail_to: def.policy?.mail_to ?? ["*"], ...(def.policy?.cross_project?.length ? { cross_project: def.policy.cross_project } : {}) },
            restart: def.restart ?? "on-failure",
            status: "stopped",
            source: "api",
            created_at: iso(),
            updated_at: iso(),
          };
          w.agents.push(a);
          audit("agent.create", a.id, a.project, { name: a.name, harness: a.harness });
          return a;
        }),
      update: (id, patch) =>
        reply(() => {
          const a = agent(id);
          if (a.source === "file") fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file; edit the file, then POST /api/reload`);
          if (patch.harness) a.harness = patch.harness;
          if (patch.subscription !== undefined) a.subscription = patch.subscription || undefined;
          if (patch.model !== undefined) a.model = patch.model || undefined;
          if (patch.prompt !== undefined) a.prompt = patch.prompt || undefined;
          if (patch.policy) a.policy = { targets: patch.policy.targets, mail_to: patch.policy.mail_to, ...(patch.policy.cross_project?.length ? { cross_project: patch.policy.cross_project } : {}) };
          if (patch.restart) a.restart = patch.restart;
          a.updated_at = iso();
          audit("agent.update", a.id, a.project, { ...patch });
          return a;
        }),
      remove: (id, confirm) =>
        reply(() => {
          const a = agent(id);
          if (a.source === "file") fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file`);
          if (LIVE.includes(a.status) && !confirm) fail(409, "confirm_required", `agent \`${a.project}/${a.name}\` has a live session; send ?confirm=true`);
          w.agents = w.agents.filter((x) => x.id !== a.id);
          audit("agent.delete", a.id, a.project, { name: a.name });
        }),
      // 202: the answer shows the agent as the daemon accepted the call. The
      // fake supervisor then steps through the statuses, as the daemon's does.
      start: (id) =>
        reply(() => {
          const a = agent(id);
          if (LIVE.includes(a.status)) fail(409, "already_running", `agent \`${a.project}/${a.name}\` is running already`);
          const snap = clone(a);
          audit("agent.start", a.id, a.project);
          later(() => {
            openSession(a);
            setStatus(a, "starting");
            later(() => setStatus(a, "idle"));
          }, 0);
          return snap;
        }),
      stop: (id) =>
        reply(() => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent \`${a.project}/${a.name}\` is not running`);
          const snap = clone(a);
          audit("agent.stop", a.id, a.project);
          later(() => {
            endSession(a, "stopped");
            setStatus(a, "stopped", "stopped by operator");
          }, 0);
          return snap;
        }),
      restart: (id) =>
        reply(() => {
          const a = agent(id);
          const snap = clone(a);
          audit("agent.restart", a.id, a.project);
          later(() => {
            const wasLive = LIVE.includes(a.status);
            if (wasLive) setStatus(a, "restarting");
            openSession(a);
            setStatus(a, "starting");
            later(() => setStatus(a, "idle"));
          }, 0);
          return snap;
        }),
      send: (id, text) =>
        reply(() => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent \`${a.project}/${a.name}\` is not running`);
          if (!text.trim()) fail(422, "invalid", "the text is empty");
          audit("agent.send", a.id, a.project, { chars: text.length });
          later(() => {
            const s = w.sessions.find((x) => x.agent === a.id && x.current);
            if (s) s.turns++;
            agentEvent(a, "turn_start", `Turn typed by operator: ${text.slice(0, 60)}`, { text }, "operator");
            setStatus(a, "working");
            later(() => {
              agentEvent(a, "turn_end", "Turn ended");
              setStatus(a, "idle");
            });
          }, 0);
          return { accepted: true };
        }),
      answer: (id) =>
        reply(() => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent \`${a.project}/${a.name}\` is not running`);
          audit("agent.answer", a.id, a.project);
          return { accepted: true };
        }),
      events: (id, q = {}) =>
        reply(() => {
          const a = agent(id);
          return eventsAfter(q, (e) => e.agent_id === a.id);
        }),
      sessions: (id, q = {}) =>
        reply(() => {
          const a = agent(id);
          const list = w.sessions.filter((s) => s.agent === a.id).sort((x, y) => y.started_at.localeCompare(x.started_at));
          return paginate(list, q, (s) => s.id);
        }),
      roles: (id) => reply(() => agent(id).roles),
      setRoles: (id, roles) =>
        reply(() => {
          const a = agent(id);
          if (a.source === "file") fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file`);
          const next = [...new Set(roles.filter((r) => r !== "member"))];
          for (const r of next) if (r !== "project_admin" && r !== "sysadmin") fail(422, "invalid", `\`${String(r)}\` is not a role`);
          audit("agent.roles", a.id, a.project, { from: a.roles, to: next });
          a.roles = next;
          return a.roles;
        }),
      leases: (id, o = {}) =>
        reply(() => {
          const a = agent(id);
          return leasesWhere((l) => l.agent === a.id, o.all);
        }),
      pty: (id) => {
        const a = w.agents.find((x) => x.id === id || `${x.project}/${x.name}` === id || x.name === id);
        const name = a?.name ?? id;
        let n = 0;
        return sock(`/api/agents/${encodeURIComponent(id)}/pty`, (protocol) => new FakePty({
          protocol,
          // The daemon answers 409 not_running before the upgrade.
          refuse: !a || !LIVE.includes(a.status),
          banner: [
            `\x1b[1;35m✻ ${a?.harness ?? "claude"}\x1b[0m  \x1b[2m(mock PTY for ${name})\x1b[0m`,
            `\x1b[2mcwd: ${a?.workspace ?? "/state/agents"}\x1b[0m`,
            "",
            "Type a line and press Enter. The mock echoes it.",
          ],
          ticker: () => {
            if (a?.status !== "working") return null;
            n++;
            return `\x1b[36m●\x1b[0m ${TOOLS[n % TOOLS.length]}(${FILES[n % FILES.length]}) \x1b[2m⎿ ok\x1b[0m`;
          },
          onLine: (line) => (line ? [`\x1b[33m(mock)\x1b[0m turn received: ${line}`, `\x1b[2mThinking… done.\x1b[0m`] : []),
        }));
      },
    },

    board: {
      get: (proj) => reply(() => boardOf(proj)),
      update: (proj, columns: ColumnEdit[]) =>
        reply(() => {
          project(proj);
          const old = w.columns[proj] ?? [];
          const names = columns.map((c) => c.name);
          for (const r of REQUIRED_COLUMNS) if (!names.includes(r)) fail(422, "invalid", `the column \`${r}\` must stay and keep its name`);
          if (new Set(names).size !== names.length) fail(422, "invalid", "two columns have the same name");
          const renamed = new Map(columns.filter((c) => c.rename_from).map((c) => [c.rename_from!, c.name]));
          for (const o of old) {
            if (names.includes(o.name) || renamed.has(o.name)) continue;
            if (activeIn(proj, o.name).length) fail(409, "conflict", `column \`${o.name}\` has cards`);
          }
          for (const [from, to] of renamed) for (const c of w.cards.filter((x) => x.project === proj && x.column === from)) c.column = to;
          w.columns[proj] = columns.map((c, i) => ({ name: c.name, position: i, ...(c.wip_limit !== undefined ? { wip_limit: c.wip_limit } : {}) }));
          audit("board.columns", proj, proj, { columns: names });
          emit({ kind: "board", actor: "operator", project: proj, summary: `operator changed the columns of ${proj}`, data: { columns: names } });
          return boardOf(proj);
        }),
    },

    cards: {
      list: (proj, q = {}) =>
        reply(() => {
          project(proj);
          const rows = w.cards
            .filter((c) => c.project === proj && (q.archived ? true : !c.archived) && (!q.column || c.column === q.column) && (!q.label || c.labels.includes(q.label)) && (!q.unassigned || !c.assignee) && (!q.assignee || c.assignee === q.assignee || c.assignee_name === q.assignee))
            .sort((a, b) => Number(a.id.split("-")[1]) - Number(b.id.split("-")[1]))
            .map(view);
          return paginate(rows, q, (c) => c.id.split("-")[1]!);
        }),
      create: (proj, c) =>
        reply(() => {
          const p = project(proj);
          const msg = c.from_mail ? find(w.mail, (m) => m.id === c.from_mail, `message ${c.from_mail}`) : undefined;
          const title = c.title?.trim() || msg?.subject || fail(422, "invalid", "a card needs a title");
          const n = Math.max(0, ...w.cards.filter((x) => x.project === proj).map((x) => Number(x.id.split("-")[1]) || 0)) + 1;
          const column = c.column ?? "backlog";
          if (!(w.columns[proj] ?? []).some((x) => x.name === column)) fail(422, "invalid", `no column \`${column}\``);
          if (column === "blocked" && !c.comment?.trim()) fail(422, "comment_required", "a move to `blocked` needs a comment");
          const card: Card = {
            id: `${p.card_prefix}-${n}`,
            project: proj,
            column,
            position: activeIn(proj, column).length,
            title,
            body: c.body ?? (msg ? `From ${msg.from_name}:\n\n${msg.body}` : ""),
            labels: c.labels ?? [],
            ...(c.repo ? { repo: c.repo } : {}),
            ...(c.branch ? { branch: c.branch } : {}),
            depends_on: c.depends_on ?? [],
            blocked_by: [],
            comments: [],
            links: msg ? [{ kind: "thread", ref: msg.thread_id, created_by: "operator", created_at: iso() }] : [],
            created_by: "operator",
            created_by_name: "operator",
            created_at: iso(),
            updated_at: iso(),
            archived: false,
          };
          w.cards.push(card);
          if (c.comment?.trim()) comment(card, c.comment.trim());
          cardEvent(card, "created", `operator created ${card.id}`);
          if (c.assignee) assign(card, c.assignee);
          return view(card);
        }),
      get: (id) => reply(() => view(cardById(id))),
      update: (id, patch) =>
        reply(() => {
          const c = cardById(id);
          if (c.archived) fail(409, "conflict", `${c.id} is archived`);
          const fields: string[] = [];
          const self = patch.assignee === "me" || patch.assignee === "operator";
          if (patch.assignee !== undefined && self) assign(c, patch.assignee);
          if (patch.column !== undefined || patch.position !== undefined) move(c, patch.column ?? c.column, patch.position, patch.comment);
          else if (patch.comment?.trim()) {
            comment(c, patch.comment.trim());
            cardEvent(c, "commented", `operator commented on ${c.id}`, { comment: patch.comment.trim() });
          }
          if (patch.assignee !== undefined && !self) assign(c, patch.assignee);
          for (const k of ["title", "body", "labels", "depends_on"] as const) {
            if (patch[k] !== undefined) {
              (c as unknown as Record<string, unknown>)[k] = patch[k];
              fields.push(k);
            }
          }
          for (const k of ["repo", "branch"] as const) {
            if (patch[k] !== undefined) {
              if (patch[k] === null) delete c[k];
              else c[k] = patch[k];
              fields.push(k);
            }
          }
          if (fields.length) {
            c.updated_at = iso();
            cardEvent(c, "edited", `operator edited ${c.id}`, { fields });
          }
          return view(c);
        }),
      archive: (id) =>
        reply(() => {
          const c = cardById(id);
          if (c.archived) fail(409, "conflict", `${c.id} is archived`);
          c.archived = true;
          reindex(c.project, c.column);
          cardEvent(c, "archived", `operator archived ${c.id}`);
        }),
      move: (id, to) =>
        reply(() => {
          const c = cardById(id);
          move(c, to.column, to.position, to.comment);
          return view(c);
        }),
      assign: (id, assignee) =>
        reply(() => {
          const c = cardById(id);
          assign(c, assignee);
          return view(c);
        }),
      comment: (id, body) =>
        reply(() => {
          const c = cardById(id);
          if (!body.trim()) fail(422, "invalid", "the comment is empty");
          comment(c, body);
          cardEvent(c, "commented", `operator commented on ${c.id}`, { comment: body });
          return view(c);
        }),
      link: (id, kind, ref) =>
        reply(() => {
          const c = cardById(id);
          c.links.push({ kind, ref, created_by: "operator", created_at: iso() });
          cardEvent(c, "linked", `operator linked ${kind} ${ref} to ${c.id}`, { kind, ref });
          return view(c);
        }),
      events: (id, q = {}) =>
        reply(() => {
          const c = cardById(id);
          return eventsAfter(q, (e) => e.card === c.id);
        }),
    },

    memories: {
      list: (proj, q = {}) =>
        reply(() => {
          project(proj);
          const needle = q.q?.toLowerCase();
          const authorAddr = q.author === "me" ? "operator" : q.author;
          return w.memories
            .filter(
              (m) =>
                m.project === proj &&
                (q.all || !m.superseded_by) &&
                (!q.tag || m.tags.includes(q.tag)) &&
                (!authorAddr || m.author === authorAddr || m.author_name === authorAddr) &&
                (!needle || needle.split(/\s+/).every((word) => `${m.title} ${m.body} ${m.tags.join(" ")}`.toLowerCase().includes(word))),
            )
            .sort((x, y) => memNo(y.id) - memNo(x.id))
            .map(memView);
        }),
      create: (proj, m) =>
        reply(() => {
          project(proj);
          if (!m.title?.trim()) fail(422, "invalid", "a memory needs a title");
          const n = Math.max(0, ...w.memories.filter((x) => x.project === proj).map((x) => memNo(x.id))) + 1;
          const id = `MEM-${n}`;
          const words = m.title.toLowerCase().split(/\s+/);
          const similar = w.memories
            .filter((x) => x.project === proj && words.some((wd) => x.title.toLowerCase().includes(wd)))
            .slice(0, 5)
            .map((x) => ({ id: x.id, title: x.title, superseded: !!x.superseded_by }));
          const mm: Memory = { id, key: `${proj}/${id}`, project: proj, title: m.title, body: m.body ?? "", tags: m.tags ?? [], author: "operator", author_name: "operator", sources: m.sources ?? [], supersedes: [], links: [], created_at: iso(), updated_at: iso() };
          w.memories.push(mm);
          for (const l of m.links ?? []) w.links.push({ from: id, to: memory(l.to, proj).id, relation: l.relation, note: l.note, author: "operator", author_name: "operator", created_at: iso() });
          emit({ kind: "memory.created", actor: "operator", project: proj, summary: `operator wrote ${id}: ${m.title}`, data: { memory_id: id, key: mm.key, title: m.title } });
          return { ...memView(mm), similar };
        }),
      get: (id) => reply(() => memView(memory(id))),
      update: (id, patch) =>
        reply(() => {
          const m = memory(id);
          const fields: string[] = [];
          if (patch.title !== undefined) m.title = patch.title;
          if (patch.body !== undefined) m.body = patch.body;
          if (patch.sources !== undefined) m.sources = patch.sources;
          if (patch.tags !== undefined) m.tags = patch.tags;
          if (patch.add_tags) m.tags = [...new Set([...m.tags, ...patch.add_tags])];
          if (patch.remove_tags) m.tags = m.tags.filter((t) => !patch.remove_tags!.includes(t));
          for (const k of ["title", "body", "sources", "tags"] as const) if (patch[k] !== undefined) fields.push(k);
          if (patch.add_tags || patch.remove_tags) fields.push("tags");
          m.updated_at = iso();
          emit({ kind: "memory.updated", actor: "operator", project: m.project, summary: `operator edited ${m.id}`, data: { memory_id: m.id, key: m.key, title: m.title, fields } });
          return memView(m);
        }),
      remove: (id) =>
        reply(() => {
          const m = memory(id);
          w.memories = w.memories.filter((x) => x !== m);
          w.links = w.links.filter((l) => !(memKey(m.project, l.from) === m.key || memKey(m.project, l.to) === m.key));
          audit("memory.delete", m.key, m.project, { title: m.title });
          emit({ kind: "memory.deleted", actor: "operator", project: m.project, summary: `operator deleted ${m.id}`, data: { memory_id: m.id, key: m.key, title: m.title } });
        }),
      graph: (id, depth = 1) =>
        reply(() => {
          const root = memory(id);
          const d = Math.max(1, Math.min(depth, 5));
          const proj = root.project;
          const seen = new Set([root.id]);
          let frontier = [root.id];
          for (let i = 0; i < d; i++) {
            const next: string[] = [];
            for (const l of w.links.filter((x) => memProject(x.from) === proj)) {
              for (const [x, y] of [[l.from, l.to], [l.to, l.from]] as const) {
                if (frontier.includes(x) && !seen.has(y)) {
                  seen.add(y);
                  next.push(y);
                }
              }
            }
            frontier = next;
          }
          return {
            root: root.id,
            depth: d,
            nodes: w.memories.filter((m) => m.project === proj && seen.has(m.id)).map(memView),
            edges: w.links.filter((l) => memProject(l.from) === proj && seen.has(l.from) && seen.has(l.to)).map(({ from, to, relation, note, author, created_at }) => ({ from, to, relation, ...(note ? { note } : {}), author, created_at })),
          };
        }),
      link: (id, to, relation, note) =>
        reply(() => {
          const m = memory(id);
          const other = memory(to, m.project);
          if (other.project !== m.project) fail(422, "invalid", "links stay inside one project");
          if (relation === "supersedes") supersede(other, m);
          else w.links.push({ from: m.id, to: other.id, relation, ...(note ? { note } : {}), author: "operator", author_name: "operator", created_at: iso() });
          emit({ kind: "memory.linked", actor: "operator", project: m.project, summary: `operator linked ${m.id} ${relation} ${other.id}`, data: { memory_id: m.id, key: m.key, title: m.title, to: other.id, relation, note: note ?? null } });
          return memView(m);
        }),
      unlink: (id, to, relation) =>
        reply(() => {
          const m = memory(id);
          const other = memory(to, m.project);
          w.links = w.links.filter((l) => !(l.from === m.id && l.to === other.id && l.relation === relation && memProject(l.from) === m.project));
          emit({ kind: "memory.unlinked", actor: "operator", project: m.project, summary: `operator unlinked ${m.id} ${relation} ${other.id}`, data: { memory_id: m.id, key: m.key, title: m.title, to: other.id, relation } });
          return memView(m);
        }),
      supersede: (id, by) =>
        reply(() => {
          const old = memory(id);
          const next = memory(by, old.project);
          supersede(old, next);
          return memView(old);
        }),
    },

    wiki: {
      tree: (proj, q = {}) =>
        reply(() => {
          project(proj);
          ensureIndex(proj);
          const needle = q.q?.toLowerCase();
          return w.pages
            .filter((p) => p.project === proj && (q.deleted ? !!p.deleted_at : !p.deleted_at) && (!q.status || p.status === q.status) && (!needle || `${p.path} ${p.title} ${p.body}`.toLowerCase().includes(needle)))
            .sort((x, y) => x.path.localeCompare(y.path))
            .map((p) => summaryOf(p, needle));
        }),
      create: (proj, pg) =>
        reply(() => {
          project(proj);
          if (!/^[a-z0-9_.-]+(\/[a-z0-9_.-]+)*$/.test(pg.path)) fail(422, "invalid", `\`${pg.path}\` is not a valid page path`);
          if (w.pages.some((p) => p.project === proj && p.path === pg.path && !p.deleted_at)) fail(409, "conflict", `page \`${pg.path}\` exists`);
          if (!pg.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          const created: WikiPage = { id: ulid("pg"), project: proj, path: pg.path, title: "", rev: 1, body: pg.body, author: "operator", author_name: "operator", summary: pg.summary, created_by: "operator", created_at: iso(), updated_at: iso(), links: [] };
          applyBody(created, pg.body, pg.title);
          w.pages.push(created);
          w.revisions[created.id] = [{ page: created.id, rev: 1, author: "operator", author_name: "operator", summary: pg.summary, created_at: iso(), body: pg.body }];
          wikiEvent("wiki.created", created, `wiki ${created.path} created (rev 1)`);
          return created;
        }),
      get: (id) => reply(() => page(id)),
      save: (id, body) =>
        reply(() => {
          const pg = page(id);
          if (body.base !== pg.rev) {
            const baseBody = (w.revisions[pg.id] ?? []).find((r) => r.rev === body.base)?.body ?? "";
            fail(409, "stale_base", `page \`${pg.path}\` is at rev ${pg.rev}, not your base rev ${body.base}; merge the changes since your base, then save with base ${pg.rev}`, {
              base: body.base,
              current: pg.rev,
              diff: `--- rev ${body.base}\n+++ rev ${pg.rev}\n${lineDiff(baseBody, pg.body)}`,
            });
          }
          if (!body.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          newRevision(pg, body.body, body.summary, body.title);
          wikiEvent("wiki.saved", pg, `wiki ${pg.path} saved (rev ${pg.rev})`, { summary: body.summary });
          return pg;
        }),
      remove: (id) =>
        reply(() => {
          const pg = page(id);
          pg.deleted_at = iso();
          audit("wiki.delete", pg.id, pg.project, { path: pg.path });
          wikiEvent("wiki.deleted", pg, `wiki ${pg.path} deleted`);
        }),
      section: (id, sct) =>
        reply(() => {
          const pg = page(id);
          if (!sct.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          newRevision(pg, `${pg.body.replace(/\n*$/, "")}\n\n## ${sct.heading}\n\n${sct.body}\n`, sct.summary);
          wikiEvent("wiki.saved", pg, `wiki ${pg.path} saved (rev ${pg.rev})`, { summary: sct.summary });
          return pg;
        }),
      append: (id, sct) =>
        reply(() => {
          const pg = page(id);
          if (!sct.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          newRevision(pg, `${pg.body.replace(/\n*$/, "")}\n\n${sct.body}\n`, sct.summary);
          wikiEvent("wiki.saved", pg, `wiki ${pg.path} saved (rev ${pg.rev})`, { summary: sct.summary });
          return pg;
        }),
      revisions: (id) => reply(() => (w.revisions[page(id).id] ?? []).map(({ body: _b, ...r }) => r).reverse()),
      revision: (id, rev) => reply(() => find(w.revisions[page(id).id] ?? [], (r) => r.rev === rev, `revision ${rev}`)),
      diff: (id, x, y) =>
        reply(() => {
          const pg = page(id);
          const b = y ?? pg.rev;
          const a = x ?? b - 1;
          const ra = find(w.revisions[pg.id] ?? [], (r) => r.rev === a, `revision ${a}`).body;
          const rb = find(w.revisions[pg.id] ?? [], (r) => r.rev === b, `revision ${b}`).body;
          return { page: pg.id, a, b, diff: `--- rev ${a}\n+++ rev ${b}\n${lineDiff(ra, rb)}` };
        }),
      restore: (id, rev) =>
        reply(() => {
          const pg = page(id);
          const r = find(w.revisions[pg.id] ?? [], (x) => x.rev === rev, `revision ${rev}`);
          const from = pg.rev;
          newRevision(pg, r.body, `Restore rev ${rev}`);
          wikiEvent("wiki.restored", pg, `wiki ${pg.path} restored rev ${rev}`, { from_rev: from });
          return pg;
        }),
      move: (id, path) =>
        reply(() => {
          const pg = page(id);
          const from = pg.path;
          pg.path = path;
          audit("wiki.move", pg.id, pg.project, { from, to: path });
          wikiEvent("wiki.moved", pg, `wiki ${from} moved to ${path}`, { from });
          return pg;
        }),
      backlinks: (id) =>
        reply(() => {
          const pg = page(id);
          return w.pages.filter((p) => p.id !== pg.id && !p.deleted_at && p.body.includes(`[[${pg.path}`)).map((p) => summaryOf(p));
        }),
      review: (id, decision, comment) =>
        reply(() => {
          const pg = page(id);
          if (decision === "comment" && !comment?.trim()) fail(422, "comment_required", "a review comment needs `comment`");
          if (decision === "approve") {
            const { body } = splitFrontMatter(pg.body);
            const fm = pg.reviewer ? `---\nstatus: reviewed\nreviewer: ${pg.reviewer}\n---\n` : "---\nstatus: reviewed\n---\n";
            newRevision(pg, fm + body, `Reviewed by operator${comment ? `: ${comment}` : ""}`);
          }
          sendMail(pg.author === "system" ? "operator" : pg.author, `project:${pg.project}`, `Review of ${pg.path}: ${decision}`, comment ?? "");
          wikiEvent("wiki.reviewed", pg, `operator ${decision === "approve" ? "approved" : "commented on"} ${pg.path}`, { decision, comment: comment ?? null });
          return pg;
        }),
    },

    artifacts: {
      list: (proj) => reply(() => (project(proj), w.artifacts.filter((a) => a.project === proj).sort((x, y) => artNo(y.id) - artNo(x.id)).map(artView))),
      usage: (proj) =>
        reply(() => {
          project(proj);
          const list = w.artifacts.filter((a) => a.project === proj);
          return { project: proj, count: list.length, used_bytes: list.reduce((n, a) => n + a.size, 0), quota_bytes: 1 << 30, max_file_bytes: 32 << 20 };
        }),
      upload: (proj, form) =>
        reply(() => {
          const file = form.get("file");
          const name = String(form.get("name") || (file instanceof File ? file.name : "upload.bin"));
          const kind = form.get("kind") === "bundle" ? ("bundle" as const) : ("file" as const);
          const ak = form.get("attach_kind");
          const ar = form.get("attach_ref");
          const attach = typeof ak === "string" && typeof ar === "string" && ak && ar ? { kind: ak as ArtifactLinkKind, ref: ar } : undefined;
          const type = kind === "bundle" ? "text/html; charset=utf-8" : file instanceof File && file.type ? file.type : "application/octet-stream";
          return artView(addArtifact(proj, name, type, file instanceof File ? file.size : 0, kind, attach));
        }),
      uploadJson: (proj, body) =>
        reply(() => {
          if (!body.name.trim()) fail(422, "invalid", "an artifact needs a name");
          const kind = body.kind ?? "file";
          return artView(addArtifact(proj, body.name, kind === "bundle" ? "text/html; charset=utf-8" : (body.content_type ?? "application/octet-stream"), Math.floor((body.data_base64.length * 3) / 4), kind, body.attach));
        }),
      get: (id) => reply(() => artView(artifact(id))),
      remove: (id) =>
        reply(() => {
          const a = artifact(id);
          if (a.links.some((l) => l.kind === "version" && w.versions.find((v) => v.id === l.ref)?.status === "released")) fail(409, "conflict", `${a.id} is on a released version`);
          w.artifacts = w.artifacts.filter((x) => x !== a);
          emit({ kind: "artifact.deleted", actor: "operator", project: a.project, summary: `operator deleted ${a.id}`, data: { artifact_id: a.id, key: a.key, uid: a.uid, name: a.name, kind: a.kind } });
        }),
      link: (id, kind, ref) =>
        reply(() => {
          const a = artifact(id);
          const label = kind === "version" ? w.versions.find((v) => v.id === ref || v.name === ref)?.name : kind === "page" ? w.pages.find((p) => p.id === ref || p.path === ref)?.path : undefined;
          a.links.push({ kind, ref, ...(label ? { label } : {}), created_by: "operator", created_at: iso() });
          emit({ kind: "artifact.linked", actor: "operator", project: a.project, summary: `operator attached ${a.id} to ${kind} ${ref}`, data: { artifact_id: a.id, key: a.key, uid: a.uid, name: a.name, kind: a.kind, link_kind: kind, ref } });
          return artView(a);
        }),
      // The daemon signs `/a/{uid}/{name}?exp&sig`. The mock hands out data: URLs.
      url: (id) =>
        reply(() => {
          const a = artifact(id);
          const raw = w.artifactBodies[a.id] ?? `uploaded ${a.name}`;
          const url = raw.startsWith("data:") ? raw : `data:${a.content_type.split(";")[0]};charset=utf-8,${encodeURIComponent(raw)}`;
          const exp = Math.floor(now() / 1000) + 300;
          return { url, exp, expires_at: new Date(exp * 1000).toISOString() };
        }),
      text: (url) =>
        reply(() => {
          const m = /^data:[^,]*,(.*)$/s.exec(url);
          return m ? decodeURIComponent(m[1]!) : "";
        }),
    },

    versions: {
      list: (proj, q = {}) =>
        reply(() => {
          project(proj);
          return w.versions.filter((v) => v.project === proj && (!q.status || v.status === q.status) && (!q.name || v.name === q.name)).sort((a, b) => b.created_at.localeCompare(a.created_at));
        }),
      create: (proj, v) =>
        reply(() => {
          project(proj);
          if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$/.test(v.name)) fail(422, "invalid", `\`${v.name}\` is not a valid version name`);
          if (w.versions.some((x) => x.project === proj && x.name === v.name)) fail(409, "conflict", `version \`${v.name}\` exists`);
          const id = ulid("ver");
          const ver: Version = {
            id, project: proj, name: v.name, notes: v.notes ?? "", status: "draft", ...(v.git_ref ? { git_ref: v.git_ref } : {}), ...(v.repo ? { repo: v.repo } : {}),
            cards: (v.cards ?? []).map(cardRef), artifacts: (v.artifacts ?? []).map(artRef), reviews: [],
            created_by: "operator", created_by_name: "operator", created_at: iso(), updated_at: iso(), url: `/p/${proj}/versions/${id}`,
          };
          w.versions.push(ver);
          emit({ kind: "version.created", actor: "operator", project: proj, summary: `operator created version ${v.name}`, data: { version_id: id, name: v.name, status: "draft" } });
          return ver;
        }),
      get: (id) => reply(() => version(id)),
      update: (id, patch) =>
        reply(() => {
          const v = version(id);
          if (v.status !== "draft" && v.status !== "rejected") fail(409, "conflict", `version ${v.name} is ${v.status}; edits happen in draft and rejected only`);
          if (patch.name !== undefined) v.name = patch.name;
          if (patch.notes !== undefined) v.notes = patch.notes;
          if (patch.git_ref !== undefined) v.git_ref = patch.git_ref || undefined;
          if (patch.repo !== undefined) v.repo = patch.repo || undefined;
          let cards = v.cards.map((c) => c.id);
          if (patch.cards) cards = patch.cards;
          cards = [...new Set([...cards, ...(patch.add_cards ?? [])])].filter((c) => !(patch.remove_cards ?? []).includes(c));
          v.cards = cards.map(cardRef);
          let arts = v.artifacts.map((a) => a.id);
          if (patch.artifacts) arts = patch.artifacts;
          arts = [...new Set([...arts, ...(patch.add_artifacts ?? [])])].filter((a) => !(patch.remove_artifacts ?? []).includes(a));
          v.artifacts = arts.map(artRef);
          v.updated_at = iso();
          emit({ kind: "version.updated", actor: "operator", project: v.project, summary: `operator edited version ${v.name}`, data: { version_id: v.id, name: v.name, status: v.status, fields: Object.keys(patch) } });
          return v;
        }),
      transition: (id, action, comment) =>
        reply(() => {
          const v = version(id);
          const allowed: Record<typeof action, Version["status"][]> = {
            submit: ["draft", "rejected"],
            approve: ["in_review"],
            reject: ["in_review"],
            release: ["approved"],
          };
          if (!allowed[action].includes(v.status)) fail(409, "conflict", `cannot ${action} a version that is ${v.status}`);
          if (action === "reject" && !comment?.trim()) fail(422, "comment_required", "a rejection needs a comment");
          const next = { submit: "in_review", approve: "approved", reject: "rejected", release: "released" } as const;
          const decision = { submit: "submitted", approve: "approved", reject: "rejected", release: "released" } as const;
          v.status = next[action];
          if (action === "submit") {
            v.submitted_at = iso();
            v.submitted_by = "operator";
            if (!v.thread_id) v.thread_id = sendMail("operator", `project:${v.project}`, `Version ${v.name} is ready for review`, `${v.notes}\n\n${comment ?? ""}`).thread_id;
          }
          if (action === "approve" || action === "reject") {
            v.decided_at = iso();
            v.decided_by = "operator";
            audit(`version.${action}`, v.id, v.project, { name: v.name, comment: comment ?? null });
          }
          if (action === "release") {
            v.released_at = iso();
            audit("version.release", v.id, v.project, { name: v.name });
          }
          v.reviews.push({ id: ulid("rev"), author: "operator", author_name: "operator", decision: decision[action], comment: comment ?? "", created_at: iso() });
          v.updated_at = iso();
          emit({ kind: `version.${decision[action]}` as AgentEvent["kind"], actor: "operator", project: v.project, summary: `operator ${decision[action]} ${v.name}`, data: { version_id: v.id, name: v.name, status: v.status, comment: comment ?? null } });
          return v;
        }),
      compare: (id, to) =>
        reply(() => {
          const a = version(id);
          const b = version(to);
          const ac = a.cards.map((c) => c.id);
          const bc = b.cards.map((c) => c.id);
          const aa = a.artifacts.map((x) => x.id);
          const ba = b.artifacts.map((x) => x.id);
          const times = [a.released_at ?? a.submitted_at ?? iso(), b.released_at ?? b.submitted_at ?? iso()].sort();
          return {
            version: a.name,
            to: b.name,
            since: times[0],
            until: times[1],
            cards: { added: ac.filter((c) => !bc.includes(c)), removed: bc.filter((c) => !ac.includes(c)), landed: w.cards.filter((c) => c.project === a.project && c.column === "done").map((c) => c.id) },
            artifacts: { added: aa.filter((x) => !ba.includes(x)), removed: ba.filter((x) => !aa.includes(x)) },
            pages: w.pages.filter((p) => p.project === a.project && !p.deleted_at && p.updated_at >= times[0]! && p.updated_at <= times[1]!).map((p) => ({ id: p.id, path: p.path, from_rev: Math.max(1, p.rev - 1), to_rev: p.rev })),
          };
        }),
    },

    subscriptions: {
      list: () => reply(() => w.subscriptions.map(subView)),
      get: (name) => reply(() => subView(sub(name))),
      create: (def) =>
        reply(() => {
          checkSubName(def.name);
          if (w.subscriptions.some((x) => x.name === def.name)) fail(409, "conflict", `subscription \`${def.name}\` exists`);
          if (def.probe && !PROBES.includes(def.probe)) fail(422, "invalid", `unknown probe \`${def.probe}\``);
          if (!def.provider?.trim()) fail(422, "invalid", "the provider is empty");
          const s: Subscription = {
            name: def.name, provider: def.provider, ...(def.plan ? { plan: def.plan } : {}), harnesses: def.harnesses ?? [], ...(def.probe ? { probe: def.probe } : {}),
            alert_at: def.alert_at ?? 80, source: "api", login_status: "unknown", probe_status: "none", created_at: iso(), updated_at: iso(),
            windows: [], local_estimate_only: !def.probe, has_api_key: false, held_agents: 0,
          };
          w.subscriptions.push(s);
          audit("subscription.create", s.name, undefined, { provider: s.provider });
          return subView(s);
        }),
      remove: (name) =>
        reply(() => {
          const s = sub(name);
          if (s.source === "file") fail(409, "file_managed", `subscription \`${name}\` comes from subscriptions/${name}.toml; edit or delete the file`);
          w.subscriptions.splice(w.subscriptions.indexOf(s), 1);
          delete w.apiKeys[name];
          w.quota = w.quota.filter((q) => q.subscription !== name);
          audit("subscription.delete", name, undefined);
        }),
      quota: (name, q = {}) =>
        reply(() => {
          sub(name);
          const since = q.since || new Date(now() - 24 * 3600_000).toISOString();
          return w.quota.filter((x) => x.subscription === name && x.at >= since && (!q.window || x.window === q.window)).slice(0, q.limit ?? 1000);
        }),
      probe: (name) =>
        reply(() => {
          const s = sub(name);
          if (!s.probe) return { subscription: s.name, probe_status: "none" as const, windows: [] };
          s.last_probe_at = iso();
          if (s.login_status === "missing" || s.login_status === "unknown" || s.probe_status === "failed") {
            s.probe_status = "failed";
            s.probe_error ??= "no credential in the shared harness home";
            return { subscription: s.name, probe_status: "failed" as const, error: s.probe_error, windows: [] };
          }
          reading(s);
          return { subscription: s.name, probe_status: "ok" as const, windows: s.windows.map((x) => ({ window: x.window, used_pct: x.used_pct, resets_at: x.resets_at, raw: { utilization: x.used_pct } })) };
        }),
      verify: (name) =>
        reply(() => {
          const s = sub(name);
          if (s.env_var && w.apiKeys[name]) {
            s.login_status = "logged_in";
            return { subscription: subView(s), status: { logged_in: true, detail: "a stored API key" } };
          }
          const harness = s.harnesses[0];
          if (!harness) fail(422, "invalid", `subscription \`${name}\` names no harness to check`);
          const ok = s.login_status !== "missing" && s.login_status !== "expired";
          s.login_status = ok ? "logged_in" : "missing";
          s.verified_at = iso();
          audit("subscription.verify", name, undefined, { logged_in: ok });
          return { subscription: subView(s), status: { kind: harness, harness, provider: s.provider, logged_in: ok, ...(s.plan ? { plan: s.plan } : {}), detail: ok ? "Logged in" : "Not logged in" } };
        }),
      logout: (name) =>
        reply(() => {
          const s = sub(name);
          const harness = s.harnesses[0];
          if (!harness) fail(422, "invalid", `subscription \`${name}\` names no harness`);
          if (harness === "pi") fail(422, "invalid", "pi has no logout command");
          s.login_status = "missing";
          audit("subscription.logout", name, undefined, { kind: harness });
          return subView(s);
        }),
      setApiKey: (name, input) =>
        reply(() => {
          if (!input.key.trim()) fail(422, "invalid", "the key is empty");
          const env = KEY_ENV[input.provider];
          if (!env) fail(422, "invalid", `unknown provider \`${input.provider}\`; use anthropic, openai, openai-compatible, google, openrouter, xai, groq, or github`);
          if (input.provider === "openai-compatible" && !input.base_url?.trim()) fail(422, "invalid", "openai-compatible needs base_url");
          let s = w.subscriptions.find((x) => x.name === name);
          if (!s) {
            checkSubName(name);
            s = { name, provider: input.provider, plan: "API key", harnesses: [input.provider === "github" ? "copilot" : "pi"], alert_at: 80, source: "api", login_status: "logged_in", probe_status: "none", created_at: iso(), updated_at: iso(), windows: [], local_estimate_only: true, has_api_key: true, held_agents: 0 };
            w.subscriptions.push(s);
          }
          w.apiKeys[name] = { provider: input.provider, key: input.key, base_url: input.base_url, models: input.models };
          s.env_var = env;
          s.login_status = "logged_in";
          s.updated_at = iso();
          audit("subscription.api_key", name, undefined, { provider: input.provider, env_var: env, base_url: input.base_url ?? null, models: input.models ?? [] });
          return subView(s);
        }),
      detect: () => reply(detected),
      startLogin: (req) =>
        reply(() => {
          if (!["claude", "codex", "copilot", "pi"].includes(req.kind)) fail(422, "invalid", `unknown login kind \`${req.kind}\``);
          if (req.kind === "pi" && !req.provider) fail(422, "invalid", "a pi login needs a provider");
          if (req.name) checkSubName(req.name);
          if ([...logins.values()].filter((x) => !x.done).length >= 4) fail(409, "conflict", "four login sessions are live; finish or cancel one");
          const s: LoginSession & { name?: string } = {
            id: ulid("login"), kind: req.kind, ...(req.provider ? { provider: req.provider } : {}), state: "starting", started_at: iso(), done: false, instructions: INSTRUCTIONS(req.kind, req.provider),
            ...(req.name ? { name: req.name } : {}),
          };
          logins.set(s.id, s);
          audit("login.start", s.id, undefined, { kind: s.kind, provider: s.provider ?? null });
          if (s.kind !== "pi") setTimeout(() => toWaiting(s), opts.loginStepMs ?? (latency > 0 ? 800 : 0));
          setTimeout(() => endLogin(s, "expired"), opts.loginTtlMs ?? 10 * 60_000);
          return s;
        }),
      loginState: (id) => reply(() => logins.get(id) ?? fail(404, "not_found", `login session \`${id}\` not found`)),
      cancelLogin: (id) =>
        reply(() => {
          const s = live(id);
          audit("login.cancel", s.id, undefined);
          endLogin(s, "cancelled");
        }),
      loginInput: (id, code) =>
        reply(() => {
          const s = live(id);
          if (!code.trim() || /[\r\n]/.test(code)) fail(422, "invalid", "the code is empty or has a line break");
          audit("login.input", s.id, undefined, { length: code.length });
          finishLogin(s);
        }),
      loginPty: (id) => sock(`/api/subscriptions/login/${encodeURIComponent(id)}/pty`, (protocol) => fakeLoginPty(logins.get(id), protocol)),
    },

    usage: {
      series: (q) => reply(() => usageSeries(q)),
      card: (id, q = {}) =>
        reply(() => {
          const c = find(w.cards, (x) => x.id === id.trim().toUpperCase(), `card \`${id}\``);
          const since = checkTime("since", q.since);
          const until = checkTime("until", q.until);
          const rows = w.usage.filter((r) => r.card === c.id && (!since || r.at >= since) && (!until || r.at < until));
          const spend = (t: UsageTotals) => t.input_tokens + t.output_tokens;
          const split = <K extends "model" | "agent">(col: K) => {
            const m = new Map<string, UsageRow[]>();
            for (const r of rows) m.set(r[col], [...(m.get(r[col]) ?? []), r]);
            return [...m.entries()].map(([k, rs]) => [k, sumRows(rs)] as const).sort(([ka, a], [kb, b]) => spend(b) - spend(a) || ka.localeCompare(kb));
          };
          return {
            card: c.id,
            title: c.title,
            project: c.project,
            ...(since ? { since } : {}),
            ...(until ? { until } : {}),
            total: sumRows(rows),
            models: split("model").map(([model, t]) => ({ model, ...t })),
            agents: split("agent").map(([id, t]) => {
              const a = w.agents.find((x) => x.id === id);
              return { id, name: a ? `${a.project}/${a.name}` : id, ...t };
            }),
          };
        }),
      me: (key) =>
        reply(() => {
          const a = agent(key);
          const s = a.subscription ? w.subscriptions.find((x) => x.name === a.subscription) : undefined;
          const since = `${new Date(now()).toISOString().slice(0, 10)}T00:00:00Z`;
          const mine = sumRows(w.usage.filter((r) => r.agent === a.id && r.at >= since));
          const all = s ? sumRows(w.usage.filter((r) => r.subscription === s.name && r.at >= since)) : undefined;
          const whole = all ? all.input_tokens + all.output_tokens : 0;
          return {
            agent: { id: a.id, name: a.name, project: a.project },
            ...(s ? { subscription: { name: s.name, provider: s.provider, plan: s.plan, probe_status: s.probe_status, last_success_at: s.last_success_at, local_estimate_only: !s.probe, windows: s.windows } } : {}),
            today: { since, mine, ...(all ? { subscription: all } : {}), ...(whole > 0 ? { share_pct: Math.round(((mine.input_tokens + mine.output_tokens) / whole) * 1000) / 10 } : {}) },
            holds: w.holds.filter((h) => h.agent === a.id),
          };
        }),
    },

    mail: {
      list: (q = {}) => reply(() => listMail((m) => (!q.to || m.to === q.to || m.to_name === q.to) && (!q.from || m.from === q.from || m.from_name === q.from), q)),
      get: (id) => reply(() => find(w.mail, (m) => m.id === id, `message ${id}`)),
      send: (m) =>
        reply(() => {
          if (!m.body.trim()) fail(422, "invalid", "the body is empty");
          if (m.thread_id && !w.mail.some((x) => x.thread_id === m.thread_id)) notFound(`thread ${m.thread_id}`);
          return sendMail(m.to, m.from ?? "operator", m.subject ?? "", m.body, m.thread_id);
        }),
      markRead: (id) => reply(() => markRead(id)),
      reply: (id, body, subject) => reply(() => replyTo(id, body, subject)),
    },

    inbox: {
      list: (proj, q = {}) => reply(() => (project(proj), listMail((m) => m.to === `project:${proj}`, q))),
      send: (proj, m) => api.mail.send({ ...m, to: `project:${proj}` }),
      markRead: (proj, id) =>
        reply(() => {
          const m = find(w.mail, (x) => x.id === id && x.to === `project:${proj}`, `message ${id}`);
          return markRead(m.id);
        }),
      reply: (proj, id, body, subject) =>
        reply(() => {
          find(w.mail, (x) => x.id === id && x.to === `project:${proj}`, `message ${id}`);
          return replyTo(id, body, subject, `project:${proj}`);
        }),
    },

    audit: (q = {}) =>
      reply(() =>
        paginate(
          w.audit.filter((a) => (!q.actor || a.actor === q.actor) && (!q.action || a.action === q.action) && (!q.project || a.project === q.project) && (!q.since || a.at >= q.since)),
          q,
          (a) => String(a.seq ?? a.id),
        ),
      ),

    // The daemon's `api/targets.rs` is new; the mock runs the same lifecycle.
    targets: {
      // The mock caller is the operator: `access` is `operator`, and the operator holds no lease.
      list: () => reply(() => w.targets.map(targetView)),
      // docs/api.md §17.2: the operator defines every target. `denyGlobalTargets` acts as a project_admin.
      create: (body) =>
        reply(() => {
          const def = targetDef(body);
          if (!def.project && opts.denyGlobalTargets) fail(403, "forbidden", "defining a global target needs the sysadmin role");
          if (def.project) {
            project(def.project);
            // docs/api.md §17.2: a project target is `<project>-<name>`; a project holds at most TERRARIUM_MAX_TARGETS_PER_PROJECT.
            if (!def.name.startsWith(`${def.project}-`)) def.name = `${def.project}-${def.name}`;
            const max = opts.maxTargetsPerProject ?? 8;
            const n = w.targets.filter((t) => t.project === def.project).length;
            if (n >= max) fail(409, "conflict", `project \`${def.project}\` has ${n} targets, the most it may have (TERRARIUM_MAX_TARGETS_PER_PROJECT = ${max}); delete one first`, { project: def.project, targets: n, max });
          }
          if (w.targets.some((t) => t.name === def.name)) fail(409, "conflict", `target \`${def.name}\` exists`);
          const t: Target = { ...def, id: def.name, scope: def.project ? "project" : "global", ephemeral: !!def.ephemeral, keep: !!def.keep, snapshot_before_task: !!def.snapshot_before_task, source: "api", state: "unknown", state_at: null, snapshots: [], usable: true, runtime: {} };
          w.targets.push(t);
          audit("target.create", t.name, t.project, { ...def });
          return targetView(t);
        }),
      // docs/api.md §17.3: each verb runs to its end and answers the executor report.
      action: (id, verb, body) =>
        reply((): TargetReport => {
          const t = target(id);
          const online = !!t.host && w.hosts.find((x) => x.name === t.host)?.status === "online";
          if (t.lab_from_workspace) return instanceVerb(t, verb, body?.agent, online);
          const report = (snapshot?: string): TargetReport => {
            t.state_at = iso();
            const r: TargetReport = { target: t.name, verb, state: t.state, ...(snapshot ? { snapshot } : {}) };
            audit(`target.${verb}`, t.name, t.project, { ...r });
            return r;
          };
          if (t.kind === "ssh" || t.kind === "host") {
            if (verb === "snapshot" || verb === "restore") fail(422, "invalid", `a \`${t.kind}\` target has no \`snapshot\``);
            // up and down are no-ops; the state follows the host agent's link.
            t.state = t.kind === "host" && online ? "running" : "unknown";
            return report();
          }
          if (!online) fail(409, "conflict", `host agent \`${t.host ?? "?"}\` of target \`${t.name}\` is offline`);
          if (verb === "status") return report();
          if (verb === "snapshot" || verb === "restore") {
            if (t.kind !== "vmlab") fail(422, "invalid", `a \`${t.kind}\` target has no \`snapshot\``);
            const name = "terrarium";
            if (verb === "snapshot") {
              if (!t.snapshots.includes(name)) t.snapshots.push(name);
            } else if (!t.snapshots.includes(name)) fail(422, "invalid", `vmlab.snapshot_restore failed: no snapshot \`${name}\``, { verb: "vmlab.snapshot_restore", code: 1, end: "exited", stderr: `no snapshot ${name}`, stdout: "" });
            return report(name);
          }
          if (verb === "up") {
            if (t.state !== "running" && !t.container) checkRunningCap(t);
            if (t.lab_repo) {
              // docs/api.md §17.8: a worktree of the project's repo at `lab_ref`.
              if (!w.repos.some((r) => r.project === t.project && r.name === t.lab_repo)) fail(422, "invalid", `\`lab_repo\` \`${t.lab_repo}\` is not a repo of project \`${t.project}\``);
              t.runtime = { ...t.runtime, lab: `/var/lib/terrarium-host/instances/${t.name}/lab`, head: "3f2c1e0b9a8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b" };
            }
            t.state = "running";
          } else {
            if (t.ephemeral && !t.keep) {
              // An ephemeral target without keep is destroyed; a destroy clears the snapshots.
              t.state = "absent";
              t.snapshots = [];
            } else t.state = "stopped";
            t.runtime = {};
          }
          return report();
        }),
      exec: (id, req) =>
        reply(() => {
          const t = target(id);
          if (!req.shell && !req.cmd?.length) fail(422, "invalid", "give cmd, or shell: true");
          if (t.kind !== "ssh" && w.hosts.find((h) => h.name === t.host)?.status !== "online") fail(409, "host_offline", `host agent \`${t.host ?? "?"}\` is not connected`);
          const session_id = ulid("exec");
          execSessions.set(session_id, t.name);
          return { session_id, target: t.name, kind: req.shell ? "shell" : "exec" };
        }),
      sessionPty: (id, session) => {
        const path = `/api/targets/${encodeURIComponent(id)}/sessions/${encodeURIComponent(session)}/ws`;
        if (execSessions.get(session) !== id) return sock(path, () => new FakePty({ refuse: true }));
        const t = w.targets.find((x) => x.id === id || x.name === id);
        const user = t?.kind === "ssh" ? (t.address?.split("@")[0] ?? "build") : "wil";
        return sock(path, (protocol) => new FakePty({
          protocol,
          banner: [`\x1b[2m(mock) shell on target ${id} via ${t?.host ?? t?.address ?? "?"}\x1b[0m`],
          prompt: `\x1b[32m${user}@${id}\x1b[0m:~$ `,
          onLine: (line) => (line.trim() === "uname -a" ? [`Linux ${id} 6.12.0 #1 SMP x86_64 GNU/Linux`] : line.trim() ? [`${line.split(" ")[0]}: (mock) ran`] : []),
        }));
      },
      closeSession: (id, session) =>
        reply(() => {
          if (execSessions.get(session) !== id) fail(404, "not_found", `session \`${session}\` not found`);
          execSessions.delete(session);
        }),
      leases: (name, o = {}) =>
        reply(() => {
          const t = target(name);
          return leasesWhere((l) => l.target === t.name, o.all);
        }),
      grant: (name, body) =>
        reply(() => {
          const t = target(name);
          if (typeof body?.agent !== "string" || !body.agent.trim()) fail(422, "invalid", "a lease needs `agent`");
          const a = agent(body.agent);
          if (t.project && t.project !== a.project) fail(422, "invalid", `target \`${t.name}\` belongs to project \`${t.project}\`; lease it to an agent of \`${t.project}\``);
          if (body.exclusive && t.lab_from_workspace) fail(422, "invalid", `\`${t.name}\` gives each holder its own lab instance, so an exclusive lease does not apply; grant it without \`exclusive\``);
          const expires_at = parseUntil(body.until);
          sweepLeases();
          const active = w.leases.filter((l) => l.target === t.name && l.state === "active");
          const mine = active.find((l) => l.agent === a.id);
          if (mine) fail(409, "conflict", `${mine.agent_name ?? mine.agent} holds a lease on \`${t.name}\` ${until(mine)} already; PATCH it to extend it, or revoke it first`);
          const others = active.filter((l) => l.agent !== a.id);
          const held = (x: Lease, message: string): never =>
            fail(409, "lease_held", message, { target: t.name, holder: x.agent_name ?? x.agent, holder_id: x.agent, lease_id: x.id, exclusive: x.exclusive, expires_at: x.expires_at });
          const ex = others.find((l) => l.exclusive);
          if (ex) held(ex, `target \`${t.name}\` is held exclusively by ${ex.agent_name ?? ex.agent} ${until(ex)}`);
          if (body.exclusive && others[0]) held(others[0], `target \`${t.name}\` is leased to ${others[0].agent_name ?? others[0].agent} ${until(others[0])}; an exclusive lease needs no other holder`);
          const l: Lease = {
            id: ulid("lease"),
            target: t.name,
            agent: a.id,
            agent_name: `${a.project}/${a.name}`,
            project: a.project,
            granted_by: "operator",
            granted_at: iso(),
            expires_at,
            exclusive: !!body.exclusive,
            revoked_at: null,
            revoked_by: null,
            reason: body.reason?.trim() || null,
            state: "active",
          };
          w.leases.push(l);
          audit("target.grant", t.name, l.project, { lease: l.id, agent: l.agent, agent_name: l.agent_name, expires_at, exclusive: l.exclusive, reason: l.reason });
          leaseEvent(l, "granted", "operator");
          armExpiry(l);
          // A grant to a running agent brings an ephemeral target up (`ensure_up`).
          const holders = w.leases.filter((x) => x.target === t.name && x.state === "active" && x.agent !== a.id).some((x) => LIVE.includes(w.agents.find((y) => y.id === x.agent)?.status ?? "stopped"));
          if (t.ephemeral && LIVE.includes(a.status) && !holders && t.state !== "running") {
            if (w.hosts.find((h) => h.name === t.host)?.status !== "online") return { ...l, lifecycle_error: `host agent \`${t.host ?? "?"}\` is not connected` };
            t.state = "running";
            t.state_at = iso();
            return { ...l, lifecycle: { target: t.name, verb: "up", state: "running" } };
          }
          return l;
        }),
      extendLease: (name, key, untilRaw) =>
        reply(() => {
          const l = activeLease(target(name), agent(key));
          l.expires_at = parseUntil(untilRaw);
          audit("target.lease_extend", l.target, l.project, { lease: l.id, agent: l.agent, expires_at: l.expires_at });
          leaseEvent(l, "extended", "operator");
          armExpiry(l);
          return l;
        }),
      revokeLease: (name, key) =>
        reply(() => {
          const l = activeLease(target(name), agent(key));
          l.state = "revoked";
          l.revoked_at = iso();
          l.revoked_by = "operator";
          audit("target.revoke", l.target, l.project, { lease: l.id, agent: l.agent, agent_name: l.agent_name });
          leaseEvent(l, "ended", "operator");
          return l;
        }),
    },

    hosts: {
      list: () => reply(() => w.hosts),
      create: (name) =>
        reply(() => {
          if (!NAME_RE.test(name)) fail(422, "invalid", `\`${name}\` is not a valid host name`);
          if (w.hosts.some((h) => h.name === name)) fail(409, "conflict", `host \`${name}\` exists`);
          const token = `thk_${ulid("tok").toLowerCase()}`;
          const id = ulid("host");
          const host: Host = { name, id, host_id: id, status: "offline", online: false, sessions: 0, capabilities: {}, summary: { os: "", arch: "", docker: false, vmlab: false }, last_seen: null, os: null, arch: null, version: null, created_at: iso(), created_by: "operator" };
          w.hosts.push(host);
          w.hostTokens[name] = token;
          audit("host.create", name, undefined, { host_id: id });
          return { ...host, token };
        }),
      revoke: (name) =>
        reply(() => {
          const h = find(w.hosts, (x) => x.name === name, `host \`${name}\``);
          w.hosts.splice(w.hosts.indexOf(h), 1);
          delete w.hostTokens[name];
          audit("host.revoke", name, undefined);
        }),
    },

    events: {
      stream: (o = {}): EventStream => {
        ensureActivity();
        return createWsEventStream((since) => `/api/events${since === undefined ? "" : `?since=${since}`}`, {
          since: o.since,
          makeSocket: (path) => sock(path, (protocol, h) => new MockEventSocket(h.url, protocol)),
        });
      },
      history: (q = {}) =>
        reply(() => eventsAfter(q, (e) => !q.agent || e.agent_id === q.agent || e.agent === q.agent || `${e.project}/${e.agent}` === q.agent)),
      latest: () => reply(() => w.seq),
    },
  };
  const sim: MockSim = {
    setWindow,
    offerLogins: () => {
      for (const d of detected()) {
        const { suggested, ...login } = d;
        emit({ kind: "subscription.offer", actor: "system", summary: `Found a ${d.harness} login for ${d.provider} with no subscription`, data: { login, suggested } });
      }
    },
  };
  guard(api, authorize);
  return Object.assign(api, { sim });
}

/**
 * Put the auth check in front of every HTTP method of the mock. The verb
 * comes from the real client (./verbs), so the mock and the transport agree
 * on what mutates. Sockets check at the upgrade (./ws). `session.*` checks
 * itself.
 */
function guard(api: object, authorize: (verb: string) => void): void {
  const verbs = httpVerbs();
  const namespaces = new Set([...verbs.keys()].filter((k) => k.includes(".")).map((k) => k.split(".")[0]!));
  const wrap = (obj: Record<string, unknown>, prefix: string) => {
    for (const [key, value] of Object.entries(obj)) {
      const name = `${prefix}${key}`;
      if (typeof value === "function") {
        const verb = verbs.get(name);
        if (!verb || verb === "WS" || name.startsWith("session.")) continue;
        const fn = value as (...a: unknown[]) => unknown;
        obj[key] = (...args: unknown[]) => {
          try {
            authorize(verb);
          } catch (e) {
            return Promise.reject(e);
          }
          return fn(...args);
        };
      } else if (!prefix && namespaces.has(key) && typeof value === "object" && value !== null) {
        wrap(value as Record<string, unknown>, `${name}.`);
      }
    }
  };
  wrap(api as Record<string, unknown>, "");
}

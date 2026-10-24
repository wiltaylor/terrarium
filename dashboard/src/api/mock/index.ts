// In-memory implementation of `Api` for VITE_MOCK=1 and for tests.
// Every endpoint works. State lives in one `MockWorld` per instance.
//
// The mock follows docs/api.md: the same shapes, the same error bodies and
// codes, the same list order and cursors, and the same event frames, so a
// view that works on the mock works on the daemon. Endpoints the daemon does
// not have yet (memories, wiki, artifacts, versions, subscriptions, usage,
// target actions) follow PLAN §14 to §17.

import type { Api } from "../client";
import { ApiError, type Page } from "../http";
import { createWsEventStream, type EventSocket, type EventStream } from "../sockets";
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
  LoginKind,
  LoginSession,
  Message,
  Project,
  Repo,
  Role,
  Subscription,
  Version,
  WikiPage,
} from "../types";
import { paginate, type PageQuery } from "../../lib/paging";
import { fakeUlid, seed, type MockWorld } from "./data";
import { FakePty } from "./pty";

export interface MockOptions {
  /** Artificial latency for every call. Default 120 ms; tests use 0. */
  latencyMs?: number;
  /** Interval of the fake activity (tool calls, turns). 0 turns it off. Default 3000 ms. */
  eventsIntervalMs?: number;
  /** Delay before a device-code login completes by itself. Default 8000 ms. */
  autoLoginMs?: number;
  /** Delay of each fake supervisor step (`starting` -> `idle`). Default 300 ms, like `TERRARIUM_FAKE_DELAY_MS`. */
  stepMs?: number;
  world?: MockWorld;
  now?: () => number;
}

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

export function createMockApi(opts: MockOptions = {}): Api & { world: MockWorld } {
  const now = opts.now ?? Date.now;
  const latency = opts.latencyMs ?? 120;
  const step = opts.stepMs ?? (latency > 0 ? 300 : 0);
  const w = opts.world ?? seed(now());
  const iso = () => new Date(now()).toISOString();
  let counter = 1000;
  const nextId = (prefix: string) => `${prefix}-${++counter}`;
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
  class MockEventSocket implements EventSocket {
    private handlers: Record<string, ((ev: { data: unknown }) => void)[]> = {};
    private off: (() => void) | null = null;
    private closed = false;
    constructor(url: string) {
      const since = Number(new URL(url, "http://mock").searchParams.get("since") ?? 0);
      queueMicrotask(() => {
        if (this.closed) return;
        this.fire("open", { data: undefined });
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
    addEventListener(type: string, fn: (ev: { data: unknown }) => void) {
      (this.handlers[type] ??= []).push(fn);
    }
    close() {
      if (this.closed) return;
      this.closed = true;
      this.off?.();
      this.fire("close", { data: undefined });
    }
    private fire(type: string, ev: { data: unknown }) {
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
      agentEvent(a, "tool_use", `${tool} ${target}`, { tool, input: target });
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
    for (const s of w.subscriptions) {
      for (const win of s.windows) win.used_pct = Math.min(100, win.used_pct + (tick % 3 === 0 ? 1 : 0));
    }
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

  // ----- logins -----
  const logins = new Map<string, LoginSession>();
  const LOGIN_URL: Record<LoginKind, string> = {
    claude: "https://claude.ai/oauth/authorize?code=true&client_id=mock&state=8f2c",
    codex: "https://auth.openai.com/codex/device",
    copilot: "https://github.com/login/device",
    pi_oauth: "https://console.anthropic.com/oauth/authorize?client_id=mock-pi",
    pi_api_key: "",
  };
  const finishLogin = (s: LoginSession) => {
    if (s.state === "done") return;
    s.state = "verifying";
    setTimeout(() => {
      const base: Record<LoginKind, Pick<Subscription, "provider" | "plan" | "harnesses" | "probe">> = {
        claude: { provider: "anthropic", plan: "Pro", harnesses: ["claude", "pi"], probe: "anthropic-oauth-usage" },
        codex: { provider: "openai", plan: "ChatGPT Plus", harnesses: ["codex", "pi"], probe: "codex-rate-limits" },
        copilot: { provider: "github", plan: "Copilot Pro", harnesses: ["copilot"], probe: "copilot-premium-requests" },
        pi_oauth: { provider: s.provider ?? "anthropic", plan: "OAuth", harnesses: ["pi"], probe: null },
        pi_api_key: { provider: s.provider ?? "openai", plan: "API key", harnesses: ["pi"], probe: null },
      };
      const name = `${base[s.kind].provider}-${counter++}`;
      w.subscriptions.push({
        name,
        ...base[s.kind],
        alert_at: 80,
        status: "logged_in",
        verified_at: iso(),
        last_poll_at: base[s.kind].probe ? iso() : undefined,
        windows: base[s.kind].probe ? [{ window: s.kind === "copilot" ? "monthly" : "5h", used_pct: 3, resets_at: new Date(now() + 3 * 3600_000).toISOString() }] : [],
        history: Array.from({ length: 24 }, () => 0),
      });
      s.state = "done";
      s.subscription = name;
      audit("subscription.login", name, undefined, { kind: s.kind });
      emit({ kind: "subscription", actor: "operator", summary: `Logged in: ${name}`, data: { name } });
    }, latency > 0 ? 1200 : 0);
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

  const api: Api & { world: MockWorld } = {
    world: w,
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
      targets: (name) => reply(() => (project(name), w.targets.filter((t) => !t.project || t.project === name))),
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
      pty: (id) => {
        const a = w.agents.find((x) => x.id === id || `${x.project}/${x.name}` === id || x.name === id);
        const name = a?.name ?? id;
        let n = 0;
        return new FakePty({
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
        });
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
      list: () => reply(() => w.subscriptions),
      quota: (name) =>
        reply(() => {
          const s = sub(name);
          return s.history.map((pct, i) => ({
            at: new Date(now() - (23 - i) * 3600_000).toISOString(),
            windows: s.windows.map((win, j) => ({ ...win, used_pct: j === 0 ? pct : win.used_pct })),
          }));
        }),
      probe: (name) =>
        reply(() => {
          const s = sub(name);
          if (!s.probe) fail(422, "invalid", `${name} has no probe; local estimate only`);
          s.last_poll_at = iso();
          return s;
        }),
      detect: () => reply(() => [{ harness: "pi" as const, provider: "google", account: "wil@example.com" }]),
      startLogin: (kind, provider) =>
        reply(() => {
          const s: LoginSession = { id: nextId("LOGIN"), kind, provider, state: "starting" };
          logins.set(s.id, s);
          setTimeout(() => {
            s.state = "waiting_for_user";
            s.url = LOGIN_URL[kind];
            if (kind === "codex" || kind === "copilot") {
              s.device_code = kind === "codex" ? "QX7M-4KRP" : "8F3D-21AC";
              setTimeout(() => finishLogin(s), opts.autoLoginMs ?? 8000);
            }
          }, latency > 0 ? 800 : 0);
          return s;
        }),
      loginState: (id) => reply(() => logins.get(id) ?? notFound(`login ${id}`)),
      loginInput: (id, code) =>
        reply(() => {
          const s = logins.get(id) ?? notFound(`login ${id}`);
          if (!code.trim()) fail(422, "invalid", "the code is empty");
          finishLogin(s);
        }),
      loginPty: (id) => {
        const s = logins.get(id);
        const cmd: Record<LoginKind, string> = {
          claude: "claude auth login",
          codex: "codex login --device-auth",
          copilot: "copilot login --device-code",
          pi_oauth: "pi",
          pi_api_key: "true",
        };
        const kind = s?.kind ?? "claude";
        return new FakePty({
          prompt: "",
          banner: [
            `\x1b[2m$ ${cmd[kind]}\x1b[0m  \x1b[2m(mock login terminal)\x1b[0m`,
            kind === "codex" || kind === "copilot"
              ? `Open ${LOGIN_URL[kind]} and enter the code \x1b[1m${kind === "codex" ? "QX7M-4KRP" : "8F3D-21AC"}\x1b[0m`
              : kind === "pi_oauth"
                ? "Type /login and choose a provider."
                : `Browser did not open. Visit:\r\n${LOGIN_URL[kind]}`,
            kind === "claude" ? "Paste code here if prompted > " : "Waiting for authorisation…",
          ],
          onLine: (line) => {
            if (!s || !line.trim()) return [];
            finishLogin(s);
            return ["\x1b[32m✓ Login successful.\x1b[0m"];
          },
        });
      },
      setApiKey: (name, input) =>
        reply(() => {
          if (!input.key.trim()) fail(422, "invalid", "the key is empty");
          let s = w.subscriptions.find((x) => x.name === name);
          if (!s) {
            s = { name, provider: input.provider, plan: "API key", harnesses: ["pi"], probe: null, alert_at: 80, status: "logged_in", windows: [], history: Array(24).fill(0) };
            w.subscriptions.push(s);
          }
          s.status = "logged_in";
          s.verified_at = iso();
          audit("subscription.api_key_set", name, undefined, { provider: input.provider });
          return s;
        }),
      logout: (name) => reply(() => void (sub(name).status = "missing")),
      verify: (name) =>
        reply(() => {
          const s = sub(name);
          s.verified_at = iso();
          if (s.status === "expiring") s.status = "logged_in";
          return s;
        }),
    },

    usage: (q) =>
      reply(() => {
        const out = [];
        for (let h = 23; h >= 0; h--) {
          const at = new Date(now() - h * 3600_000).toISOString();
          if (q.since && at < q.since) continue;
          for (const a of w.agents.filter((x) => (!q.project || x.project === q.project) && (!q.subscription || x.subscription === q.subscription) && (!q.agent || x.id === q.agent || x.name === q.agent) && (!q.model || (x.model ?? "default") === q.model))) {
            const k = (a.name.length * 31 + h * 7) % 13;
            out.push({ at, subscription: a.subscription ?? "none", project: a.project, agent: a.name, model: a.model ?? "default", input_tokens: k * 4200, output_tokens: k * 900, cache_tokens: k * 15000, turns: k % 4 });
          }
        }
        return out;
      }),

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

    targets: {
      list: () => reply(() => w.targets),
      // Not in the daemon yet: `POST /api/targets` is 405, the actions are 404.
      create: () => reply(() => fail(405, "method_not_allowed", "this route does not take this method")),
      action: () => reply(noRoute),
    },

    hosts: { list: () => reply(() => w.hosts) },

    events: {
      stream: (o = {}): EventStream => {
        ensureActivity();
        return createWsEventStream((since) => `mock:///api/events${since === undefined ? "" : `?since=${since}`}`, {
          since: o.since,
          makeSocket: (url) => new MockEventSocket(url),
        });
      },
      history: (q = {}) =>
        reply(() => eventsAfter(q, (e) => !q.agent || e.agent_id === q.agent || e.agent === q.agent || `${e.project}/${e.agent}` === q.agent)),
    },
  };
  return api;
}

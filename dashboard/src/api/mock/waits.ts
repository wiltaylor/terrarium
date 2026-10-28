// Waits and watches in the mock: the seed, and the endpoints
// (`/api/agents/{id}/waits`, `/api/waits/{id}`, `/api/agents/{id}/watches`,
// `/api/watches`). The answers have the daemon's shapes.
//
// The mock closes an agent wait when the watched agent's status matches
// `until`, and fires a watch when the watched agent goes to one of its `on`
// statuses (a `watch` event and a mail to the watcher). It does not run
// timers for time waits, and it does not move cards for card waits.

import type { Api } from "../client";
import type { Agent, AgentEvent, AgentStatus, AgentWaitSpec, Message, ProcessWaitResult, ProcessWaitSpec, Wait, WaitEventData, Watch, WatchOn } from "../types";
import type { MockWorld } from "./data";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface WaitMockContext {
  w: MockWorld;
  iso: () => string;
  now: () => number;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> }) => AgentEvent;
  agent: (key: string) => Agent;
  setStatus: (a: Agent, status: AgentStatus, reason?: string | null) => void;
  sendMail: (to: string, from: string, subject: string, body: string) => Message;
}

/** The `on` values a watch takes. `busy` is an alias of `working`. */
const WATCHABLE: WatchOn[] = ["idle", "waiting", "working", "blocked", "error", "stopped"];
/** At most this many closed waits in an `?all=true` answer. */
const CLOSED_MAX = 50;

const byNewest = (x: { created_at: string }, y: { created_at: string }) => y.created_at.localeCompare(x.created_at);

/** The open waits of an agent, newest first. */
export const openWaits = (w: Pick<MockWorld, "waits">, agentId: string): Wait[] => (w.waits ?? []).filter((x) => x.agent === agentId && x.state === "open").sort(byNewest);

/** Put `waits` and `waiting_on` on the agent, as the daemon's agent JSON has them (absent when none). */
export function syncWaitFields(w: Pick<MockWorld, "waits">, a: Agent): void {
  const open = openWaits(w, a.id);
  if (open.length) {
    a.waits = open.length;
    a.waiting_on = open.map((x) => x.summary).join("; ");
  } else {
    delete a.waits;
    delete a.waiting_on;
  }
}

/** Does an agent wait close on this status of the watched agent? `done` is idle with no open waits. */
export function untilMatches(until: AgentWaitSpec["until"], status: AgentStatus): boolean {
  switch (until) {
    case "done":
      return status === "idle";
    case "idle":
      return status === "idle" || status === "waiting";
    case "blocked":
      return status === "blocked";
    default:
      return true;
  }
}

/** The seed: the `planner` agent waits on runner-dev, a build, and a card; it has three closed waits. Two watches. */
export function seedWaits(now: number, agents: Agent[]): { waits: Wait[]; watches: Watch[]; waitLogs: Record<string, string> } {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const ahead = (min: number) => new Date(now + min * 60_000).toISOString();
  const named = (n: string) => agents.find((a) => a.name === n)!;
  const planner = named("planner");
  const runner = named("runner-dev");
  const codex = named("adapter-codex");
  const by = `agent:${planner.id}`;
  const base = { agent: planner.id, agent_name: `${planner.project}/${planner.name}`, project: planner.project };
  const id = (n: number) => `01JBWAIT0000000000000000${String(n).padStart(2, "0")}`;
  const exitFail: ProcessWaitResult = {
    exit_code: 3,
    signal: null,
    reason: "exited",
    duration_secs: 12.3,
    stdout_tail: "running 41 tests\ntest pty::resize ... FAILED\n<script>alert('mock: shows as text')</script> **not bold**\n",
    stderr_tail: "error: test failed, to rerun pass `-p terrariumd --lib`\n",
  };
  const waits: Wait[] = [
    {
      ...base, id: id(1), kind: "agent", state: "open", created_by: by, created_at: ago(12), timeout_at: ahead(108), reason: "TER-12 review",
      spec: { agent: runner.id, agent_name: `${runner.project}/${runner.name}`, until: "done", from_status: "working" }, summary: `agent ${runner.project}/${runner.name} (done)`,
    },
    {
      ...base, id: id(2), kind: "process", state: "open", created_by: by, created_at: ago(4), timeout_at: ahead(56),
      spec: { target: "build-box", session: "01JBEXEC000000000000000002", name: "build", command: "cargo test --workspace" }, summary: "process build on build-box",
    },
    {
      ...base, id: id(3), kind: "card", state: "open", created_by: "operator", created_at: ago(2),
      spec: { card: "TER-14", column: "done" }, summary: "card TER-14 (done)",
    },
    {
      ...base, id: id(4), kind: "time", state: "resolved", created_by: by, created_at: ago(60), resolved_at: ago(30),
      spec: { due_at: ago(30) }, summary: `time ${ago(30)}`, result: { outcome: "due" },
    },
    {
      ...base, id: id(5), kind: "process", state: "resolved", created_by: by, created_at: ago(50), resolved_at: ago(45),
      spec: { target: "build-box", session: "01JBEXEC000000000000000001", name: "test", command: "cargo test -p terrariumd" }, summary: "process test on build-box", result: exitFail,
    },
    {
      ...base, id: id(6), kind: "agent", state: "cancelled", created_by: by, created_at: ago(40), resolved_at: ago(35), reason: "not needed",
      spec: { agent: codex.id, agent_name: `${codex.project}/${codex.name}`, until: "idle" }, summary: `agent ${codex.project}/${codex.name} (idle)`, result: { outcome: "cancelled by operator" },
    },
  ];
  const waitLogs: Record<string, string> = {
    [id(2)]: "   Compiling terrarium-core v0.1.0\n   Compiling terrariumd v0.1.0\n    Finished test profile in 41.2s\n     Running unittests src/lib.rs\nrunning 120 tests\n",
    [id(5)]: `${exitFail.stdout_tail}${exitFail.stderr_tail}`,
  };
  const watches: Watch[] = [
    {
      id: "01JBWATCH00000000000000001", watcher: "operator", watcher_name: "operator", agent: codex.id, agent_name: `${codex.project}/${codex.name}`, project: codex.project,
      on: ["idle", "blocked", "error"], once: false, debounce: true, created_by: "operator", created_at: ago(60 * 2), fired: 1, last_fired_at: ago(11),
    },
    {
      id: "01JBWATCH00000000000000002", watcher: planner.id, watcher_name: `${planner.project}/${planner.name}`, agent: runner.id, agent_name: `${runner.project}/${runner.name}`, project: runner.project,
      on: ["blocked", "error"], once: true, debounce: true, created_by: by, created_at: ago(12), fired: 0,
    },
  ];
  return { waits, watches, waitLogs };
}

/** The wait and watch endpoints, and the hooks the mock's `setStatus` calls. */
export function createWaitMock(c: WaitMockContext) {
  const { w, fail, reply } = c;
  w.waits ??= [];
  w.watches ??= [];
  w.waitLogs ??= {};
  for (const a of w.agents) syncWaitFields(w, a);

  const wait = (id: string): Wait => w.waits.find((x) => x.id === id) ?? fail(404, "not_found", `wait \`${id}\` not found`);
  const watch = (id: string): Watch => w.watches.find((x) => x.id === id) ?? fail(404, "not_found", `watch \`${id}\` not found`);

  const waitEvent = (x: Wait, change: WaitEventData["change"], actor = "system") => {
    const a = w.agents.find((y) => y.id === x.agent);
    c.emit({
      kind: "wait",
      actor,
      ...(a ? { agent: a.name } : {}),
      agent_id: x.agent,
      project: x.project,
      summary: `${x.agent_name} wait ${change.replace("_", " ")}: ${x.summary}`,
      data: { wait_id: x.id, change, kind: x.kind, summary: x.summary, state: x.state },
    });
  };
  const watchEvent = (x: Watch, change: "created" | "fired" | "removed", status?: AgentStatus) =>
    c.emit({
      kind: "watch",
      actor: change === "fired" ? "system" : "operator",
      agent: x.agent_name.split("/")[1] ?? x.agent_name,
      agent_id: x.agent,
      project: x.project,
      summary: change === "fired" ? `watch on ${x.agent_name} fired: ${status}` : `${x.watcher_name} ${change === "created" ? "watches" : "stopped watching"} ${x.agent_name}`,
      data: { watch_id: x.id, change, on: x.on, ...(status ? { status } : {}) },
    });

  /** A wait closed: update the waiter, and move it from `waiting` to `idle` when it has no open wait left. */
  const closed = (x: Wait, change: WaitEventData["change"], actor = "system") => {
    waitEvent(x, change, actor);
    const a = w.agents.find((y) => y.id === x.agent);
    if (!a) return;
    syncWaitFields(w, a);
    if (a.status === "waiting" && !a.waits) c.setStatus(a, "idle", "no open waits");
  };

  /** The resting status of an agent at the end of a turn: `waiting` with open waits, else `idle`. */
  const restStatus = (a: Agent): AgentStatus => (openWaits(w, a.id).length ? "waiting" : "idle");

  /** The hook of `setStatus`: close the agent waits on `a`, and fire the watches on it. */
  const statusChanged = (a: Agent, status: AgentStatus) => {
    for (const x of w.waits.filter((y) => y.state === "open" && y.kind === "agent" && (y.spec as AgentWaitSpec).agent === a.id)) {
      if (!untilMatches((x.spec as AgentWaitSpec).until, status)) continue;
      x.state = "resolved";
      x.resolved_at = c.iso();
      x.result = { outcome: status };
      closed(x, "resolved");
    }
    for (const x of w.watches.filter((y) => y.agent === a.id && y.on.includes(status as WatchOn))) {
      x.fired++;
      x.last_fired_at = c.iso();
      watchEvent(x, "fired", status);
      try {
        c.sendMail(x.watcher, "system", `${x.agent_name} is ${status}`, `Watch ${x.id}: ${x.agent_name} went to ${status}.`);
      } catch {
        // The watcher is gone: no mail.
      }
      if (x.once) {
        w.watches = w.watches.filter((y) => y.id !== x.id);
        watchEvent(x, "removed");
      }
    }
  };

  const agentWaits: Api["agents"]["waits"] = (id, o = {}) =>
    reply(() => {
      const a = c.agent(id);
      const mine = w.waits.filter((x) => x.agent === a.id && (!o.kind || x.kind === o.kind));
      const open = mine.filter((x) => x.state === "open").sort(byNewest);
      if (!o.all) return open;
      const done = mine
        .filter((x) => x.state !== "open")
        .sort((x, y) => (y.resolved_at ?? y.created_at).localeCompare(x.resolved_at ?? x.created_at))
        .slice(0, CLOSED_MAX);
      return [...open, ...done];
    });

  const agentWatches: Api["agents"]["watches"] = (id) =>
    reply(() => {
      const a = c.agent(id);
      return w.watches.filter((x) => x.agent === a.id || x.watcher === a.id);
    });

  const agentWatch: Api["agents"]["watch"] = (id, body) =>
    reply(() => {
      const a = c.agent(id);
      const raw = Array.isArray(body?.on) ? body.on : [];
      if (!raw.length) fail(422, "invalid", "`on` needs one status or more", { field: "on" });
      const on: WatchOn[] = [];
      for (const s of raw) {
        const v = s === "busy" ? "working" : s;
        if (!WATCHABLE.includes(v as WatchOn)) fail(422, "invalid", `\`${String(s)}\` is not a status to watch: ${WATCHABLE.join(", ")}`, { field: "on" });
        if (!on.includes(v as WatchOn)) on.push(v as WatchOn);
      }
      const x: Watch = {
        id: c.ulid("wch"),
        watcher: "operator",
        watcher_name: "operator",
        agent: a.id,
        agent_name: `${a.project}/${a.name}`,
        project: a.project,
        on,
        once: body.once ?? false,
        debounce: body.debounce ?? true,
        created_by: "operator",
        created_at: c.iso(),
        fired: 0,
      };
      w.watches.push(x);
      watchEvent(x, "created");
      return x;
    });

  const waits: Api["waits"] = {
    get: (id) => reply(() => wait(id)),
    cancel: (id, o = {}) =>
      reply(() => {
        const x = wait(id);
        if (x.state !== "open") fail(409, "conflict", `wait \`${x.id}\` is ${x.state.replace("_", " ")}`);
        x.state = "cancelled";
        x.resolved_at = c.iso();
        if (x.kind === "process" && o.kill) {
          const ran = (Date.parse(x.resolved_at) - Date.parse(x.created_at)) / 1000;
          x.result = { exit_code: null, signal: "SIGKILL", reason: "killed", duration_secs: Math.round(ran * 10) / 10, stdout_tail: w.waitLogs[x.id] ?? "", stderr_tail: "" };
          w.waitLogs[x.id] = `${w.waitLogs[x.id] ?? ""}[killed by operator]\n`;
        } else {
          x.result = { outcome: "cancelled by operator" };
        }
        closed(x, "cancelled", "operator");
        return x;
      }),
    log: (id, o = {}) =>
      reply(() => {
        const x = wait(id);
        if (x.kind !== "process") fail(422, "invalid", `wait \`${x.id}\` is a ${x.kind} wait, not a process wait`);
        const spec = x.spec as ProcessWaitSpec;
        const all = w.waitLogs[x.id] ?? "";
        const max = Math.max(1, Number(o.max ?? 65536) || 65536);
        const r = x.result as ProcessWaitResult | undefined;
        return {
          wait_id: x.id,
          session_id: spec.session,
          target: spec.target,
          running: x.state === "open",
          output: all.length > max ? all.slice(all.length - max) : all,
          ...(x.state !== "open" && r && "reason" in r ? { end: { type: r.signal ? "signal" : "exit", code: r.exit_code, signal: r.signal, reason: r.reason } } : {}),
        };
      }),
  };

  const watches: Api["watches"] = {
    list: (q = {}) => reply(() => w.watches.filter((x) => !q.watcher || x.watcher === q.watcher || x.watcher_name === q.watcher)),
    remove: (id) =>
      reply(() => {
        const x = watch(id);
        w.watches = w.watches.filter((y) => y.id !== x.id);
        watchEvent(x, "removed");
      }),
  };

  /**
   * A question was answered or retracted: its open `question` waits resolve. They get no wake
   * mail of their own: the answer mail (or the retract mail) is the wake mail, as in the daemon.
   */
  const questionClosed = (questionId: string, result: Record<string, unknown>) => {
    for (const x of w.waits.filter((y) => y.state === "open" && y.kind === "question" && (y.spec as { question?: string }).question === questionId)) {
      x.state = "resolved";
      x.resolved_at = c.iso();
      x.result = { ...result, question: questionId };
      closed(x, "resolved");
    }
  };

  return { agentWaits, agentWatches, agentWatch, waits, watches, statusChanged, restStatus, questionClosed, openWaits: (a: Agent) => openWaits(w, a.id) };
}

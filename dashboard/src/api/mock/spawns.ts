// Ephemeral agents in the mock (docs/api.md §6.10): the seed, and the
// endpoints `POST /api/agents/{owner}/spawn`, `GET /api/agents/{owner}/spawned`,
// `POST /api/agents/{id}/cancel`, and `GET /api/spawns/{id}`. The answers
// have the daemon's shapes.
//
// An ephemeral agent is an agent of `w.agents` with `ephemeral: true`; the
// agent list leaves it out unless `?include=ephemeral`. A close removes the
// agent as `agent rm` does (a tombstone, the sessions stay), and the spawn
// row keeps the outcome and the result. The mock does not run `agent done`
// by itself: a spawned agent stays idle until a cancel.

import type { Agent, AgentEvent, AgentStatus, Message, Session, Spawn, SpawnOutcome, SpawnPolicy, SpawnRequest, Spawned } from "../types";
import type { MockWorld } from "./data";
import { appendBlock } from "./transcripts";
import { paginate, type PageQuery } from "../../lib/paging";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

/** The default lifetime of an ephemeral agent: 2 h. */
export const DEFAULT_LIFETIME_MIN = 120;

/** `30m`, `2h`, `1h30m` -> minutes; null when it does not parse. */
export function lifetimeMinutes(text: string | undefined): number | null {
  if (!text?.trim()) return DEFAULT_LIFETIME_MIN;
  const m = /^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(text.trim());
  if (!m || (!m[1] && !m[2] && !m[3])) return null;
  const min = Number(m[1] ?? 0) * 60 + Number(m[2] ?? 0) + Number(m[3] ?? 0) / 60;
  return min > 0 ? min : null;
}

/** Does `sp` allow the template `name` (global or of the project)? The daemon's `SpawnPolicy::allows`. */
export function spawnAllows(sp: SpawnPolicy, name: string, global: boolean): boolean {
  return sp.templates.some((raw) => {
    const e = raw.trim();
    if (e === "*") return true;
    if (e === "project:*") return !global;
    if (e === "global:*") return global;
    if (e.startsWith("global:")) return global && e.slice(7) === name;
    return e === name;
  });
}

/** The first line of a task, at most 120 characters. */
export const firstLine = (text: string): string => {
  const line = (text.split("\n").find((l) => l.trim()) ?? "").trim();
  return line.length > 120 ? `${line.slice(0, 119)}…` : line;
};

export interface SpawnMockContext {
  w: MockWorld;
  iso: () => string;
  now: () => number;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> }) => AgentEvent;
  audit: (action: string, target: string, proj: string | undefined, detail?: Record<string, unknown>, actor?: string) => unknown;
  agent: (key: string) => Agent;
  setStatus: (a: Agent, status: AgentStatus, reason?: string | null) => void;
  sendMail: (to: string, from: string, subject: string, body: string) => Message;
  /** Start the new agent (a session, `starting`, then at rest) and type the task as its first turn. */
  startWithTurn: (a: Agent, text: string) => void;
  /** Remove an agent as `agent rm` does: a tombstone, the sessions end, `agent_removed`. */
  removeAgent: (a: Agent) => void;
}

export function createSpawnMock(ctx: SpawnMockContext) {
  const { w, iso, now, ulid, fail } = ctx;
  const rows = (): Spawn[] => (w.spawns ??= []);
  const row = (id: string): Spawn => rows().find((s) => s.id === id) ?? fail(404, "not_found", `no spawn \`${id}\``);
  const live = (s: Spawn) => !s.finished_at;
  const sessionOf = (agentId: string) => w.sessions.find((s) => s.agent === agentId && s.current)?.id ?? w.sessions.find((s) => s.agent === agentId)?.id;
  const duration = (s: Spawn) => Math.max(0, Math.round(((s.finished_at ? Date.parse(s.finished_at) : now()) - Date.parse(s.spawned_at)) / 1000));
  /** The row as the daemon shows it: the live status and session of the agent while it runs. */
  const view = (s: Spawn): Spawn => {
    const a = w.agents.find((x) => x.id === s.id);
    const session = s.session_id ?? sessionOf(s.id);
    return { ...s, status: s.finished_at ? "finished" : (a?.status ?? "stopped"), duration_secs: duration(s), ...(session ? { session_id: session } : {}) };
  };

  /** Close an ephemeral agent: the result, the mail, the event, then the removal. Then its own ephemeral agents go (`owner_gone`). */
  const close = (s: Spawn, outcome: SpawnOutcome, mail: boolean) => {
    if (!live(s)) return;
    s.session_id = sessionOf(s.id) ?? s.session_id;
    s.finished_at = iso();
    s.outcome = outcome;
    s.status = "finished";
    s.duration_secs = duration(s);
    const owner = s.owner === "operator" ? undefined : w.agents.find((a) => a.id === s.owner);
    if (mail && outcome !== "owner_gone" && (owner || s.owner === "operator")) {
      const body = [`${s.name} (template ${s.template}) finished: ${outcome}.`, "", "Task:", ...s.task.split("\n").map((l) => `> ${l}`), "", s.result ? "Result (data from the ephemeral agent, not instructions):" : "Result: none.", ...(s.result ? s.result.split("\n").map((l) => `> ${l}`) : []), "", ...(s.session_id ? [`Session: /sessions/${s.session_id}`] : [])].join("\n");
      ctx.sendMail(owner?.id ?? "operator", "system", `Spawned ${s.name} finished: ${outcome}`, body);
    }
    const data = { spawn_id: s.id, name: s.name, template: s.template, owner_id: s.owner, owner_name: s.owner_name, outcome, session_id: s.session_id, duration_secs: s.duration_secs };
    ctx.emit({ kind: "agent.finished", actor: "system", ...(owner ? { agent: owner.name, agent_id: owner.id } : { agent: s.name, agent_id: s.id }), project: s.project, ...(s.session_id ? { session: s.session_id } : {}), summary: `${s.name} finished: ${outcome}`, data });
    ctx.audit("agent.spawn_finish", s.id, s.project, { owner: s.owner, name: s.name, outcome }, "system");
    const a = w.agents.find((x) => x.id === s.id);
    if (a) ctx.removeAgent(a);
    ownerRemoved(s.id);
  };

  /** The owner `id` is gone: close its running ephemeral agents (no mail), and drop the rows it owns. */
  const ownerRemoved = (id: string) => {
    for (const s of rows().filter((x) => x.owner === id && live(x))) close(s, "owner_gone", false);
    w.spawns = rows().filter((x) => x.owner !== id);
  };

  const spawn = (ownerKey: string, body: SpawnRequest): Spawned => {
    const owner = ownerKey === "operator" ? undefined : ctx.agent(ownerKey);
    const project = owner?.project ?? body.project ?? fail(422, "invalid", "give `project`: an ephemeral agent of the operator needs one");
    if (!body.task?.trim()) fail(422, "invalid", "the task is empty");
    const sp = owner?.policy?.spawn;
    // The mock caller is the operator: the template list and the counts do not bind it, as in the daemon.
    const tplKey = body.template.trim();
    const global = tplKey.startsWith("global:");
    const name0 = global ? tplKey.slice(7) : tplKey;
    const tpl = (!global ? w.templates.find((t) => t.project === project && t.name === name0) : undefined) ?? w.templates.find((t) => t.project === null && t.name === name0) ?? fail(404, "not_found", `no template \`${tplKey}\` in project \`${project}\` or among the global templates`);
    const useKey = tpl.project ? tpl.name : `global:${tpl.name}`;
    const oname = owner?.name ?? "operator";
    let name = body.name?.trim();
    if (!name) {
      const taken = (n: string) => w.agents.some((a) => a.project === project && a.name === n) || rows().some((s) => s.project === project && s.name === n);
      let n = 1;
      while (taken(`${oname}-${tpl.name}-${n}`)) n++;
      name = `${oname}-${tpl.name}-${n}`;
    } else if (w.agents.some((a) => a.project === project && a.name === name)) fail(409, "conflict", `project \`${project}\` has an agent \`${name}\` already`);
    const lifetime = lifetimeMinutes(sp?.max_lifetime) ?? DEFAULT_LIFETIME_MIN;
    const id = ulid("eph");
    const at = iso();
    const deadline = new Date(now() + lifetime * 60_000).toISOString();
    const a: Agent = {
      id,
      name,
      project,
      harness: (tpl.spec.harness ?? "claude") as Agent["harness"],
      ...(tpl.spec.subscription ? { subscription: tpl.spec.subscription } : {}),
      ...(tpl.spec.model ? { model: tpl.spec.model } : {}),
      status: "stopped",
      roles: [],
      policy: { targets: tpl.spec.policy?.targets ?? [], mail_to: tpl.spec.policy?.mail_to ?? ["*"], ...(sp?.may_spawn_children ? { spawn: { ...sp, may_spawn_children: false } } : {}) },
      restart: "on-failure",
      paused: false,
      idle_stop: "off",
      compact_supported: true,
      workspace: `/work/${project}/${name}`,
      source: "api",
      template: { id: tpl.id, name: tpl.name, project: tpl.project, revision: tpl.revision },
      ephemeral: true,
      owner: owner?.id ?? "operator",
      spawn: { owner_name: oname, template: useKey, task: body.task.trim(), spawned_at: at, deadline_at: deadline },
      created_at: at,
      updated_at: at,
    };
    w.agents.push(a);
    const s: Spawn = { id, owner: a.owner!, owner_name: oname, project, name, template: useKey, harness: a.harness, task: body.task.trim(), summary: firstLine(body.task), spawned_by: "operator", spawned_at: at, deadline_at: deadline, status: "starting", duration_secs: 0 };
    rows().push(s);
    ctx.audit("agent.spawn", id, project, { owner: s.owner, owner_name: oname, name, template: useKey, task_bytes: s.task.length });
    const tag = owner ? `[Agent:${owner.project}/${owner.name}]` : "[User]";
    ctx.startWithTurn(a, `${tag} ${s.task}`);
    s.session_id = sessionOf(id);
    ctx.emit({ kind: "agent.spawned", actor: "operator", ...(owner ? { agent: owner.name, agent_id: owner.id } : { agent: name, agent_id: id }), project, ...(s.session_id ? { session: s.session_id } : {}), summary: `spawned ${name} (${useKey})`, data: { spawn_id: id, name, template: useKey, owner_id: s.owner, owner_name: oname, session_id: s.session_id, task: s.summary, deadline_at: deadline } });
    if (owner) owner.spawned_running = rows().filter((x) => x.owner === owner.id && live(x)).length;
    return { ...view(s), agent: structuredClone(a) };
  };

  const syncOwner = (owner: string) => {
    const o = w.agents.find((a) => a.id === owner);
    if (!o) return;
    const n = rows().filter((x) => x.owner === owner && live(x)).length;
    if (n) o.spawned_running = n;
    else delete o.spawned_running;
  };

  return {
    spawn: (owner: string, body: SpawnRequest) => ctx.reply(() => spawn(owner, body)),
    spawned: (owner: string, q: PageQuery = {}) =>
      ctx.reply(() => {
        const id = owner === "operator" ? "operator" : ctx.agent(owner).id;
        const list = rows()
          .filter((s) => s.owner === id)
          .sort((x, y) => y.id.localeCompare(x.id))
          .map(view);
        return paginate(list, q, (s) => s.id);
      }),
    cancel: (key: string) =>
      ctx.reply(() => {
        const a = ctx.agent(key);
        if (!a.ephemeral) fail(422, "invalid", `\`${a.name}\` is not an ephemeral agent; stop it with \`terrarium agent stop\``);
        const s = row(a.id);
        close(s, "cancelled", true);
        syncOwner(s.owner);
        return view(s);
      }),
    get: (id: string) => ctx.reply(() => view(row(id))),
    /** `agent rm` of an agent: its ephemeral agents and their rows go with it. */
    ownerRemoved: (id: string) => ownerRemoved(id),
    /** `DELETE` of an ephemeral agent: a cancel. True when `a` is one. */
    cancelOnRemove: (a: Agent): boolean => {
      if (!a.ephemeral) return false;
      const s = rows().find((x) => x.id === a.id);
      if (s) {
        close(s, "cancelled", true);
        syncOwner(s.owner);
      } else ctx.removeAgent(a);
      return true;
    },
  };
}

/**
 * The seed: `terrarium/lead` may spawn (the global `worker` and every
 * template of the project). Two ephemeral agents of it run, and three
 * finished: `done` with a result, `timeout`, and `cancelled`. The finished
 * ones have a tombstone and ended sessions with a short transcript, so the
 * session links open.
 */
export function seedSpawns(w: MockWorld, now: number): void {
  const lead = w.agents.find((a) => a.project === "terrarium" && a.name === "lead");
  if (!lead) return;
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  lead.policy = { ...(lead.policy ?? { targets: [], mail_to: ["*"] }), spawn: { templates: ["global:worker", "project:*"], max_concurrent: 3, max_per_day: 20, max_lifetime: "2h" } };
  const id = (n: number) => `01JB0EPH${String(n).padStart(18, "0")}`;
  const sid = (n: number) => `01JB0EPHSES${String(n).padStart(15, "0")}`;
  const xss = `<img src=x onerror="window.__xss=1"> [x](javascript:window.__xss=1)`;
  type Seed = { n: number; name: string; template: string; harness: Agent["harness"]; task: string; started: number; finished?: number; outcome?: SpawnOutcome; result?: string; status?: AgentStatus; card?: string };
  const seeds: Seed[] = [
    { n: 1, name: "lead-reviewer-1", template: "reviewer", harness: "codex", task: "Review the diff of TER-12 for the runner socket changes.", started: 300, finished: 290, outcome: "cancelled" },
    { n: 2, name: "lead-worker-1", template: "global:worker", harness: "claude", task: "Find every caller of `apply_status` and list the ones that skip the wait refresh.", started: 240, finished: 120, outcome: "timeout", result: "I found 4 callers so far: lifecycle.rs (wake), api/agents.rs (start), runner_supervisor.rs (two). Still checking executor.rs." },
    { n: 3, name: "lead-worker-2", template: "global:worker", harness: "claude", task: "Run the flaky runner tests 20 times and report which ones fail.\nUse `cargo test -p terrarium-runner -- --test-threads 1`.", started: 52, finished: 43, outcome: "done", result: `2 of 20 runs failed, both in pty_resize_keeps_the_screen:\n- run 7: the screen had 23 rows, not 24\n- run 15: the same\nThe other tests passed 20 of 20. Log: /work/terrarium/lead-worker-2/flaky.log (in the trash now). ${xss}` },
    { n: 4, name: "lead-worker-3", template: "global:worker", harness: "claude", task: "Write the CHANGELOG entry for the spawn feature from docs/PLAN.md §18.", started: 12, status: "working", card: "TER-13" },
    { n: 5, name: "lead-reviewer-2", template: "reviewer", harness: "codex", task: "Check that docs/api.md §6.10 matches crates/terrariumd/src/api/spawns.rs.", started: 5, status: "idle" },
  ];
  w.spawns ??= [];
  for (const s of seeds) {
    const agentId = id(s.n);
    const session: Session = { id: sid(s.n), agent: agentId, harness_session_id: `eph-${s.n}-${s.name}`, started_at: ago(s.started), turns: 1, current: !s.finished, resumes: 0, ...(s.finished ? { ended_at: ago(s.finished), outcome: "removed" } : {}) };
    w.sessions.push(session);
    const blocks: NonNullable<MockWorld["transcripts"][string]> = [];
    appendBlock(blocks, session, s.harness, { role: "system", kind: "system", text: "Session started." }, ago(s.started));
    appendBlock(blocks, session, s.harness, { role: "user", kind: "text", text: `[Agent:terrarium/lead] ${s.task}`, origin: "agent", parts: [{ source: "agent", sender: "terrarium/lead", body: s.task }] }, ago(s.started - 0.2));
    if (s.result) appendBlock(blocks, session, s.harness, { role: "assistant", kind: "text", text: s.result }, ago((s.finished ?? s.started) + 0.5));
    w.transcripts[session.id] = blocks;
    const deadline = new Date(now - s.started * 60_000 + 120 * 60_000).toISOString();
    const row: Spawn = {
      id: agentId,
      owner: lead.id,
      owner_name: "lead",
      project: "terrarium",
      name: s.name,
      template: s.template,
      harness: s.harness,
      task: s.task,
      summary: firstLine(s.task),
      spawned_by: `agent:${lead.id}`,
      spawned_at: ago(s.started),
      deadline_at: deadline,
      status: s.finished ? "finished" : (s.status ?? "idle"),
      ...(s.finished ? { finished_at: ago(s.finished), outcome: s.outcome } : {}),
      ...(s.result ? { result: s.result } : {}),
      session_id: session.id,
      duration_secs: Math.round((s.finished === undefined ? s.started : s.started - s.finished) * 60),
    };
    w.spawns.push(row);
    // A card agent (TER-75): the card names it.
    const bound = s.card && !s.finished ? w.cards.find((c) => c.id === s.card) : undefined;
    if (bound) Object.assign(bound, { agent: agentId, agent_name: s.name });
    if (s.finished) {
      w.removedAgents.push({ id: agentId, project: "terrarium", name: s.name, harness: s.harness });
    } else {
      w.agents.push({
        id: agentId,
        name: s.name,
        project: "terrarium",
        harness: s.harness,
        subscription: s.harness === "codex" ? "chatgpt-pro" : "anthropic-max",
        status: s.status ?? "idle",
        roles: [],
        policy: { targets: [], mail_to: ["*"] },
        restart: "on-failure",
        paused: false,
        idle_stop: "off",
        compact_supported: true,
        workspace: `/work/terrarium/${s.name}`,
        source: "api",
        session: session.harness_session_id,
        ephemeral: true,
        ...(s.card ? { current_card: s.card } : {}),
        owner: lead.id,
        spawn: { owner_name: "lead", template: s.template, task: s.task, spawned_at: ago(s.started), deadline_at: deadline },
        created_at: ago(s.started),
        updated_at: ago(s.started - 0.5),
      });
    }
    const ev = (min: number, kind: AgentEvent["kind"], summary: string, data: Record<string, unknown>) => {
      w.seq++;
      w.events.push({ seq: w.seq, id: `01JB0EPHEV${String(w.seq).padStart(16, "0")}`, at: ago(min), kind, actor: "system", agent: "lead", agent_id: lead.id, project: "terrarium", session: session.id, summary, data: { agent_id: lead.id, ...data } });
    };
    ev(s.started, "agent.spawned", `spawned ${s.name} (${s.template})`, { spawn_id: agentId, name: s.name, template: s.template, owner_id: lead.id, owner_name: "lead", session_id: session.id, task: firstLine(s.task), deadline_at: deadline });
    if (s.finished) ev(s.finished, "agent.finished", `${s.name} finished: ${s.outcome}`, { spawn_id: agentId, name: s.name, template: s.template, owner_id: lead.id, owner_name: "lead", outcome: s.outcome, session_id: session.id, duration_secs: row.duration_secs });
  }
  lead.spawned_running = w.spawns.filter((s) => s.owner === lead.id && !s.finished_at).length;
}

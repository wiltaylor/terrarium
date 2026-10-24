// In-memory implementation of `Api` for VITE_MOCK=1 and for tests.
// Every endpoint works. State lives in one `MockWorld` per instance.

import type { Api } from "../client";
import { ApiError } from "../http";
import type { ConnectionState, EventStream } from "../sockets";
import type {
  Agent,
  AgentEvent,
  AgentStatus,
  Card,
  LoginKind,
  LoginSession,
  Message,
  Subscription,
  Version,
  WikiPage,
} from "../types";
import { seed, type MockWorld } from "./data";
import { FakePty } from "./pty";

export interface MockOptions {
  /** Artificial latency for every call. Default 120 ms; tests use 0. */
  latencyMs?: number;
  /** Interval of the fake event firehose. 0 turns it off. Default 3000 ms. */
  eventsIntervalMs?: number;
  /** Delay before a device-code login completes by itself. Default 8000 ms. */
  autoLoginMs?: number;
  world?: MockWorld;
  now?: () => number;
}

const clone = <T>(v: T): T => structuredClone(v);

export function createMockApi(opts: MockOptions = {}): Api & { world: MockWorld } {
  const now = opts.now ?? Date.now;
  const latency = opts.latencyMs ?? 120;
  const w = opts.world ?? seed(now());
  const iso = () => new Date(now()).toISOString();
  let seq = 1000;
  const nextId = (prefix: string) => `${prefix}-${++seq}`;
  const startedAt = now();

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

  const notFound = (what: string): never => {
    throw new ApiError(404, `${what} not found`);
  };
  const find = <T>(list: T[], pred: (x: T) => boolean, what: string): T => list.find(pred) ?? notFound(what);
  const agent = (id: string) => find(w.agents, (a) => a.id === id || a.name === id, `agent ${id}`);
  const cardById = (id: string) => find(w.cards, (c) => c.id === id, `card ${id}`);
  const page = (id: string) => find(w.pages, (p) => p.id === id, `page ${id}`);
  const version = (id: string) => find(w.versions, (v) => v.id === id || v.name === id, `version ${id}`);
  const sub = (name: string) => find(w.subscriptions, (s) => s.name === name, `subscription ${name}`);

  // ----- event bus -----
  const listeners = new Set<(e: AgentEvent) => void>();
  const emit = (e: Omit<AgentEvent, "id" | "at">) => {
    const full: AgentEvent = { id: nextId("EV"), at: iso(), ...e };
    w.events.unshift(full);
    w.events.length = Math.min(w.events.length, 500);
    if (full.agent) {
      const a = w.agents.find((x) => x.name === full.agent && x.project === full.project);
      if (a) a.last_event = full;
    }
    listeners.forEach((fn) => fn(clone(full)));
    return full;
  };
  const setStatus = (a: Agent, status: AgentStatus, summary?: string) => {
    a.status = status;
    if (status === "working") a.turn_started_at = iso();
    if (status !== "working") delete a.turn_started_at;
    emit({ kind: "status", agent: a.name, project: a.project, summary: summary ?? `Status ${status}`, data: { status, agent_id: a.id } });
  };

  const TOOLS = ["Read", "Edit", "Bash", "Grep", "Write"];
  const FILES = ["crates/terrariumd/src/runner.rs", "crates/terrarium-core/src/event.rs", "dashboard/src/api/client.ts", "Cargo.toml"];
  let tick = 0;
  const firehose = () => {
    tick++;
    const working = w.agents.filter((a) => a.status === "working");
    const a = working[tick % Math.max(working.length, 1)];
    if (a && tick % 5 !== 0) {
      const tool = TOOLS[tick % TOOLS.length]!;
      const target = tool === "Bash" ? "cargo test -p terrariumd" : FILES[tick % FILES.length]!;
      emit({ kind: "tool_call", agent: a.name, project: a.project, summary: `${tool} ${target}`, data: { tool } });
    }
    if (tick % 5 === 0) {
      const lead = w.agents.find((x) => x.name === "lead");
      if (lead && (lead.status === "idle" || lead.status === "working")) {
        if (lead.status === "idle") {
          setStatus(lead, "working", "Turn started: triage inbox");
          emit({ kind: "turn_start", agent: lead.name, project: lead.project, summary: "Turn started: triage inbox" });
        } else {
          setStatus(lead, "idle", "Turn ended");
          emit({ kind: "turn_end", agent: lead.name, project: lead.project, summary: "Turn ended after 12s" });
        }
      }
    }
    if (tick % 7 === 0) {
      const reviewer = w.agents.find((x) => x.name === "reviewer");
      if (reviewer?.status === "starting") setStatus(reviewer, "idle", "Session ready");
    }
    // Usage creeps up.
    for (const s of w.subscriptions) {
      for (const win of s.windows) win.used_pct = Math.min(100, win.used_pct + (tick % 3 === 0 ? 1 : 0));
    }
  };
  let timer: ReturnType<typeof setInterval> | undefined;
  const ensureFirehose = () => {
    const ms = opts.eventsIntervalMs ?? 3000;
    if (!timer && ms > 0) timer = setInterval(firehose, ms);
  };

  // ----- boards -----
  const boardOf = (project: string) => {
    find(w.projects, (p) => p.name === project, `project ${project}`);
    return {
      project,
      columns: w.columns[project] ?? [],
      cards: w.cards.filter((c) => c.project === project).sort((a, b) => a.position - b.position),
    };
  };
  const reindex = (project: string, column: string) => {
    w.cards
      .filter((c) => c.project === project && c.column === column)
      .sort((a, b) => a.position - b.position)
      .forEach((c, i) => (c.position = i));
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
      const name = `${base[s.kind].provider}-${seq++}`;
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
      w.audit.unshift({ id: nextId("AU"), at: iso(), actor: "operator", action: "subscription.login", target: name });
      emit({ kind: "subscription", summary: `Logged in: ${name}`, data: { name } });
    }, latency > 0 ? 1200 : 0);
  };

  const api: Api & { world: MockWorld } = {
    world: w,
    health: () => reply(() => ({ version: "0.1.0-mock", uptime: Math.round((now() - startedAt) / 1000) + 3600 })),

    projects: {
      list: () => reply(() => w.projects),
      get: (name) => reply(() => find(w.projects, (p) => p.name === name, `project ${name}`)),
      create: (p) =>
        reply(() => {
          const proj = { ...p, card_prefix: p.name.slice(0, 3).toUpperCase(), created_at: iso() };
          w.projects.push(proj);
          w.columns[p.name] = clone(w.columns.terrarium ?? []);
          return proj;
        }),
      update: (name, patch) => reply(() => Object.assign(find(w.projects, (p) => p.name === name, "project"), patch)),
      remove: (name) => reply(() => void (w.projects = w.projects.filter((p) => p.name !== name))),
      agents: (name) => reply(() => w.agents.filter((a) => a.project === name)),
      targets: (name) => reply(() => w.targets.filter((t) => t.project === name)),
      repos: (name) => reply(() => w.repos.filter((r) => r.project === name)),
      attachRepo: (name, r) =>
        reply(() => {
          const repo = { id: nextId("repo"), project: name, ...r };
          w.repos.push(repo);
          return repo;
        }),
    },

    repos: { checkout: () => reply(() => undefined) },

    agents: {
      list: (project) => reply(() => w.agents.filter((a) => !project || a.project === project)),
      get: (id) => reply(() => agent(id)),
      create: (def) =>
        reply(() => {
          const a: Agent = {
            id: nextId("01JBNEW"),
            name: String(def.name ?? "agent"),
            project: String(def.project ?? "terrarium"),
            harness: (def.harness as Agent["harness"]) ?? "claude",
            subscription: String(def.subscription ?? "anthropic-max"),
            status: "starting",
            roles: ["member"],
            workspace: "/state/workspaces/new",
            created_at: iso(),
          };
          w.agents.push(a);
          return a;
        }),
      remove: (id) => reply(() => void (w.agents = w.agents.filter((a) => a.id !== id))),
      start: (id) => reply(() => setStatus(agent(id), "starting")).then(() => {
        setTimeout(() => setStatus(agent(id), "idle"), latency > 0 ? 1500 : 0);
      }),
      stop: (id) => reply(() => void setStatus(agent(id), "stopped", "Stopped by operator")),
      restart: (id) =>
        reply(() => void setStatus(agent(id), "restarting")).then(() => {
          setTimeout(() => setStatus(agent(id), "idle", "Restarted"), latency > 0 ? 1500 : 0);
        }),
      send: (id, text) =>
        reply(() => {
          const a = agent(id);
          emit({ kind: "turn_start", agent: a.name, project: a.project, summary: `Turn typed by operator: ${text.slice(0, 60)}` });
          setStatus(a, "working");
        }),
      events: (id) =>
        reply(() => {
          const a = agent(id);
          return w.events.filter((e) => e.agent === a.name && e.project === a.project);
        }),
      sessions: (id) => reply(() => w.sessions.filter((s) => s.agent === agent(id).id)),
      roles: (id) => reply(() => agent(id).roles),
      setRoles: (id, roles) => reply(() => (agent(id).roles = roles)),
      pty: (id) => {
        const a = w.agents.find((x) => x.id === id || x.name === id);
        const name = a?.name ?? id;
        let n = 0;
        return new FakePty({
          banner: [
            `\x1b[1;35m✻ ${a?.harness ?? "claude"}\x1b[0m  \x1b[2m(mock PTY for ${name})\x1b[0m`,
            `\x1b[2mcwd: ${a?.workspace ?? "/state/workspaces"}\x1b[0m`,
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
      get: (project) => reply(() => boardOf(project)),
      update: (project, columns) =>
        reply(() => {
          w.columns[project] = columns;
          return boardOf(project);
        }),
    },

    cards: {
      create: (project, c) =>
        reply(() => {
          const proj = find(w.projects, (p) => p.name === project, "project");
          const n = Math.max(0, ...w.cards.filter((x) => x.project === project).map((x) => Number(x.id.split("-")[1]) || 0)) + 1;
          const column = c.column ?? "backlog";
          const card: Card = {
            id: `${proj.card_prefix}-${n}`,
            project,
            column,
            position: w.cards.filter((x) => x.project === project && x.column === column).length,
            title: c.title,
            body: c.body ?? "",
            assignee: c.assignee,
            labels: c.labels ?? [],
            depends_on: [],
            comments: [],
            links: c.from_mail ? [{ kind: "thread", ref: w.mail.find((m) => m.id === c.from_mail)?.thread_id ?? c.from_mail }] : [],
            created_by: "operator",
            created_at: iso(),
            updated_at: iso(),
          };
          w.cards.push(card);
          emit({ kind: "card", project, summary: `operator created ${card.id}`, data: { card_id: card.id } });
          return card;
        }),
      get: (id) => reply(() => cardById(id)),
      update: (id, patch) =>
        reply(() => {
          const c = cardById(id);
          const cols = w.columns[c.project] ?? [];
          if (patch.column && patch.column !== c.column) {
            const col = cols.find((x) => x.name === patch.column) ?? notFound(`column ${patch.column}`);
            const count = w.cards.filter((x) => x.project === c.project && x.column === col.name).length;
            if (col.wip_limit !== undefined && count >= col.wip_limit)
              throw new ApiError(409, `Column ${col.name} is at its WIP limit (${col.wip_limit}).`);
            if (col.name === "in_progress") {
              const unmet = c.depends_on.filter((d) => w.cards.find((x) => x.id === d)?.column !== "done");
              if (unmet.length) throw new ApiError(409, `${c.id} depends on ${unmet.join(", ")}, which are not done.`);
            }
            if (col.name === "blocked" && !patch.comment?.trim())
              throw new ApiError(422, "Moving a card to blocked requires a comment.");
          }
          const from = c.column;
          if (patch.comment) c.comments.push({ author: "operator", body: patch.comment, created_at: iso() });
          if (patch.title !== undefined) c.title = patch.title;
          if (patch.body !== undefined) c.body = patch.body;
          if (patch.labels !== undefined) c.labels = patch.labels;
          if (patch.assignee !== undefined) c.assignee = patch.assignee ?? undefined;
          if (patch.column !== undefined || patch.position !== undefined) {
            c.column = patch.column ?? c.column;
            // Insert at the position, shifting the others down.
            const target = patch.position ?? Number.MAX_SAFE_INTEGER;
            w.cards
              .filter((x) => x !== c && x.project === c.project && x.column === c.column && x.position >= target)
              .forEach((x) => x.position++);
            c.position = target === Number.MAX_SAFE_INTEGER ? 1e9 : target;
            reindex(c.project, c.column);
            if (from !== c.column) reindex(c.project, from);
          }
          c.updated_at = iso();
          emit({
            kind: "card",
            project: c.project,
            summary: from !== c.column ? `operator moved ${c.id} to ${c.column}` : `operator edited ${c.id}`,
            data: { card_id: c.id, column: c.column },
          });
          return c;
        }),
      archive: (id) => reply(() => void (w.cards = w.cards.filter((c) => c.id !== id))),
      comment: (id, body) =>
        reply(() => {
          const c = cardById(id);
          c.comments.push({ author: "operator", body, created_at: iso() });
          emit({ kind: "card", project: c.project, summary: `operator commented on ${c.id}`, data: { card_id: c.id } });
          return c;
        }),
      events: (id) => reply(() => w.events.filter((e) => e.data?.card_id === id || e.summary.includes(id))),
    },

    memories: {
      list: (project, q = {}) =>
        reply(() => {
          const needle = q.q?.toLowerCase();
          return w.memories.filter(
            (m) =>
              m.project === project &&
              (q.all || !m.superseded_by) &&
              (!q.tag || m.tags.includes(q.tag)) &&
              (!q.author || m.author === q.author) &&
              (!needle || `${m.title} ${m.body} ${m.tags.join(" ")}`.toLowerCase().includes(needle)),
          );
        }),
      create: (project, m) =>
        reply(() => {
          const mem = { id: nextId("MEM"), project, ...m, author: "operator", sources: [], created_at: iso(), updated_at: iso() };
          w.memories.push(mem);
          return mem;
        }),
      get: (id) => reply(() => find(w.memories, (m) => m.id === id, `memory ${id}`)),
      update: (id, patch) => reply(() => Object.assign(find(w.memories, (m) => m.id === id, "memory"), patch, { updated_at: iso() })),
      remove: (id) => reply(() => void (w.memories = w.memories.filter((m) => m.id !== id))),
      graph: (id, depth = 1) =>
        reply(() => {
          const seen = new Set([find(w.memories, (m) => m.id === id, `memory ${id}`).id]);
          let frontier = [id];
          for (let d = 0; d < depth; d++) {
            const next: string[] = [];
            for (const l of w.links) {
              for (const [a, b] of [[l.from, l.to], [l.to, l.from]] as const) {
                if (frontier.includes(a) && !seen.has(b)) {
                  seen.add(b);
                  next.push(b);
                }
              }
            }
            frontier = next;
          }
          return {
            nodes: w.memories.filter((m) => seen.has(m.id)),
            links: w.links.filter((l) => seen.has(l.from) && seen.has(l.to)),
          };
        }),
      link: (id, to, relation, note) =>
        reply(() => {
          const l = { from: id, to, relation, note, author: "operator", created_at: iso() };
          w.links.push(l);
          return l;
        }),
      unlink: (id, to, relation) =>
        reply(() => void (w.links = w.links.filter((l) => !(l.from === id && l.to === to && l.relation === relation)))),
      supersede: (id, by) =>
        reply(() => {
          find(w.memories, (m) => m.id === id, "memory").superseded_by = by;
          w.links.push({ from: by, to: id, relation: "supersedes", author: "operator", created_at: iso() });
        }),
    },

    wiki: {
      tree: (project, q) =>
        reply(() =>
          w.pages
            .filter((p) => p.project === project && (!q || `${p.path} ${p.title} ${p.body}`.toLowerCase().includes(q.toLowerCase())))
            .map(({ body: _b, reviewer: _r, ...summary }) => summary)
            .sort((a, b) => a.path.localeCompare(b.path)),
        ),
      create: (project, pg) =>
        reply(() => {
          const created: WikiPage = { id: nextId("PG"), project, path: pg.path, title: pg.title, body: pg.body, status: "draft", current_rev: 1, updated_at: iso(), updated_by: "operator" };
          w.pages.push(created);
          w.revisions[created.id] = [{ rev: 1, author: "operator", summary: pg.summary, created_at: iso(), body: pg.body }];
          return created;
        }),
      get: (id) => reply(() => page(id)),
      save: (id, body) =>
        reply(() => {
          const pg = page(id);
          if (body.base !== pg.current_rev) throw new ApiError(409, `Stale base ${body.base}; current is ${pg.current_rev}.`);
          if (!body.summary.trim()) throw new ApiError(422, "A revision needs a summary.");
          pg.current_rev++;
          pg.body = body.body;
          pg.updated_at = iso();
          pg.updated_by = "operator";
          (w.revisions[id] ??= []).push({ rev: pg.current_rev, author: "operator", summary: body.summary, created_at: iso(), body: body.body });
          return pg;
        }),
      remove: (id) => reply(() => void (w.pages = w.pages.filter((p) => p.id !== id))),
      section: (id, s) =>
        reply(() => {
          const pg = page(id);
          pg.body += `\n\n## ${s.heading}\n\n${s.body}\n`;
          pg.current_rev++;
          return pg;
        }),
      revisions: (id) => reply(() => (w.revisions[id] ?? []).map(({ body: _b, ...r }) => r).reverse()),
      revision: (id, rev) => reply(() => find(w.revisions[id] ?? [], (r) => r.rev === rev, `revision ${rev}`)),
      diff: (id, a, b) =>
        reply(() => {
          const ra = find(w.revisions[id] ?? [], (r) => r.rev === a, "rev").body.split("\n");
          const rb = find(w.revisions[id] ?? [], (r) => r.rev === b, "rev").body.split("\n");
          const out = [...ra.filter((l) => !rb.includes(l)).map((l) => `- ${l}`), ...rb.filter((l) => !ra.includes(l)).map((l) => `+ ${l}`)];
          return { diff: out.join("\n") };
        }),
      restore: (id, rev) =>
        reply(() => {
          const pg = page(id);
          const r = find(w.revisions[id] ?? [], (x) => x.rev === rev, "rev");
          pg.current_rev++;
          pg.body = r.body;
          (w.revisions[id] ??= []).push({ rev: pg.current_rev, author: "operator", summary: `Restore rev ${rev}`, created_at: iso(), body: r.body });
          return pg;
        }),
      backlinks: (id) =>
        reply(() => {
          const pg = page(id);
          return w.pages
            .filter((p) => p.id !== id && p.body.includes(`[[${pg.path}]]`))
            .map(({ body: _b, reviewer: _r, ...summary }) => summary);
        }),
      review: (id, decision, comment) =>
        reply(() => {
          const pg = page(id);
          if (decision === "approve") pg.status = "reviewed";
          emit({ kind: "wiki", project: pg.project, summary: `operator ${decision === "approve" ? "approved" : "commented on"} ${pg.path}: ${comment}` });
          return pg;
        }),
    },

    artifacts: {
      list: (project) => reply(() => w.artifacts.filter((a) => a.project === project)),
      upload: (project, form) =>
        reply(() => {
          const file = form.get("file");
          const name = file instanceof File ? file.name : "upload.bin";
          const a = { id: nextId("ART"), project, name, content_type: file instanceof File ? file.type : "application/octet-stream", size: file instanceof File ? file.size : 0, sha256: "0".repeat(64), kind: "file" as const, created_by: "operator", created_at: iso(), url: "data:text/plain,uploaded", links: [] };
          w.artifacts.push(a);
          return a;
        }),
      get: (id) => reply(() => find(w.artifacts, (a) => a.id === id, `artifact ${id}`)),
      remove: (id) => reply(() => void (w.artifacts = w.artifacts.filter((a) => a.id !== id))),
      link: (id, kind, ref) => reply(() => void find(w.artifacts, (a) => a.id === id, "artifact").links.push({ kind, ref })),
      text: (a) => reply(() => w.artifactBodies[a.id] ?? ""),
    },

    versions: {
      list: (project) => reply(() => w.versions.filter((v) => v.project === project).sort((a, b) => b.created_at.localeCompare(a.created_at))),
      create: (project, v) =>
        reply(() => {
          const ver: Version = { id: nextId("VER"), project, name: v.name, notes: v.notes, status: "draft", cards: [], artifacts: [], reviews: [], created_by: "operator", created_at: iso() };
          w.versions.push(ver);
          return ver;
        }),
      get: (id) => reply(() => version(id)),
      update: (id, patch) => reply(() => Object.assign(version(id), patch)),
      transition: (id, action, comment) =>
        reply(() => {
          const v = version(id);
          const allowed: Record<typeof action, Version["status"][]> = {
            submit: ["draft", "rejected"],
            approve: ["in_review"],
            reject: ["in_review"],
            release: ["approved"],
          };
          if (!allowed[action].includes(v.status)) throw new ApiError(409, `Cannot ${action} a version that is ${v.status}.`);
          if (action === "reject" && !comment?.trim()) throw new ApiError(422, "A rejection needs a comment.");
          const next = { submit: "in_review", approve: "approved", reject: "rejected", release: "released" } as const;
          v.status = next[action];
          if (action === "submit") v.submitted_at = iso();
          if (action === "approve" || action === "reject") {
            v.decided_at = iso();
            v.decided_by = "operator";
            v.reviews.push({ author: "operator", decision: action, comment: comment ?? "", created_at: iso() });
          }
          emit({ kind: "version", project: v.project, summary: `operator ${next[action].replace("_", " ")} ${v.name}` });
          return v;
        }),
      compare: (id, to) =>
        reply(() => {
          const a = version(id);
          const b = version(to);
          return { cards: a.cards.filter((c) => !b.cards.includes(c)), pages: ["reports/phase-1"], artifacts: a.artifacts.filter((x) => !b.artifacts.includes(x)) };
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
          if (!s.probe) throw new ApiError(422, `${name} has no probe; local estimate only.`);
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
          if (!code.trim()) throw new ApiError(422, "Empty code.");
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
          if (!input.key.trim()) throw new ApiError(422, "The key is empty.");
          let s = w.subscriptions.find((x) => x.name === name);
          if (!s) {
            s = { name, provider: input.provider, plan: "API key", harnesses: ["pi"], probe: null, alert_at: 80, status: "logged_in", windows: [], history: Array(24).fill(0) };
            w.subscriptions.push(s);
          }
          s.status = "logged_in";
          s.verified_at = iso();
          w.audit.unshift({ id: nextId("AU"), at: iso(), actor: "operator", action: "subscription.api_key_set", target: name });
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
          for (const a of w.agents.filter((x) => (!q.project || x.project === q.project) && (!q.subscription || x.subscription === q.subscription) && (!q.agent || x.id === q.agent))) {
            const k = (a.name.length * 31 + h * 7) % 13;
            out.push({ at: new Date(now() - h * 3600_000).toISOString(), subscription: a.subscription, project: a.project, agent: a.name, model: a.model ?? "default", input_tokens: k * 4200, output_tokens: k * 900, cache_tokens: k * 15000, turns: k % 4 });
          }
        }
        return out;
      }),

    mail: {
      list: (q = {}) => reply(() => w.mail.filter((m) => (!q.to || m.to === q.to) && (!q.state || m.state === q.state)).sort((a, b) => b.created_at.localeCompare(a.created_at))),
      send: (m) =>
        reply(() => {
          const msg: Message = { id: nextId("MSG"), thread_id: m.thread_id ?? nextId("TH"), from: m.from ?? "operator", to: m.to, subject: m.subject, body: m.body, state: "queued", created_at: iso() };
          w.mail.push(msg);
          emit({ kind: "mail", summary: `Mail from ${msg.from} to ${msg.to}: ${msg.subject}` });
          setTimeout(() => (msg.state = "delivered"), latency > 0 ? 1000 : 0);
          return msg;
        }),
      markRead: (id) => reply(() => void (find(w.mail, (m) => m.id === id, "message").state = "read")),
      reply: (id, body) =>
        reply(() => {
          const orig = find(w.mail, (m) => m.id === id, "message");
          orig.state = "replied";
          const msg: Message = { id: nextId("MSG"), thread_id: orig.thread_id, from: "operator", to: orig.from, subject: orig.subject.startsWith("Re:") ? orig.subject : `Re: ${orig.subject}`, body, state: "queued", created_at: iso() };
          w.mail.push(msg);
          return msg;
        }),
    },

    inbox: {
      list: (project) => reply(() => w.mail.filter((m) => m.to === `project:${project}`).sort((a, b) => b.created_at.localeCompare(a.created_at))),
      send: (project, m) => api.mail.send({ ...m, to: `project:${project}` }),
    },

    audit: (q = {}) => reply(() => w.audit.filter((a) => (!q.actor || a.actor === q.actor) && (!q.since || a.at >= q.since))),

    targets: {
      list: () => reply(() => w.targets),
      create: (t) =>
        reply(() => {
          const target = { id: nextId("TGT"), name: t.name ?? "target", kind: t.kind ?? "ssh", state: "down" as const, ephemeral: false, snapshots: [], ...t };
          w.targets.push(target);
          return target;
        }),
      action: (id, action) =>
        reply(() => {
          const t = find(w.targets, (x) => x.id === id, `target ${id}`);
          if (action === "up") t.state = "up";
          if (action === "down") t.state = "down";
          if (action === "snapshot") t.snapshots.push(`snap-${t.snapshots.length + 1}`);
          w.audit.unshift({ id: nextId("AU"), at: iso(), actor: "operator", action: `target.${action}`, target: t.name });
          return t;
        }),
    },

    hosts: { list: () => reply(() => w.hosts) },

    events: (): EventStream => {
      ensureFirehose();
      const mine = new Set<(e: AgentEvent) => void>();
      const states = new Set<(s: ConnectionState) => void>();
      queueMicrotask(() => states.forEach((fn) => fn("open")));
      return {
        subscribe(fn) {
          mine.add(fn);
          listeners.add(fn);
          return () => {
            mine.delete(fn);
            listeners.delete(fn);
          };
        },
        onState(fn) {
          states.add(fn);
          return () => states.delete(fn);
        },
        close() {
          mine.forEach((fn) => listeners.delete(fn));
          states.forEach((fn) => fn("closed"));
        },
      };
    },
  };
  return api;
}

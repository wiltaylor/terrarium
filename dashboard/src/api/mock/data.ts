// Seed data for mock mode. `seed(now)` returns a fresh, self-consistent world.

import type {
  Agent,
  AgentEvent,
  Artifact,
  AuditEntry,
  Card,
  Column,
  Host,
  Memory,
  MemoryEdge,
  Message,
  Project,
  QuotaHold,
  QuotaSample,
  Repo,
  Revision,
  Session,
  Subscription,
  Target,
  Version,
  WikiPage,
} from "../types";

export interface MockWorld {
  projects: Project[];
  repos: Repo[];
  agents: Agent[];
  sessions: Session[];
  /** The event log, oldest first, like the daemon's. */
  events: AgentEvent[];
  columns: Record<string, Column[]>;
  cards: Card[];
  memories: Memory[];
  /** Every memory edge once. */
  links: MemoryEdge[];
  pages: WikiPage[];
  revisions: Record<string, (Revision & { body: string })[]>;
  artifacts: Artifact[];
  artifactBodies: Record<string, string>;
  versions: Version[];
  subscriptions: Subscription[];
  /** Quota samples, oldest first (docs/api.md §16.3 `.../quota`). */
  quota: QuotaSample[];
  /** Local accounting rows (`usage_samples`). `agent` is the ULID, as in the daemon. */
  usage: UsageRow[];
  /** Holds on agents' turns (docs/api.md §16.6). */
  holds: QuotaHold[];
  /** Stored API keys by subscription. No endpoint returns them. */
  apiKeys: Record<string, { provider: string; key: string; base_url?: string; models?: string[] }>;
  mail: Message[];
  targets: Target[];
  hosts: Host[];
  /** Host tokens by host name. The API shows a token once, at create. */
  hostTokens: Record<string, string>;
  audit: AuditEntry[];
  /** The last `seq` handed out. */
  seq: number;
}

/** One row of local accounting, as the daemon stores it. */
export interface UsageRow {
  at: string;
  agent: string;
  project: string;
  subscription: string;
  model: string;
  input_tokens: number;
  output_tokens: number;
  cache_read: number;
  cache_write: number;
  messages: number;
  /** The `seq` of the agent's `turn_start`. */
  turn: number;
  /** The card the agent had in `in_progress` when the row was stored (`card_id`). */
  card?: string;
}

const COLUMNS: Column[] = [
  { name: "backlog", position: 0 },
  { name: "ready", position: 1 },
  { name: "in_progress", position: 2, wip_limit: 3 },
  { name: "review", position: 3 },
  { name: "blocked", position: 4 },
  { name: "done", position: 5 },
];

/** A fake 26-character ULID with a readable tail. */
export const fakeUlid = (tag: string, n: number): string => `01JB${tag.toUpperCase().replace(/[^0-9A-Z]/g, "").padEnd(6, "0").slice(0, 6)}${String(n).padStart(16, "0")}`;

export function seed(now: number = Date.now()): MockWorld {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const ahead = (min: number) => new Date(now + min * 60_000).toISOString();

  const projects: Project[] = [
    { name: "terrarium", description: "The harness wrapper itself.", card_prefix: "TER", source: "file", created_at: ago(60 * 24 * 9), updated_at: ago(60 * 24 * 9) },
    { name: "webshop", description: "Storefront rewrite in Rust + HTMX.", card_prefix: "WEB", source: "api", created_at: ago(60 * 24 * 4), updated_at: ago(60 * 24 * 4) },
  ];

  const repos: Repo[] = [
    { id: fakeUlid("repo", 1), project: "terrarium", name: "terrarium", url: "git@github.com:wil/terrarium.git", default_branch: "main", path: "terrarium", source: "file", created_at: ago(60 * 24 * 9) },
    { id: fakeUlid("repo", 2), project: "webshop", name: "webshop", url: "git@github.com:wil/webshop.git", default_branch: "main", credential: "github-deploy", path: "webshop", source: "api", created_at: ago(60 * 24 * 4) },
  ];

  const agentRow = (
    id: string,
    name: string,
    project: string,
    harness: Agent["harness"],
    subscription: string,
    status: Agent["status"],
    extra: Partial<Agent> = {},
  ): Agent => ({
    id,
    name,
    project,
    harness,
    subscription,
    status,
    roles: [],
    policy: { targets: [], mail_to: ["*"] },
    restart: "on-failure",
    workspace: `/state/agents/${id}/workspace`,
    source: "api",
    created_at: ago(60 * 24 * 3),
    updated_at: ago(60 * 24),
    ...extra,
  });

  const agents: Agent[] = [
    agentRow("01JB0LEAD00000000000000001", "lead", "terrarium", "claude", "anthropic-max", "idle", { roles: ["project_admin"], prompt: "lead", source: "file" }),
    agentRow("01JB0WRK100000000000000002", "runner-dev", "terrarium", "claude", "anthropic-max", "working", { current_card: "TER-12", prompt: "worker" }),
    agentRow("01JB0WRK200000000000000003", "adapter-codex", "terrarium", "codex", "chatgpt-pro", "blocked", { model: "gpt-5-codex", current_card: "TER-15" }),
    agentRow("01JB0WRK300000000000000004", "reviewer", "terrarium", "pi", "openrouter-key", "starting", { model: "anthropic/claude-sonnet", policy: { targets: [], mail_to: ["lead"], cross_project: ["webshop/shop-lead"] } }),
    agentRow("01JB0WEB100000000000000005", "shop-lead", "webshop", "copilot", "copilot-pro", "working", { roles: ["project_admin"], current_card: "WEB-4", policy: { targets: ["pg-test"], mail_to: ["*"] } }),
    agentRow("01JB0WEB200000000000000006", "css-worker", "webshop", "codex", "chatgpt-pro", "error", { restart: "never" }),
    agentRow("01JB0SYS000000000000000007", "sysadmin", "terrarium", "claude", "anthropic-max", "stopped", { roles: ["sysadmin"], restart: "always" }),
  ];
  const byName = (name: string) => agents.find((a) => a.name === name)!;
  /** The stored address and the readable name of an agent, `operator`, or `project:<name>`. */
  const who = (name: string): { addr: string; kind: Message["from_kind"]; label: string } => {
    if (name === "operator") return { addr: "operator", kind: "operator", label: "operator" };
    if (name.startsWith("project:")) return { addr: name, kind: "project", label: name };
    const a = byName(name);
    return { addr: a.id, kind: "agent", label: `${a.project}/${a.name}` };
  };

  const sessions: Session[] = agents.flatMap((a, i) => [
    { id: fakeUlid(`ses${i}`, 1), agent: a.id, harness_session_id: `${(i * 7919).toString(16)}-0a1b-4c2d-9e8f-${a.name}`, started_at: ago(60 * 26), ended_at: ago(60 * 20), outcome: "restarted", turns: 12 + i, current: false },
    ...(a.status === "stopped"
      ? []
      : [{ id: fakeUlid(`ses${i}`, 2), agent: a.id, harness_session_id: `${(i * 104729).toString(16)}-77aa-4bbb-8ccc-${a.name}`, started_at: ago(60 * 3), turns: 3 + i, current: true }]),
  ]);
  const sessionOf = (a: Agent) => sessions.find((s) => s.agent === a.id && s.current)?.id;
  for (const a of agents) a.session = sessions.find((s) => s.agent === a.id && s.current)?.harness_session_id;

  let seq = 0;
  const events: AgentEvent[] = [];
  /** An event about an agent, shaped like the daemon's (docs/api.md §9). */
  const ev = (min: number, kind: AgentEvent["kind"], agentName: string | null, project: string, summary: string, data: Record<string, unknown> = {}, extra: Partial<AgentEvent> = {}) => {
    const a = agentName ? byName(agentName) : undefined;
    seq++;
    const e: AgentEvent = {
      seq,
      id: fakeUlid("ev", seq),
      at: ago(min),
      kind,
      actor: "system",
      ...(a ? { agent: a.name, agent_id: a.id, session: sessionOf(a) } : {}),
      project,
      summary,
      data: a ? { agent_id: a.id, ...data } : data,
      ...extra,
    };
    events.push(e);
    return e;
  };
  const status = (min: number, name: string, from: Agent["status"], to: Agent["status"], reason: string | null = null) =>
    ev(min, "status", name, byName(name).project, `${name}: ${from} -> ${to}`, { status: to, from, reason });

  status(60 * 3, "sysadmin", "idle", "stopped", "stopped by operator");
  status(60 * 3 - 1, "lead", "starting", "idle");
  status(26, "css-worker", "working", "error", "restart budget exhausted");
  ev(26, "exit", "css-worker", "webshop", "Exited with code 1; restart budget exhausted", { code: 1 });
  ev(20, "turn_start", "shop-lead", "webshop", "Turn started: checkout flow");
  status(20, "shop-lead", "idle", "working");
  ev(9, "mail", "adapter-codex", "terrarium", "Mail from terrarium/lead to terrarium/adapter-codex: Re: TER-15 blocked", {
    message_id: fakeUlid("msg", 2),
    thread_id: fakeUlid("th", 1),
    from: "01JB0LEAD00000000000000001",
    from_name: "terrarium/lead",
    to: "01JB0WRK200000000000000003",
    to_name: "terrarium/adapter-codex",
    subject: "Re: TER-15 blocked: Codex adapter: resume by session id",
  });
  status(11, "adapter-codex", "working", "blocked", "needs a Codex login in the image");
  ev(6, "turn_start", "runner-dev", "terrarium", "Turn started: implement PTY resize");
  status(6, "runner-dev", "idle", "working");
  ev(5, "tool_use", "runner-dev", "terrarium", "Read crates/terrariumd/src/pty.rs", { tool: "Read" });
  ev(3, "turn_end", "lead", "terrarium", "Turn ended after 41s");
  ev(1, "tool_use", "shop-lead", "webshop", "Bash cargo test -p checkout", { tool: "Bash" });
  status(0.5, "reviewer", "stopped", "starting");
  ev(10, "self_report", "adapter-codex", "terrarium", "adapter-codex says: blocked (waiting for an operator Codex login)", { status: "blocked", note: "waiting for an operator Codex login" }, { actor: "agent:01JB0WRK200000000000000003" });
  seq++;
  events.push({
    seq,
    id: fakeUlid("ev", seq),
    at: ago(4),
    kind: "quota.alert",
    actor: "system",
    summary: "chatgpt-pro: window 5h at 87% (alert at 80%)",
    data: { subscription: "chatgpt-pro", window: "5h", used_pct: 87, resets_at: ahead(41), level: "alert", alert_at: 80 },
  });
  ev(0.2, "tool_use", "runner-dev", "terrarium", "Edit crates/terrariumd/src/runner.rs", { tool: "Edit" });

  let pos = 0;
  const card = (id: string, project: string, column: string, title: string, extra: Partial<Card> & { assigneeName?: string; comments?: Card["comments"] } = {}): Card => {
    const { assigneeName, ...rest } = extra;
    const as = assigneeName ? who(assigneeName) : undefined;
    const creator = who("lead");
    return {
      id,
      project,
      column,
      position: pos++,
      title,
      body: extra.body ?? `Details for ${title.toLowerCase()}.`,
      ...(as ? { assignee: as.addr, assignee_name: as.label } : {}),
      labels: [],
      depends_on: [],
      blocked_by: [],
      comments: [],
      links: [],
      created_by: creator.addr,
      created_by_name: creator.label,
      created_at: ago(60 * 24),
      updated_at: ago(30),
      archived: false,
      ...rest,
    };
  };
  let cid = 0;
  const comment = (author: string, body: string, min: number) => {
    const w = who(author);
    return { id: fakeUlid("cmt", ++cid), author: w.addr, author_name: w.label, body, created_at: ago(min) };
  };

  const cards: Card[] = [
    card("TER-10", "terrarium", "done", "Scaffold the Cargo workspace", { assigneeName: "lead", labels: ["infra"] }),
    card("TER-11", "terrarium", "done", "Dockerfile with pinned harnesses", { assigneeName: "lead", labels: ["infra", "docker"] }),
    card("TER-12", "terrarium", "in_progress", "PTY resize follows the browser", {
      assigneeName: "runner-dev",
      labels: ["runner"],
      branch: "runner-dev/pty-resize",
      repo: "terrarium",
      comments: [comment("runner-dev", "Claimed. Branch `runner-dev/pty-resize`.", 40), comment("runner-dev", "SIGWINCH reaches the child. Writing a test with the fake TUI.", 12)],
      links: [{ kind: "branch", ref: "runner-dev/pty-resize", created_by: byName("runner-dev").id, created_at: ago(40) }],
    }),
    card("TER-13", "terrarium", "ready", "Stall timeout marks agents blocked", { labels: ["runner"] }),
    card("TER-14", "terrarium", "ready", "Board WIP limits in the API", { labels: ["api"], depends_on: ["TER-12"], blocked_by: ["TER-12"] }),
    card("TER-15", "terrarium", "blocked", "Codex adapter: resume by session id", {
      assigneeName: "adapter-codex",
      labels: ["adapter", "codex"],
      comments: [comment("adapter-codex", "Blocked: `codex login status` fails in the image. Needs an operator login.", 11)],
    }),
    card("TER-16", "terrarium", "review", "Mailbox push path", { assigneeName: "lead", labels: ["mail"], branch: "lead/mail-push" }),
    card("TER-17", "terrarium", "backlog", "Memory graph FTS5 index", { labels: ["memory"] }),
    card("TER-18", "terrarium", "backlog", "Artifact origin with signed URLs", { labels: ["artifacts", "security"] }),
    card("WEB-1", "webshop", "done", "Product listing page", { assigneeName: "shop-lead" }),
    card("WEB-4", "webshop", "in_progress", "Checkout flow", { assigneeName: "shop-lead", labels: ["checkout"] }),
    card("WEB-5", "webshop", "blocked", "Dark mode stylesheet", {
      assigneeName: "css-worker",
      comments: [comment("css-worker", "Blocked: the harness crashed twice on the same prompt.", 26)],
    }),
    card("WEB-6", "webshop", "backlog", "Order emails"),
  ];
  const cardEvent = (min: number, id: string, change: string, summary: string, data: Record<string, unknown> = {}) => {
    const c = cards.find((x) => x.id === id)!;
    const as = c.assignee ? agents.find((a) => a.id === c.assignee) : undefined;
    seq++;
    events.push({
      seq,
      id: fakeUlid("ev", seq),
      at: ago(min),
      kind: "card",
      actor: as ? `agent:${as.id}` : "operator",
      ...(as ? { agent: as.name, agent_id: as.id } : {}),
      project: c.project,
      card: c.id,
      summary,
      data: { card_id: c.id, change, title: c.title, column: c.column, ...(as ? { agent_id: as.id } : {}), ...data },
    });
  };
  cardEvent(40, "TER-12", "claimed", "runner-dev claimed TER-12");
  cardEvent(11, "TER-15", "moved", "adapter-codex moved TER-15 to blocked", { from: "in_progress", to: "blocked", comment: "Needs an operator login." });
  cardEvent(8, "WEB-4", "claimed", "shop-lead claimed WEB-4");
  events.sort((x, y) => x.at.localeCompare(y.at)).forEach((e, i) => {
    e.seq = i + 1;
    e.id = fakeUlid("ev", i + 1);
  });
  seq = events.length;

  const lead = who("lead");
  const mem = (id: string, title: string, tags: string[], author: string, min: number, extra: Partial<Memory> = {}): Memory => {
    const w = who(author);
    return {
      id,
      key: `terrarium/${id}`,
      project: "terrarium",
      title,
      body: extra.body ?? `${title}. Seen while working on the runner.`,
      tags,
      author: w.addr,
      author_name: w.label,
      sources: [],
      supersedes: [],
      links: [],
      created_at: ago(min),
      updated_at: ago(min),
      ...extra,
    };
  };

  const memories: Memory[] = [
    mem("MEM-1", "The runner owns the PTY, not the supervisor", ["runner", "architecture"], "lead", 60 * 30, { sources: [{ kind: "path", ref: "crates/terrariumd/src/runner.rs" }] }),
    mem("MEM-2", "Claude Code fires Stop once per turn", ["hooks", "claude"], "lead", 60 * 28),
    mem("MEM-3", "Codex hooks need notify in config.toml", ["hooks", "codex"], "adapter-codex", 60 * 20),
    mem("MEM-4", "portable-pty needs a resize after spawn", ["runner", "pty"], "runner-dev", 20, { sources: [{ kind: "card", ref: "TER-12" }] }),
    mem("MEM-5", "xterm.js resize must debounce", ["dashboard", "pty"], "runner-dev", 15),
    mem("MEM-6", "Stall timeout default is 10 minutes", ["runner"], "lead", 60 * 10, { supersedes: ["MEM-7"] }),
    mem("MEM-7", "Stall timeout default is 5 minutes", ["runner"], "lead", 60 * 40, { superseded_by: "MEM-6" }),
    mem("MEM-8", "bwrap needs user namespaces in Docker", ["sandbox", "docker"], "lead", 60 * 50, { sources: [{ kind: "commit", ref: "a1b2c3d" }] }),
  ];

  const link = (from: string, to: string, relation: MemoryEdge["relation"], author = "lead"): MemoryEdge => {
    const w = who(author);
    return { from, to, relation, author: w.addr, author_name: w.label, created_at: ago(60) };
  };

  /** Every edge once. `links` on each memory is derived from these, in both directions. */
  const links: MemoryEdge[] = [
    link("MEM-4", "MEM-1", "part_of", "runner-dev"),
    link("MEM-5", "MEM-4", "relates_to", "runner-dev"),
    link("MEM-6", "MEM-7", "supersedes"),
    link("MEM-6", "MEM-1", "part_of"),
    link("MEM-3", "MEM-2", "contradicts", "adapter-codex"),
    link("MEM-8", "MEM-1", "relates_to"),
    link("MEM-2", "MEM-1", "supports"),
  ];

  const pageBodies: Record<string, string> = {
    index: `# Terrarium wiki

Start here. The lead keeps this page current.

- [[reports/phase-1]]: phase 1 report (needs review)
- [[designs/runner-v2]]: runner redesign
- Current work: [[TER-12]], blocked on [[TER-15]]
- Background: [[MEM-1]]
- Release under review: [[v0.1.0]]
- Elsewhere: [[project:webshop/index]]
`,
    "reports/phase-1": `---
status: ready_for_review
reviewer: operator
---
# Phase 1 report

The daemon supervises one Claude agent end to end. Resume works after a
container restart.

![Dashboard screenshot](artifact:ART-1)

## Results

| Check | Result |
|---|---|
| Hooks captured | 14 / 14 |
| Resume after restart | pass |
| PTY resize | in progress ([[TER-12]]) |

Full log: [test output](artifact:ART-2)

\`\`\`artifact ART-3
\`\`\`
`,
    "designs/runner-v2": `# Runner v2

The runner owns the PTY ([[MEM-1]]). It types pushed turns only when the agent
is \`idle\`.

\`\`\`rust
fn push_turn(&mut self, text: &str) -> Result<()> {
    self.pty.write_all(text.as_bytes())
}
\`\`\`

See [[does/not-exist]] for the old design.
`,
    "webshop:index": `# Webshop wiki

- [[WEB-4]] checkout is in progress.
`,
  };

  const page = (id: string, project: string, path: string, title: string, status: WikiPage["status"], rev: number, min: number): WikiPage => ({
    id,
    project,
    path,
    title,
    ...(status ? { status } : {}),
    rev,
    updated_at: ago(min),
    author: lead.addr,
    author_name: lead.label,
    summary: rev === 1 ? "Create page" : `Update section ${rev - 1}`,
    created_by: lead.addr,
    created_at: ago(min + 60 * 24),
    body: pageBodies[project === "terrarium" ? path : `${project}:${path}`] ?? `# ${title}\n`,
    ...(status === "ready_for_review" ? { reviewer: "operator" } : {}),
    links: [],
  });

  const pages: WikiPage[] = [
    page(fakeUlid("pg", 1), "terrarium", "index", "Terrarium wiki", undefined, 5, 60 * 2),
    page(fakeUlid("pg", 2), "terrarium", "reports/phase-1", "Phase 1 report", "ready_for_review", 3, 35),
    page(fakeUlid("pg", 3), "terrarium", "designs/runner-v2", "Runner v2", "draft", 2, 60 * 5),
    page(fakeUlid("pg", 4), "webshop", "index", "Webshop wiki", "reviewed", 1, 60 * 24),
  ];

  const revisions: MockWorld["revisions"] = {};
  for (const pg of pages) {
    revisions[pg.id] = Array.from({ length: pg.rev }, (_, i) => {
      const a = who(i % 2 ? "runner-dev" : "lead");
      return {
        page: pg.id,
        rev: i + 1,
        author: a.addr,
        author_name: a.label,
        summary: i === 0 ? "Create page" : `Update section ${i}`,
        created_at: ago(60 * (pg.rev - i) + 5),
        body: i === pg.rev - 1 ? pg.body : `${pg.body}\n\n_(revision ${i + 1})_`,
      };
    });
  }

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="480" height="240" viewBox="0 0 480 240"><rect width="480" height="240" fill="#1f2a24"/><rect x="16" y="16" width="120" height="208" rx="6" fill="#2f3d35"/><g fill="#7fd1a3"><rect x="152" y="16" width="96" height="60" rx="6"/><rect x="264" y="16" width="96" height="60" rx="6" fill="#e6b450"/><rect x="376" y="16" width="88" height="60" rx="6" fill="#6cb6ff"/></g><text x="160" y="140" fill="#cfe" font-family="monospace" font-size="16">terrarium dashboard (mock)</text></svg>`;
  const artifactBodies: Record<string, string> = {
    "ART-1": `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`,
    "ART-2": `running 14 tests\ntest hooks::stop_once_per_turn ... ok\ntest runner::resume_after_restart ... ok\ntest runner::pty_resize ... FAILED\n\nfailures:\n  pty_resize: expected 120x40, got 80x24\n\ntest result: FAILED. 13 passed; 1 failed`,
    "ART-3": `<!doctype html><html><body style="font-family:sans-serif;background:#fffbe8;padding:1rem"><h2>Coverage report</h2><p>Line coverage: <b>71.4%</b></p><button onclick="this.textContent='Scripts run, but sandboxed'">Click me</button></body></html>`,
    "ART-4": `# Release notes v0.1.0\n\n- One Claude agent, supervised.\n- Resume after restart.\n- Dashboard skeleton.`,
    "ART-5": "data:application/pdf;base64,JVBERi0xLjQKMSAwIG9iajw8L1R5cGUvQ2F0YWxvZy9QYWdlcyAyIDAgUj4+ZW5kb2JqCjIgMCBvYmo8PC9UeXBlL1BhZ2VzL0tpZHNbMyAwIFJdL0NvdW50IDE+PmVuZG9iagozIDAgb2JqPDwvVHlwZS9QYWdlL01lZGlhQm94WzAgMCAzMDAgMTQ0XS9QYXJlbnQgMiAwIFIvUmVzb3VyY2VzPDwvRm9udDw8L0YxIDQgMCBSPj4+Pi9Db250ZW50cyA1IDAgUj4+ZW5kb2JqCjQgMCBvYmo8PC9UeXBlL0ZvbnQvU3VidHlwZS9UeXBlMS9CYXNlRm9udC9IZWx2ZXRpY2E+PmVuZG9iago1IDAgb2JqPDwvTGVuZ3RoIDQ0Pj5zdHJlYW0KQlQgL0YxIDE4IFRmIDIwIDcwIFRkIChNb2NrIFBERikgVGogRVQKZW5kc3RyZWFtCmVuZG9iagp0cmFpbGVyPDwvUm9vdCAxIDAgUj4+CiUlRU9G",
  };

  const versions: Version[] = [];
  const art = (n: number, name: string, content_type: string, links: Artifact["links"], min: number, kind: Artifact["kind"] = "file", embedded_in: string[] = []): Artifact => ({
    id: `ART-${n}`,
    key: `terrarium/ART-${n}`,
    uid: fakeUlid("art", n),
    project: "terrarium",
    name,
    content_type,
    size: (artifactBodies[`ART-${n}`] ?? "").length,
    sha256: `art${n}${"0".repeat(64)}`.slice(0, 64),
    kind,
    renders: true,
    created_by: lead.addr,
    created_by_name: lead.label,
    created_at: ago(min),
    links,
    embedded_in,
  });

  const artifacts: Artifact[] = [
    art(1, "dashboard.svg", "image/svg+xml", [], 40, "file", ["terrarium/reports/phase-1"]),
    art(2, "test-output.txt", "text/plain; charset=utf-8", [{ kind: "version", ref: fakeUlid("ver", 1), label: "v0.1.0" }], 41, "file", ["terrarium/reports/phase-1"]),
    art(3, "coverage", "text/html; charset=utf-8", [], 42, "bundle", ["terrarium/reports/phase-1"]),
    art(4, "release-notes.md", "text/markdown; charset=utf-8", [{ kind: "version", ref: fakeUlid("ver", 1), label: "v0.1.0" }], 50),
    art(5, "spec.pdf", "application/pdf", [], 60 * 3),
  ];

  const cardRef = (id: string) => {
    const c = cards.find((x) => x.id === id)!;
    return { id: c.id, title: c.title, column: c.column };
  };
  const artRef = (id: string) => {
    const a = artifacts.find((x) => x.id === id)!;
    return { id: a.id, uid: a.uid, name: a.name, kind: a.kind, content_type: a.content_type, size: a.size };
  };
  const shopLead = who("shop-lead");
  versions.push(
    {
      id: fakeUlid("ver", 0), project: "terrarium", name: "v0.0.1", status: "released", notes: "Empty daemon in Docker.",
      cards: ["TER-10", "TER-11"].map(cardRef), artifacts: [],
      reviews: [
        { id: fakeUlid("rev", 1), author: lead.addr, author_name: lead.label, decision: "submitted", comment: "", created_at: ago(60 * 24 * 2.5) },
        { id: fakeUlid("rev", 2), author: "operator", author_name: "operator", decision: "approved", comment: "Ship it.", created_at: ago(60 * 24 * 2) },
      ],
      created_by: lead.addr, created_by_name: lead.label, created_at: ago(60 * 24 * 3), submitted_at: ago(60 * 24 * 2.5), submitted_by: lead.addr,
      decided_at: ago(60 * 24 * 2), decided_by: "operator", released_at: ago(60 * 24 * 2), git_ref: "v0.0.1",
      url: `/p/terrarium/versions/${fakeUlid("ver", 0)}`,
    },
    {
      id: fakeUlid("ver", 1), project: "terrarium", name: "v0.1.0", status: "in_review", notes: "Phase 1: one Claude agent end to end. See [[reports/phase-1]].",
      cards: ["TER-12", "TER-16"].map(cardRef), artifacts: ["ART-2", "ART-4"].map(artRef),
      reviews: [{ id: fakeUlid("rev", 3), author: lead.addr, author_name: lead.label, decision: "submitted", comment: "Please look.", created_at: ago(50) }],
      created_by: lead.addr, created_by_name: lead.label, created_at: ago(60 * 2), submitted_at: ago(50), submitted_by: lead.addr, git_ref: "main@4f2a9c1",
      url: `/p/terrarium/versions/${fakeUlid("ver", 1)}`,
    },
    {
      id: fakeUlid("ver", 2), project: "webshop", name: "demo-2026-10-31", status: "draft", notes: "Demo build for Halloween.",
      cards: ["WEB-1"].map(cardRef), artifacts: [], reviews: [], created_by: shopLead.addr, created_by_name: shopLead.label, created_at: ago(60 * 6),
      url: `/p/webshop/versions/${fakeUlid("ver", 2)}`,
    },
  );

  const win = (window: string, used_pct: number, resets: number, readMin = 1) => ({ window, used_pct, resets_at: ahead(resets), at: ago(readMin) });
  const subBase = { source: "api" as const, created_at: ago(60 * 24 * 9), updated_at: ago(60), has_api_key: false, held_agents: 0 };
  const subscriptions: Subscription[] = [
    {
      ...subBase, name: "anthropic-max", provider: "anthropic", plan: "Max 20x", harnesses: ["claude", "pi"], probe: "anthropic-oauth-usage", alert_at: 80, source: "file",
      login_status: "logged_in", verified_at: ago(4), probe_status: "ok", last_probe_at: ago(1), last_success_at: ago(1),
      windows: [win("5h", 62, 97), win("7d", 38, 60 * 24 * 3 + 200)], local_estimate_only: false,
    },
    {
      ...subBase, name: "chatgpt-pro", provider: "openai", plan: "ChatGPT Pro", harnesses: ["codex", "pi"], probe: "openai-codex-usage", alert_at: 80,
      login_status: "expiring", verified_at: ago(30), probe_status: "ok", last_probe_at: ago(5), last_success_at: ago(5),
      windows: [win("5h", 87, 41, 5), win("7d", 54, 60 * 24 * 5, 5)], local_estimate_only: false,
    },
    {
      ...subBase, name: "copilot-pro", provider: "github", plan: "Copilot Pro+", harnesses: ["copilot"], probe: "github-copilot-quota", alert_at: 90,
      login_status: "logged_in", verified_at: ago(12), probe_status: "failed", probe_error: "the provider answered 401: the token has no Copilot Requests permission",
      last_probe_at: ago(2), last_success_at: ago(60 * 3), windows: [win("monthly", 23, 60 * 24 * 8, 60 * 3)], local_estimate_only: false,
    },
    {
      ...subBase, name: "openrouter-key", provider: "openrouter", plan: "API key", harnesses: ["pi"], alert_at: 80, env_var: "OPENROUTER_API_KEY", has_api_key: true,
      login_status: "logged_in", verified_at: ago(60 * 3), probe_status: "none", windows: [], local_estimate_only: true,
    },
    {
      ...subBase, name: "gemini", provider: "google", plan: "Gemini Code Assist", harnesses: ["pi"], alert_at: 80,
      login_status: "missing", probe_status: "none", windows: [], local_estimate_only: true,
    },
  ];
  const apiKeys: MockWorld["apiKeys"] = { "openrouter-key": { provider: "openrouter", key: "sk-or-mock-never-shown" } };

  // Quota samples: one per window per hour for a day, rising to the current value.
  const quota: QuotaSample[] = [];
  for (const s of subscriptions) {
    for (const w of s.windows) {
      for (let h = 23; h >= 0; h--) {
        const pct = Math.max(0, Math.round(w.used_pct - h * (w.used_pct / 30) + 3 * Math.sin(h)));
        quota.push({ subscription: s.name, window: w.window, used_pct: h === 0 ? w.used_pct : pct, resets_at: w.resets_at, raw: { utilization: pct, resets_at: w.resets_at }, at: ago(h * 60 + 1) });
      }
    }
  }
  quota.sort((a, b) => a.at.localeCompare(b.at));

  // Local accounting: a week of rows for every agent on a subscription.
  const MODELS: Record<string, string> = { claude: "claude-opus-5", codex: "gpt-5-codex", copilot: "claude-sonnet-5", pi: "anthropic/claude-sonnet" };
  const usage: UsageRow[] = [];
  for (const a of agents) {
    if (!a.subscription) continue;
    // The cards this agent worked on; some hours had no card in progress.
    const worked = cards.filter((c) => c.assignee === a.id && !["backlog", "ready"].includes(c.column)).map((c) => c.id);
    for (let h = 24 * 7 - 1; h >= 0; h--) {
      const k = (a.name.length * 31 + h * 7) % 13;
      if (k < 3) continue;
      const at = new Date(now - h * 3600_000 - 7 * 60_000).toISOString();
      const model = a.model ?? MODELS[a.harness]!;
      const card = worked.length && k % 3 !== 0 ? { card: worked[Math.floor(h / 8) % worked.length]! } : {};
      usage.push({ at, agent: a.id, project: a.project, subscription: a.subscription, model, input_tokens: k * 420, output_tokens: k * 90, cache_read: k * 15000, cache_write: k * 1200, messages: k, turn: 1000 + Math.floor(h / 2), ...card });
      if (a.harness === "claude" && k % 4 === 0) {
        usage.push({ at, agent: a.id, project: a.project, subscription: a.subscription, model: "claude-haiku-5-5", input_tokens: k * 40, output_tokens: k * 12, cache_read: k * 900, cache_write: 0, messages: 1, turn: 1000 + Math.floor(h / 2), ...card });
      }
    }
  }

  const msg = (id: string, thread: string, from: string, to: string, subject: string, body: string, state: Message["state"], min: number, extra: Partial<Message> = {}): Message => {
    const f = who(from);
    const t = who(to);
    return {
      id,
      thread_id: thread,
      from: f.addr,
      from_kind: f.kind,
      from_name: f.label,
      to: t.addr,
      to_kind: t.kind,
      to_name: t.label,
      subject,
      body,
      state,
      created_at: ago(min),
      ...(state !== "queued" ? { delivered_at: ago(min) } : {}),
      ...(state === "read" || state === "replied" ? { read_at: ago(min - 1) } : {}),
      ...extra,
    };
  };

  const mail: Message[] = [
    msg(fakeUlid("msg", 1), fakeUlid("th", 1), "project:terrarium", "operator", "TER-15 blocked: Codex adapter: resume by session id", "Card TER-15 moved to blocked.\n\n`codex login status` fails in the image. Needs an operator login.", "delivered", 11),
    msg(fakeUlid("msg", 2), fakeUlid("th", 1), "lead", "adapter-codex", "Re: TER-15 blocked: Codex adapter: resume by session id", "Pick up TER-13 while we wait for the login.", "read", 9, { in_reply_to: fakeUlid("msg", 1) }),
    msg(fakeUlid("msg", 3), fakeUlid("th", 2), "lead", "operator", "v0.1.0 ready for review", "Version v0.1.0 is in review. Open Versions in the dashboard.", "delivered", 50),
    msg(fakeUlid("msg", 4), fakeUlid("th", 3), "shop-lead", "project:terrarium", "bug: terrarium card ls ignores --column", "Running `terrarium card ls --column ready` lists every card. Seen from webshop.", "delivered", 70),
    msg(fakeUlid("msg", 5), fakeUlid("th", 4), "runner-dev", "lead", "PTY resize approach", "Going with SIGWINCH via portable-pty. Shout if you disagree.", "replied", 45),
    msg(fakeUlid("msg", 6), fakeUlid("th", 5), "lead", "project:webshop", "Shared CSS tokens?", "Can webshop reuse the dashboard CSS variables?", "delivered", 5),
  ].sort((x, y) => x.created_at.localeCompare(y.created_at));

  const targets: Target[] = [
    { id: "lab-win11", name: "lab-win11", kind: "vmlab", project: "terrarium", host: "lab1", scope: "project", state: "down", ephemeral: true, keep: false, snapshot_before_task: true, snapshots: ["clean"], source: "file", usable: true },
    { id: "build-box", name: "build-box", kind: "ssh", address: "build@buildbox.lan", scope: "global", state: "unknown", ephemeral: false, keep: false, snapshot_before_task: false, snapshots: [], source: "file", usable: true },
    { id: "pg-test", name: "pg-test", kind: "docker", project: "webshop", host: "lab1", scope: "project", state: "up", ephemeral: true, keep: false, snapshot_before_task: false, snapshots: [], source: "api", usable: true },
    { id: "pi-runner", name: "pi-runner", kind: "host", host: "pi-cluster", scope: "global", state: "unknown", ephemeral: false, keep: false, snapshot_before_task: false, snapshots: [], source: "file", usable: true },
  ];

  const hosts: Host[] = [
    {
      name: "lab1", id: fakeUlid("host", 1), host_id: fakeUlid("host", 1), status: "online", online: true, sessions: 1, max_sessions: 32, connected_at: ago(60), os: "linux", arch: "x86_64", version: "0.1.0", last_seen: ago(0.1),
      capabilities: { docker: { present: true, version: "29.8.0" }, vmlab: { present: true, version: "0.30.0-alpha" }, shell: "/bin/bash" },
    },
    {
      name: "pi-cluster", id: fakeUlid("host", 2), host_id: fakeUlid("host", 2), status: "offline", online: false, sessions: 0, os: "linux", arch: "aarch64", version: "0.0.9", last_seen: ago(60 * 7),
      capabilities: { docker: { present: true, version: "27.1.1" }, vmlab: { present: false }, shell: "/bin/sh" },
    },
  ];
  const hostTokens: Record<string, string> = { lab1: "th-mock-lab1", "pi-cluster": "th-mock-pi" };

  const auditRow = (n: number, min: number, actor: string, action: string, target: string, project: string | undefined, detail: Record<string, unknown>): AuditEntry => ({
    id: fakeUlid("aud", n),
    seq: n,
    at: ago(min),
    actor,
    action,
    target,
    ...(project ? { project } : {}),
    detail,
  });
  const audit: AuditEntry[] = [
    auditRow(1, 60 * 24 * 9, "system", "project.create", "terrarium", "terrarium", { card_prefix: "TER", source: "file" }),
    auditRow(2, 60 * 24 * 4, "operator", "project.create", "webshop", "webshop", { card_prefix: "WEB", source: "api" }),
    auditRow(3, 60 * 5, `agent:${byName("lead").id}`, "agent.roles", byName("reviewer").id, "terrarium", { added: [], removed: ["project_admin"] }),
    auditRow(4, 60 * 3, "operator", "agent.stop", byName("sysadmin").id, "terrarium", {}),
    auditRow(5, 26, "system", "agent.restart", byName("css-worker").id, "webshop", { reason: "3 restarts in 10 minutes" }),
  ];

  return {
    projects, repos, agents, sessions, events, columns: { terrarium: COLUMNS.map((c) => ({ ...c })), webshop: COLUMNS.map((c) => ({ ...c })) },
    cards, memories, links, pages, revisions, artifacts, artifactBodies, versions, subscriptions, quota, usage, holds: [], apiKeys, mail, targets, hosts, hostTokens, audit, seq,
  };
}

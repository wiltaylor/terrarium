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
  MemoryLink,
  Message,
  Project,
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
  events: AgentEvent[];
  columns: Record<string, Column[]>;
  cards: Card[];
  memories: Memory[];
  links: MemoryLink[];
  pages: WikiPage[];
  revisions: Record<string, (Revision & { body: string })[]>;
  artifacts: Artifact[];
  artifactBodies: Record<string, string>;
  versions: Version[];
  subscriptions: Subscription[];
  mail: Message[];
  targets: Target[];
  hosts: Host[];
  audit: AuditEntry[];
}

const COLUMNS: Column[] = [
  { name: "backlog", position: 0 },
  { name: "ready", position: 1 },
  { name: "in_progress", position: 2, wip_limit: 3 },
  { name: "review", position: 3 },
  { name: "blocked", position: 4 },
  { name: "done", position: 5 },
];

export function seed(now: number = Date.now()): MockWorld {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  const ahead = (min: number) => new Date(now + min * 60_000).toISOString();

  const projects: Project[] = [
    { name: "terrarium", description: "The harness wrapper itself.", card_prefix: "TER", created_at: ago(60 * 24 * 9) },
    { name: "webshop", description: "Storefront rewrite in Rust + HTMX.", card_prefix: "WEB", created_at: ago(60 * 24 * 4) },
  ];

  const repos: Repo[] = [
    { id: "repo-1", project: "terrarium", url: "git@github.com:wil/terrarium.git", default_branch: "main", path: "terrarium" },
    { id: "repo-2", project: "webshop", url: "git@github.com:wil/webshop.git", default_branch: "main", path: "webshop" },
  ];

  const ev = (id: number, min: number, kind: AgentEvent["kind"], agent: string, project: string, summary: string, data?: Record<string, unknown>): AgentEvent => ({
    id: `EV-${id}`,
    at: ago(min),
    kind,
    agent,
    project,
    summary,
    data,
  });

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
    roles: ["member"],
    workspace: `/state/workspaces/${project}/${name}`,
    created_at: ago(60 * 24 * 3),
    ...extra,
  });

  const agents: Agent[] = [
    agentRow("01JB0LEAD00000000000000001", "lead", "terrarium", "claude", "anthropic-max", "idle", {
      roles: ["project_admin"],
      last_event: ev(1, 3, "turn_end", "lead", "terrarium", "Turn ended after 41s"),
    }),
    agentRow("01JB0WRK100000000000000002", "runner-dev", "terrarium", "claude", "anthropic-max", "working", {
      current_card: "TER-12",
      turn_started_at: ago(4),
      last_event: ev(2, 0.2, "tool_call", "runner-dev", "terrarium", "Edit crates/terrariumd/src/runner.rs"),
    }),
    agentRow("01JB0WRK200000000000000003", "adapter-codex", "terrarium", "codex", "chatgpt-pro", "blocked", {
      model: "gpt-5-codex",
      current_card: "TER-15",
      last_event: ev(3, 11, "card", "adapter-codex", "terrarium", "Moved TER-15 to blocked: needs a Codex login in the image"),
    }),
    agentRow("01JB0WRK300000000000000004", "reviewer", "terrarium", "pi", "openrouter-key", "starting", {
      model: "anthropic/claude-sonnet",
      last_event: ev(4, 0.5, "status", "reviewer", "terrarium", "Starting session", { status: "starting" }),
    }),
    agentRow("01JB0WEB100000000000000005", "shop-lead", "webshop", "copilot", "copilot-pro", "working", {
      roles: ["project_admin"],
      current_card: "WEB-4",
      turn_started_at: ago(9),
      last_event: ev(5, 1, "tool_call", "shop-lead", "webshop", "Bash cargo test -p checkout"),
    }),
    agentRow("01JB0WEB200000000000000006", "css-worker", "webshop", "codex", "chatgpt-pro", "error", {
      last_event: ev(6, 26, "exit", "css-worker", "webshop", "Exited with code 1; restart budget exhausted"),
    }),
    agentRow("01JB0SYS000000000000000007", "sysadmin", "terrarium", "claude", "anthropic-max", "stopped", {
      roles: ["sysadmin"],
      last_event: ev(7, 180, "status", "sysadmin", "terrarium", "Stopped by operator", { status: "stopped" }),
    }),
  ];

  const sessions: Session[] = agents.flatMap((a, i) => [
    { id: `SES-${i}-1`, agent: a.id, harness_session_id: `${(i * 7919).toString(16)}-0a1b-4c2d-9e8f-${a.name}`, started_at: ago(60 * 26), ended_at: ago(60 * 20), turns: 12 + i, current: false },
    { id: `SES-${i}-2`, agent: a.id, harness_session_id: `${(i * 104729).toString(16)}-77aa-4bbb-8ccc-${a.name}`, started_at: ago(60 * 3), turns: 3 + i, current: true },
  ]);

  const events: AgentEvent[] = [
    ...agents.map((a) => a.last_event!).filter(Boolean),
    ev(10, 6, "turn_start", "runner-dev", "terrarium", "Turn started: implement PTY resize"),
    ev(11, 5, "tool_call", "runner-dev", "terrarium", "Read crates/terrariumd/src/pty.rs"),
    ev(12, 14, "mail", "lead", "terrarium", "Mail to adapter-codex: re TER-15"),
    ev(13, 20, "memory", "runner-dev", "terrarium", "Wrote MEM-4: portable-pty needs a resize after spawn"),
    ev(14, 35, "wiki", "lead", "terrarium", "Saved reports/phase-1 rev 3 (ready_for_review)"),
    ev(15, 50, "version", "lead", "terrarium", "Submitted v0.1.0 for review"),
    ev(16, 8, "card", "shop-lead", "webshop", "Claimed WEB-4"),
  ].sort((a, b) => b.at.localeCompare(a.at));

  let pos = 0;
  const card = (id: string, project: string, column: string, title: string, extra: Partial<Card> = {}): Card => ({
    id,
    project,
    column,
    position: pos++,
    title,
    body: extra.body ?? `Details for ${title.toLowerCase()}.`,
    labels: [],
    depends_on: [],
    comments: [],
    links: [],
    created_by: "lead",
    created_at: ago(60 * 24),
    updated_at: ago(30),
    ...extra,
  });

  const cards: Card[] = [
    card("TER-10", "terrarium", "done", "Scaffold the Cargo workspace", { assignee: "lead", labels: ["infra"] }),
    card("TER-11", "terrarium", "done", "Dockerfile with pinned harnesses", { assignee: "lead", labels: ["infra", "docker"] }),
    card("TER-12", "terrarium", "in_progress", "PTY resize follows the browser", {
      assignee: "runner-dev",
      labels: ["runner"],
      branch: "runner-dev/pty-resize",
      repo: "repo-1",
      comments: [
        { author: "runner-dev", body: "Claimed. Branch `runner-dev/pty-resize`.", created_at: ago(40) },
        { author: "runner-dev", body: "SIGWINCH reaches the child. Writing a test with the fake TUI.", created_at: ago(12) },
      ],
    }),
    card("TER-13", "terrarium", "ready", "Stall timeout marks agents blocked", { labels: ["runner"] }),
    card("TER-14", "terrarium", "ready", "Board WIP limits in the API", { labels: ["api"], depends_on: ["TER-12"] }),
    card("TER-15", "terrarium", "blocked", "Codex adapter: resume by session id", {
      assignee: "adapter-codex",
      labels: ["adapter", "codex"],
      comments: [{ author: "adapter-codex", body: "Blocked: `codex login status` fails in the image. Needs an operator login.", created_at: ago(11) }],
    }),
    card("TER-16", "terrarium", "review", "Mailbox push path", { assignee: "lead", labels: ["mail"], branch: "lead/mail-push" }),
    card("TER-17", "terrarium", "backlog", "Memory graph FTS5 index", { labels: ["memory"] }),
    card("TER-18", "terrarium", "backlog", "Artifact origin with signed URLs", { labels: ["artifacts", "security"] }),
    card("WEB-1", "webshop", "done", "Product listing page", { assignee: "shop-lead" }),
    card("WEB-4", "webshop", "in_progress", "Checkout flow", { assignee: "shop-lead", labels: ["checkout"] }),
    card("WEB-5", "webshop", "blocked", "Dark mode stylesheet", {
      assignee: "css-worker",
      comments: [{ author: "css-worker", body: "Blocked: the harness crashed twice on the same prompt.", created_at: ago(26) }],
    }),
    card("WEB-6", "webshop", "backlog", "Order emails"),
  ];

  const mem = (id: string, title: string, tags: string[], author: string, min: number, extra: Partial<Memory> = {}): Memory => ({
    id,
    project: "terrarium",
    title,
    body: extra.body ?? `${title}. Seen while working on the runner.`,
    tags,
    author,
    sources: [],
    created_at: ago(min),
    updated_at: ago(min),
    ...extra,
  });

  const memories: Memory[] = [
    mem("MEM-1", "The runner owns the PTY, not the supervisor", ["runner", "architecture"], "lead", 60 * 30, { sources: [{ kind: "path", ref: "crates/terrariumd/src/runner.rs" }] }),
    mem("MEM-2", "Claude Code fires Stop once per turn", ["hooks", "claude"], "lead", 60 * 28),
    mem("MEM-3", "Codex hooks need notify in config.toml", ["hooks", "codex"], "adapter-codex", 60 * 20),
    mem("MEM-4", "portable-pty needs a resize after spawn", ["runner", "pty"], "runner-dev", 20, { sources: [{ kind: "card", ref: "TER-12" }] }),
    mem("MEM-5", "xterm.js resize must debounce", ["dashboard", "pty"], "runner-dev", 15),
    mem("MEM-6", "Stall timeout default is 10 minutes", ["runner"], "lead", 60 * 10),
    mem("MEM-7", "Stall timeout default is 5 minutes", ["runner"], "lead", 60 * 40, { superseded_by: "MEM-6" }),
    mem("MEM-8", "bwrap needs user namespaces in Docker", ["sandbox", "docker"], "lead", 60 * 50, { sources: [{ kind: "commit", ref: "a1b2c3d" }] }),
  ];

  const link = (from: string, to: string, relation: MemoryLink["relation"], author = "lead"): MemoryLink => ({
    from,
    to,
    relation,
    author,
    created_at: ago(60),
  });

  const links: MemoryLink[] = [
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
    status,
    current_rev: rev,
    updated_at: ago(min),
    updated_by: "lead",
    body: pageBodies[project === "terrarium" ? path : `${project}:${path}`] ?? `# ${title}\n`,
    reviewer: status === "ready_for_review" ? "operator" : undefined,
  });

  const pages: WikiPage[] = [
    page("PG-1", "terrarium", "index", "Terrarium wiki", "reviewed", 5, 60 * 2),
    page("PG-2", "terrarium", "reports/phase-1", "Phase 1 report", "ready_for_review", 3, 35),
    page("PG-3", "terrarium", "designs/runner-v2", "Runner v2", "draft", 2, 60 * 5),
    page("PG-4", "webshop", "index", "Webshop wiki", "reviewed", 1, 60 * 24),
  ];

  const revisions: MockWorld["revisions"] = {};
  for (const pg of pages) {
    revisions[pg.id] = Array.from({ length: pg.current_rev }, (_, i) => ({
      rev: i + 1,
      author: i % 2 ? "runner-dev" : "lead",
      summary: i === 0 ? "Create page" : `Update section ${i}`,
      created_at: ago(60 * (pg.current_rev - i) + 5),
      body: i === pg.current_rev - 1 ? pg.body : `${pg.body}\n\n_(revision ${i + 1})_`,
    }));
  }

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="480" height="240" viewBox="0 0 480 240"><rect width="480" height="240" fill="#1f2a24"/><rect x="16" y="16" width="120" height="208" rx="6" fill="#2f3d35"/><g fill="#7fd1a3"><rect x="152" y="16" width="96" height="60" rx="6"/><rect x="264" y="16" width="96" height="60" rx="6" fill="#e6b450"/><rect x="376" y="16" width="88" height="60" rx="6" fill="#6cb6ff"/></g><text x="160" y="140" fill="#cfe" font-family="monospace" font-size="16">terrarium dashboard (mock)</text></svg>`;
  const artifactBodies: Record<string, string> = {
    "ART-1": `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`,
    "ART-2": `running 14 tests\ntest hooks::stop_once_per_turn ... ok\ntest runner::resume_after_restart ... ok\ntest runner::pty_resize ... FAILED\n\nfailures:\n  pty_resize: expected 120x40, got 80x24\n\ntest result: FAILED. 13 passed; 1 failed`,
    "ART-3": `<!doctype html><html><body style="font-family:sans-serif;background:#fffbe8;padding:1rem"><h2>Coverage report</h2><p>Line coverage: <b>71.4%</b></p><button onclick="this.textContent='Scripts run, but sandboxed'">Click me</button></body></html>`,
    "ART-4": `# Release notes v0.1.0\n\n- One Claude agent, supervised.\n- Resume after restart.\n- Dashboard skeleton.`,
    "ART-5": "data:application/pdf;base64,JVBERi0xLjQKMSAwIG9iajw8L1R5cGUvQ2F0YWxvZy9QYWdlcyAyIDAgUj4+ZW5kb2JqCjIgMCBvYmo8PC9UeXBlL1BhZ2VzL0tpZHNbMyAwIFJdL0NvdW50IDE+PmVuZG9iagozIDAgb2JqPDwvVHlwZS9QYWdlL01lZGlhQm94WzAgMCAzMDAgMTQ0XS9QYXJlbnQgMiAwIFIvUmVzb3VyY2VzPDwvRm9udDw8L0YxIDQgMCBSPj4+Pi9Db250ZW50cyA1IDAgUj4+ZW5kb2JqCjQgMCBvYmo8PC9UeXBlL0ZvbnQvU3VidHlwZS9UeXBlMS9CYXNlRm9udC9IZWx2ZXRpY2E+PmVuZG9iago1IDAgb2JqPDwvTGVuZ3RoIDQ0Pj5zdHJlYW0KQlQgL0YxIDE4IFRmIDIwIDcwIFRkIChNb2NrIFBERikgVGogRVQKZW5kc3RyZWFtCmVuZG9iagp0cmFpbGVyPDwvUm9vdCAxIDAgUj4+CiUlRU9G",
  };

  const art = (id: string, name: string, content_type: string, links: Artifact["links"], min: number, kind: Artifact["kind"] = "file"): Artifact => ({
    id,
    project: "terrarium",
    name,
    content_type,
    size: (artifactBodies[id] ?? "").length,
    sha256: `${id.toLowerCase().replace("-", "")}${"0".repeat(56)}`.slice(0, 64),
    kind,
    created_by: "lead",
    created_at: ago(min),
    url: artifactBodies[id]!.startsWith("data:")
      ? artifactBodies[id]!
      : `data:${content_type};charset=utf-8,${encodeURIComponent(artifactBodies[id]!)}`,
    links,
  });

  const artifacts: Artifact[] = [
    art("ART-1", "dashboard.svg", "image/svg+xml", [{ kind: "page", ref: "reports/phase-1" }], 40),
    art("ART-2", "test-output.txt", "text/plain", [{ kind: "page", ref: "reports/phase-1" }, { kind: "version", ref: "VER-1" }], 41),
    art("ART-3", "coverage", "text/html", [{ kind: "page", ref: "reports/phase-1" }], 42, "bundle"),
    art("ART-4", "release-notes.md", "text/markdown", [{ kind: "version", ref: "VER-1" }], 50),
    art("ART-5", "spec.pdf", "application/pdf", [], 60 * 3),
  ];

  const versions: Version[] = [
    {
      id: "VER-0", project: "terrarium", name: "v0.0.1", status: "released", notes: "Empty daemon in Docker.",
      cards: ["TER-10", "TER-11"], artifacts: [], reviews: [{ author: "operator", decision: "approve", comment: "Ship it.", created_at: ago(60 * 24 * 2) }],
      created_by: "lead", created_at: ago(60 * 24 * 3), submitted_at: ago(60 * 24 * 2.5), decided_at: ago(60 * 24 * 2), decided_by: "operator", git_ref: "v0.0.1",
    },
    {
      id: "VER-1", project: "terrarium", name: "v0.1.0", status: "in_review", notes: "Phase 1: one Claude agent end to end. See [[reports/phase-1]].",
      cards: ["TER-12", "TER-16"], artifacts: ["ART-2", "ART-4"], reviews: [], created_by: "lead", created_at: ago(60 * 2), submitted_at: ago(50), git_ref: "main@4f2a9c1",
    },
    {
      id: "VER-2", project: "webshop", name: "demo-2026-10-31", status: "draft", notes: "Demo build for Halloween.",
      cards: ["WEB-1"], artifacts: [], reviews: [], created_by: "shop-lead", created_at: ago(60 * 6),
    },
  ];

  const hist = (base: number, slope: number) => Array.from({ length: 24 }, (_, i) => Math.max(0, Math.min(100, Math.round(base + slope * i + 6 * Math.sin(i / 2)))));

  const subscriptions: Subscription[] = [
    {
      name: "anthropic-max", provider: "anthropic", plan: "Max 20x", harnesses: ["claude", "pi"], probe: "anthropic-oauth-usage", alert_at: 80,
      status: "logged_in", verified_at: ago(4), last_poll_at: ago(1),
      windows: [{ window: "5h", used_pct: 62, resets_at: ahead(97) }, { window: "7d", used_pct: 38, resets_at: ahead(60 * 24 * 3 + 200) }],
      history: hist(10, 2.2),
    },
    {
      name: "chatgpt-pro", provider: "openai", plan: "ChatGPT Pro", harnesses: ["codex", "pi"], probe: "codex-rate-limits", alert_at: 80,
      status: "expiring", verified_at: ago(30), last_poll_at: ago(5),
      windows: [{ window: "primary", used_pct: 87, resets_at: ahead(41) }, { window: "secondary", used_pct: 54, resets_at: ahead(60 * 24 * 5) }],
      history: hist(40, 2),
    },
    {
      name: "copilot-pro", provider: "github", plan: "Copilot Pro+", harnesses: ["copilot"], probe: "copilot-premium-requests", alert_at: 90,
      status: "logged_in", verified_at: ago(12), last_poll_at: ago(12),
      windows: [{ window: "monthly", used_pct: 23, resets_at: ahead(60 * 24 * 8) }],
      history: hist(18, 0.2),
    },
    {
      name: "openrouter-key", provider: "openrouter", plan: "API key", harnesses: ["pi"], probe: null, alert_at: 80,
      status: "logged_in", verified_at: ago(60 * 3), windows: [], history: hist(0, 0),
    },
    {
      name: "gemini", provider: "google", plan: "Gemini Code Assist", harnesses: ["pi"], probe: "gemini-stats", alert_at: 80,
      status: "missing", windows: [], history: [],
    },
  ];

  const msg = (id: string, thread: string, from: string, to: string, subject: string, body: string, state: Message["state"], min: number): Message => ({
    id, thread_id: thread, from, to, subject, body, state, created_at: ago(min),
  });

  const mail: Message[] = [
    msg("MSG-1", "TH-1", "adapter-codex", "operator", "TER-15 blocked", "Card TER-15 moved to blocked.\n\n`codex login status` fails in the image. Needs an operator login.", "delivered", 11),
    msg("MSG-2", "TH-1", "lead", "adapter-codex", "Re: TER-15 blocked", "Pick up TER-13 while we wait for the login.", "read", 9),
    msg("MSG-3", "TH-2", "lead", "operator", "v0.1.0 ready for review", "Version v0.1.0 is in review. Open Versions in the dashboard.", "delivered", 50),
    msg("MSG-4", "TH-3", "shop-lead", "project:terrarium", "bug: terrarium card ls ignores --column", "Running `terrarium card ls --column ready` lists every card. Seen from webshop.", "delivered", 70),
    msg("MSG-5", "TH-4", "runner-dev", "lead", "PTY resize approach", "Going with SIGWINCH via portable-pty. Shout if you disagree.", "replied", 45),
    msg("MSG-6", "TH-5", "lead", "project:webshop", "Shared CSS tokens?", "Can webshop reuse the dashboard CSS variables?", "queued", 5),
  ];

  const targets: Target[] = [
    { id: "TGT-1", name: "lab-win11", kind: "vmlab", project: "terrarium", host: "lab1", state: "down", ephemeral: true, snapshots: ["clean", "after-setup"] },
    { id: "TGT-2", name: "build-box", kind: "ssh", host: undefined, state: "up", ephemeral: false, snapshots: [] },
    { id: "TGT-3", name: "pg-test", kind: "docker", project: "webshop", host: "lab1", state: "up", ephemeral: true, snapshots: [] },
    { id: "TGT-4", name: "lab1-host", kind: "host", host: "lab1", state: "up", ephemeral: false, snapshots: [] },
  ];

  const hosts: Host[] = [
    { name: "lab1", connected: true, os: "linux", arch: "x86_64", capabilities: { docker: true, vmlab: true }, last_seen: ago(0.1), version: "0.1.0" },
    { name: "pi-cluster", connected: false, os: "linux", arch: "aarch64", capabilities: { docker: true, vmlab: false }, last_seen: ago(60 * 7), version: "0.0.9" },
  ];

  const audit: AuditEntry[] = [
    { id: "AU-1", at: ago(4), actor: "operator", action: "subscription.verify", target: "anthropic-max" },
    { id: "AU-2", at: ago(26), actor: "supervisor", action: "agent.restart_budget_exhausted", target: "css-worker", detail: "3 restarts in 10 minutes" },
    { id: "AU-3", at: ago(60 * 3), actor: "operator", action: "agent.stop", target: "sysadmin" },
    { id: "AU-4", at: ago(60 * 5), actor: "lead", action: "roles.grant", target: "reviewer", detail: "member" },
    { id: "AU-5", at: ago(60 * 24), actor: "operator", action: "subscription.api_key_set", target: "openrouter-key" },
  ];

  return {
    projects, repos, agents, sessions, events, columns: { terrarium: COLUMNS.map((c) => ({ ...c })), webshop: COLUMNS.map((c) => ({ ...c })) },
    cards, memories, links, pages, revisions, artifacts, artifactBodies, versions, subscriptions, mail, targets, hosts, audit,
  };
}

# Terrarium — Plan

Terrarium wraps AI coding harnesses (Claude Code, Codex, Copilot CLI) so that
one API, one CLI, one dashboard, and one status model apply to all of them.

Status: draft v0.19, 2026-10-25. Phases 0 to 5 largely built; Phase 6 partly (the fleet file is done). Target leases and mode 1 (workspaces on the host, attached into targets) are built and verified live. The security review fixes F1 to F7 and F10 to F14 are in, with cookie sessions and the CSP header (`docs/security-review.md`). See §20, "Build status", and README.md.

---

## 1. Decisions made

These are settled. The rest of the plan follows from them.

| # | Decision |
|---|---|
| D1 | Backend in **Rust**. Dashboard in **SolidJS**, served by the daemon. |
| D2 | Adapters for **Claude Code**, **Codex**, **Copilot CLI**, and **pi**. pi runs any model we hold a subscription for, through its OAuth logins. |
| D3 | Harnesses always run in **interactive mode** in a real PTY. No headless mode. |
| D4 | Every agent runs in **yolo mode**. The harness never asks for approval. |
| D5 | Agents reach Terrarium through a **CLI** inside their sandbox, not MCP. |
| D6 | Terrarium runs in **one Docker container**. Harness auth lives in that container on a volume. You log in once. All agents share it. |
| D7 | Each agent is **sandboxed** inside that container. It sees only its own files and the shared harness settings. Everything else is reached through the Terrarium CLI. |
| D8 | A **web dashboard** shows agents, status, mail, targets, and live terminals. |
| D9 | vmlab and Docker do not run inside the container. A **host agent** installed on each outside host runs them for the daemon. |
| D10 | Every agent belongs to one **project**. A project groups agents, targets, and git repositories. Agents discover and mail peers in their own project by default. |
| D11 | Each project has a **kanban board**. Agents track their work on it through the CLI. The operator sees and edits it in the dashboard. |
| D12 | Each project has a **memory graph**: nodes of knowledge that agents write, search, and link to each other. Project scoped. |
| D13 | Each project has an **inbox**. Projects message each other through inboxes: raise a bug upstream, get a notification back when it is done. |
| D14 | Agents can hold a **sysadmin** role (global) or a **project admin** role. Leader agents use these to manage the system and their projects. |
| D15 | Each project has a **wiki**: a publishing surface where agents put Markdown pages and files for the user to preview, instead of starting an ad-hoc file server. Full revision history. |
| D16 | **Artifacts** (files and static bundles) and **versions** (releases with notes, linked cards, and attached artifacts) are first-class. The user reviews them in the dashboard and approves or rejects. |
| D17 | **Subscriptions** are first-class. The dashboard home shows usage and remaining quota for every subscription Terrarium is logged in to, and which agents are spending it. |
| D18 | The dashboard **registers subscriptions**: it drives each harness's login flow in an embedded terminal, and takes API keys for pi through a form. No shell access needed to log in. |
| D19 | The repo ships a **justfile**, a **Dockerfile**, and a **compose file**. Every harness config directory is a bind mount from the host, so settings are editable from outside the container. |

---

## 2. Goals

1. Run any supported harness as a managed, durable agent.
2. Expose each agent through one API: start, stop, send, attach, status.
3. Give agents a mailbox so they can message each other.
   Group agents into projects, so an agent finds the peers that matter.
4. Map a stable Terrarium ID to the harness's own session ID. A restart
   resumes the same conversation.
5. Quiet each harness: no trust dialogs, no update checks, no approval
   prompts, and a Terrarium-owned system prompt.
6. Track agent status: `starting`, `idle`, `working`, `blocked`, `error`,
   `stopped`, `restarting`.
7. Let an agent run commands on the machines it is allowed to reach, through
   the Terrarium CLI. Targets are permanent (a host with the host agent, an
   SSH host) or ephemeral (a Docker container or a vmlab VM that a host agent
   creates on demand).
8. Attach git repositories to a project, and check them out in an agent's
   workspace or on a remote target with one command.
9. Give each project a kanban board, so agents and the operator share one
   view of what is to do, in progress, blocked, and done.
10. Give each project a memory graph, so what one agent learns is findable by
    the next, and related knowledge is linked, not duplicated.
11. Let projects talk to each other through project inboxes, with threads that
    tie a request in one project to the card that resolves it in another.
12. Let trusted agents administer the system or a project, so leader agents
    can create workers, triage inboxes, and keep projects moving.
13. Give each project a wiki where agents publish pages and files for the
    user to preview in the browser. No more per-project Python file servers.
14. Track versions per project in the dashboard, with artifacts stored against
    them, so an agent can hand the user a build, a report, or a screenshot set
    for review and get an approve or reject back.
15. Show usage for every subscription on the dashboard home, so the operator
    sees how much of each plan is left and where it went.
16. Make logging in easy. Register Claude Code, Codex, Copilot, and pi API-key
    subscriptions from the dashboard, with the login flow driven for you.

## 3. Non-goals (v1)

- A multi-host control plane. One daemon, one container. Targets may be remote.
- A workflow engine. Terrarium gives agents a mailbox. It does not decide who
  talks to whom.
- Replacing the harness UI. The real TUI runs unchanged in the PTY.

---

## 4. Harness survey (installed versions, 2026-10-24)

| | Claude Code 2.1.310 | Codex 0.160.0 | Copilot CLI 1.0.103 | pi 0.85.1 |
|---|---|---|---|---|
| Interactive start | `claude` | `codex --no-daemon` (always: without it, Codex starts a shared app-server daemon that outlives the TUI) | `copilot` with `"experimental": true` in the per-agent `settings.json` (the `--experimental` flag rewrites that file by rename, which fails on a file bind) | `pi --provider <p> --model <m>` |
| Yolo | `--dangerously-skip-permissions` (refuses as root; `--allow-dangerously-skip-permissions` to override) | `--dangerously-bypass-approvals-and-sandbox` or `-a never -s danger-full-access` | `--yolo`, plus `COPILOT_ALLOW_ALL=true` to trust the folder | tools run without approval by default (verified); `--no-approve` skips the trust dialog |
| Set session ID on start | `--session-id <uuid>` | none seen (read it from the session file or hook event) | `--session-id <uuid>` | `--session-id <id>` |
| Resume | `--resume <id>` | `codex resume <id>` | `--resume <id>` or `--session-id <id>` | `--session <id>` or `--session-id <id>` |
| System prompt | `--system-prompt`, `--append-system-prompt` | `-c model_instructions_file=<file>` replaces, `-c developer_instructions=` appends (verified); frozen at session creation | [CopilotCustomSystemPrompt](https://github.com/wiltaylor/CopilotCustomSystemPrompt) extension: `~/.copilot/SYSTEM.md` replaces, `APPEND_SYSTEM.md` appends | `--system-prompt <text|file>`, `--append-system-prompt` |
| Hooks | yes, inject via `--settings <json>` | `-c 'hooks.<Event>=[...]'`, plus `--dangerously-bypass-hook-trust` (else a "Hooks need review" dialog blocks the start) | `--plugin-dir <dir>` with `hooks.json` at the plugin root. The shared home's own hooks also run. | no hooks: a Terrarium extension loaded with `-e <path>` sends the events to the runner socket |
| Config home env | `CLAUDE_CONFIG_DIR` | `CODEX_HOME` | `COPILOT_HOME` | `PI_CODING_AGENT_DIR` (`~/.pi/agent`: `auth.json`, `models.json`, `settings.json`, `sessions/`), `PI_CODING_AGENT_SESSION_DIR` |
| Trust dialog | pre-seed in `~/.claude.json` | `-c projects.<path>.trust_level="trusted"` (`--skip-git-repo-check` is an `exec` flag, and the TUI rejects it) | `COPILOT_ALLOW_ALL=true` | `--approve` or `--no-approve`; `--no-context-files` to skip AGENTS.md |
| Updates / telemetry off | `DISABLE_AUTOUPDATER=1`, `DISABLE_TELEMETRY=1` | `-c check_for_update_on_startup=false -c analytics.enabled=false -c feedback.enabled=false` | `COPILOT_AUTO_UPDATE=false`, `--disable-client-telemetry` | `PI_TELEMETRY=0`, `--offline` for startup network |
| Working dir | `cd` | `-C <dir>` | `-C <dir>` | `cd` |
| Clean exit | `/exit` | `/exit` | `/exit` | `/quit` (Ctrl-C does not exit) |
| Subscriptions | Anthropic OAuth | OpenAI OAuth | GitHub. On a host the token is in the system keychain (D-Bus). In a sandbox the daemon passes it as `COPILOT_GITHUB_TOKEN`. | OpenAI Codex, GitHub Copilot, Google, and API keys. pi cannot spend an Anthropic Pro/Max plan: Anthropic bills it as extra usage (`docs/compat.md`, "pi"). Use an Anthropic API key. `pi auth` prints and checks credentials. |
| Login without a browser callback | `claude auth login` (URL plus pasted code), `claude setup-token` for a long-lived token, `claude auth status` | `codex login --device-auth`, `--with-api-key` from stdin, `codex login status` | `copilot login --device-code`, `--with-token` from stdin, env tokens | `/login` inside the TUI per provider; API keys in `auth.json` or env; `pi auth check --json` |

Observations that shape the design:

- Interactive mode means the harness owns stdin. Terrarium talks to it by
  typing into the PTY. Terrarium learns its state from hooks, not from a JSON
  stream. Claude, Codex, and Copilot have hooks; pi has an extension API that
  the Terrarium extension uses the same way. These are the primary status signal.
- All three support a system prompt override from outside the workspace.
  Claude and Codex do it natively. Copilot needs the CopilotCustomSystemPrompt
  extension, which uses the Copilot SDK `systemMessage` API and keeps native
  auth, billing, tools, and model routing. Terrarium never edits the user's
  repo to inject a prompt.
- pi is the escape hatch. One harness, many subscriptions. Its `--provider`
  and `--model` flags pick which plan an agent spends, so Terrarium can route
  an agent to whichever subscription has headroom. The exception is an
  Anthropic plan: pi cannot spend it (`docs/compat.md`, "pi: facts that
  contradict the plan"). The daemon refuses that start.
- The flags change often. Each adapter pins the version it was tested against.
  A compatibility suite runs in CI.

---

## 5. Domain model

| Term | Meaning |
|---|---|
| **Harness** | A coding-agent CLI: `claude`, `codex`, `copilot`, `pi`. |
| **Subscription** | A plan Terrarium is logged in to: provider, plan name, how usage is measured, which harness homes carry its credentials. An agent spends exactly one subscription per session. |
| **Adapter** | Terrarium code that knows one harness: launch flags, hook config, prompt injection, session ID capture, resume. |
| **Project** | A named group. Owns agents, project-scoped targets, and repositories. Sets the default scope for peer discovery and mail. |
| **Repository** | A git remote attached to a project: URL, default branch, credential reference, and a checkout path name. |
| **Board** | The kanban board of a project. One per project. Has ordered columns. |
| **Column** | A named lane on a board, with an optional WIP limit. Defaults: `backlog`, `ready`, `in_progress`, `review`, `blocked`, `done`. |
| **Card** | One unit of work. Title, body, column, position, assignee (agent or operator), labels, repo and branch, dependencies, comments, linked mail threads. |
| **Memory** | One node of project knowledge. Title, body, tags, author, sources (card, thread, repo path or commit), superseded-by. |
| **Link** | A directed, labelled edge between two memories, with an author and an optional note. Relations: `relates_to`, `supports`, `contradicts`, `supersedes`, `derived_from`, `part_of`, `example_of`. |
| **Wiki page** | A Markdown document in a project's wiki, addressed by a path such as `reports/perf-week-3`. Has a revision history. Renders in the dashboard with embedded artifacts. |
| **Revision** | One saved version of a page: number, author, summary, body, timestamp. |
| **Artifact** | A file or a static bundle (a directory served as a site) stored by content hash. Has a name, content type, size, author. Attached to a wiki page, a version, or a card. Served at a stable preview URL. |
| **Version** | A named release point in a project: notes, status (`draft`, `in_review`, `approved`, `rejected`, `released`), repo ref, linked cards, attached artifacts, review comments. |
| **Agent** | A durable Terrarium identity. ULID, name, project, harness, workspace, system prompt, allowed targets, restart policy, status. Agent names are unique inside a project. |
| **Session** | One harness conversation bound to an agent. Holds the harness's session ID. An agent has one current session and a history. |
| **Turn** | One prompt-to-completion cycle. Starts at a prompt-submit hook. Ends at a stop hook. |
| **Event** | One normalised record: turn start, tool call, turn end, exit, mail. Produced from hooks, the PTY, and the supervisor. |
| **Mailbox** | The per-agent inbox. Messages with sender, recipient, thread, body, state. |
| **Message** | One mailbox item. States: `queued`, `delivered`, `read`, `replied`. Addressed to an agent or to a project inbox. |
| **Project inbox** | The mailbox of a project. Project admins read it. Other projects and the operator write to it. |
| **Role** | A permission bundle on an agent. `sysadmin` is global. `project_admin` is scoped to one project. Everything else is a plain `member`. |
| **Sandbox** | The restricted view of the container that a harness runs in. |
| **Target** | A machine the agent may run commands on: `host`, `ssh`, `docker`, `vmlab`. A target is either global or owned by one project. |
| **Executor** | The daemon component that runs a command on a target for an agent. |
| **Status** | `starting`, `idle`, `working`, `blocked`, `error`, `stopped`, `restarting`. |

Status rules:

- `working`: a turn is open (prompt-submit hook fired, stop hook has not).
- `idle`: no turn is open and the process is alive. In yolo mode this is the
  normal resting state.
- `blocked`: the harness asked the user a question, or a turn has been open
  with no hook and no PTY output for longer than the stall timeout.
- `error`: the process exited non-zero, or the restart budget is exhausted.
- `stopped`: a user or the API stopped the agent on purpose.

---

## 6. Architecture

```
  browser (SolidJS dashboard, xterm.js)      operator shell (terrarium CLI)
            \                                    /
             +------ HTTP + WebSocket (axum) ---+
                             |
                        terrariumd  --------------- SQLite
                  +----------+-----------+
                  |          |           |
             supervisor   mailbox    executors --> host agents (outside) --> docker, vmlab
                  |                        \--> ssh targets
                  |
            agent runner (one per agent, holds the PTY)
                  |
         bwrap sandbox ---------------------------------+
         |  harness TUI  <-- keystrokes / screen -->    |
         |  hooks  -> runner socket                     |
         |  `terrarium` CLI (agent mode) -> runner socket
         +----------------------------------------------+
```

Everything above runs inside **one Docker container**. The image contains the
harness binaries (`claude`, `codex`, `copilot`, and `pi` from `@earendil-works/pi-coding-agent`), Node 22 (22.19+), the `copilot-custom-system-prompt` npm package,
Rust-built Terrarium binaries, `bwrap`, and `ssh`. No Docker client and no vmlab: those run on outside hosts.
On first start the entrypoint runs `copilot-custom-system-prompt install` so
the extension lands in the Copilot home on the auth volume. On an image
upgrade it runs again with `--force`. The container mounts:

| Mount | Purpose |
|---|---|
| `./config/claude`, `./config/codex`, `./config/copilot`, `./config/pi` bind-mounted under `/auth` | One host directory per harness home, so you can edit settings, hooks, and extensions from outside the container with your normal editor. Log in once from the dashboard's Subscriptions page (§16.1), or with `docker compose exec terrarium claude auth login` and the others. See §23. |
| `./state` bind-mounted at `/state` | SQLite, per-agent workspaces and logs, artifacts, credentials. |
| `~/.ssh` or a key volume (optional) | For `ssh` targets. |
| `/var/run/docker.sock` (optional) | Only for the sibling-container sandbox fallback (§11). Never for targets. |

The container runs as a non-root user. Claude Code refuses yolo mode as root.

Components:

1. **`terrariumd`** — the daemon. Owns SQLite (projects, agents, sessions,
   messages, targets, repos), the HTTP and WebSocket API, the
   supervisor, the mailbox, executors, and the embedded dashboard assets.
2. **Agent runner** — one process per agent. Owns the PTY and the `bwrap`
   child. Records the raw PTY stream and a normalised event log. Exposes a Unix
   socket for hooks and the agent CLI. Survives a daemon restart.
3. **Adapter** — one per harness. See §7.
4. **`terrarium` CLI** — one binary, two modes. Operator mode talks to the
   daemon socket. Agent mode talks to the runner socket and is scoped by the
   agent's policy. See §9.
5. **Host agent** (`terrarium-host`) — a small Rust binary installed on a
   machine outside the container. It dials in to the daemon and runs commands,
   Docker, and vmlab on that machine. See §19.
6. **Dashboard** — SolidJS single-page app. See §17.

Storage layout inside the container:

```
/auth/claude, /auth/claude.json, /auth/codex, /auth/copilot, /auth/pi    shared harness homes
/state/subscriptions/<name>.toml       provider, plan, probe method, limits
                                       (usage samples and per-agent accounting live in SQLite)
/state/projects/<name>.toml           project: repositories, members, project targets
/state/credentials/                    git tokens and deploy keys, daemon-only
                                       (boards, cards, memories, links, wiki pages, revisions, versions live in SQLite; FTS5 for search)
/state/artifacts/<sha256>              artifact blobs and unpacked bundles, daemon-only
/state/terrarium.db
/state/agents/<ulid>/workspace/        the agent's files (rw in the sandbox)
/state/agents/<ulid>/harness/          prompt.md and the adapter's per-agent files (ro in the sandbox)
/state/agents/<ulid>/private/          runner records, never in the sandbox:
                                       events.jsonl, pty.log (ring + archive), runner.json,
                                       runner-argv.json, runner-exit.json, runner.log
/run/terrarium/api.sock
/run/terrarium/agents/<ulid>.sock
```

---

## 7. Adapter interface

```rust
trait Adapter {
    fn id(&self) -> HarnessId;
    fn tested_version(&self) -> VersionReq;
    fn detect_version(&self) -> Result<Version>;

    // Command line and env for a fresh start or a resume. Includes the yolo
    // flags, the system prompt, the hook config, and the quiet-mode env.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec;

    // Hook payloads arrive on the runner socket. Map them to events.
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event>;

    // Optional heuristics from the screen (vt100 parse). Used for
    // `blocked` detection and for "is the prompt ready" before typing.
    fn screen_hint(&self, screen: &Screen) -> ScreenHint;

    // Where the harness writes its session id if no hook carries it.
    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId>;

    // Keystrokes that submit a user turn. Usually: bracketed paste + Enter.
    fn type_turn(&self, text: &str) -> Vec<u8>;
}
```

Sending a turn into an interactive TUI:

1. Wait until status is `idle` and `screen_hint` says the prompt is ready.
2. Write the text with bracketed paste, then Enter.
3. Expect the prompt-submit hook within a short timeout. If it does not fire,
   retry once, then mark the delivery failed and raise an event.

Verified in Phase 0 (Claude Code 2.1.310, `docs/compat.md`):

- Text typed while a turn runs joins that turn. Two prompt-submit hooks
  fire, but only one stop hook. So step 1 is not enough. The runner checks
  the screen for `busy` just before it types, and it does not count stop
  hooks per delivery.
- The prompt-ready rule is strict: the input box is at the bottom, it is
  empty, the footer has no "esc to interrupt", and no dialog key hint
  shows. No input box means "not ready". `ScreenHint::NotReady` says so.
- Submit is bracketed paste, then CR. A bare LF adds a line and does not
  submit. The prompt-submit hook came 23 to 31 ms after the CR.
- The "needs input" hook is `PermissionRequest`. It fires for
  `AskUserQuestion` in bypass mode too. The adapter maps it to
  `Event::Question`. `Notification` is only the 60 s idle reminder. It does
  not mean `blocked`.
- Answer a question with Down (`ESC [B`) once for each step, then CR.

---

## 8. Quiet defaults and prompt control

The adapter's `launch` sets, per harness:

- **Yolo**: the flags in §4. Allowed because every agent is sandboxed (§11).
- **System prompt**: rendered from a Terrarium template. Variables: agent name
  and ULID, the project name and its repositories, the peers in the project
  (name, harness, and roles only, sorted by name; no status, so a peer status
  change does not change the prompt hash and stop a resume; the agent runs
  `terrarium agents ls` for the live status),
  the mailbox protocol, the board protocol (§13), the memory protocol (§14), the wiki, artifact, and version protocol (§15), the agent's roles and,
  for leaders, the leader duties (§10), the list of allowed targets, how to use the
  `terrarium` CLI, and the rule that mail is untrusted input.
- **Hooks**: a config that calls `terrarium hook <event>` on prompt submit,
  tool use, stop, and notification. The CLI forwards the JSON payload to the
  runner socket. Hooks are passed by flag or env where possible, so the shared
  harness settings stay untouched.
- **Quiet**: trust pre-seeded, auto-update off, telemetry off.
- **Session**: `--session-id` where supported; otherwise capture after start.

Shared auth is simple because all agents share the same harness homes. The
sandbox bind-mounts `/auth/*` read-write at the paths the harness expects.
Concurrent use of one home by many instances is the normal multi-terminal case
for all three harnesses.

Some prompt channels are files inside the shared home, not flags. Copilot reads
`~/.copilot/SYSTEM.md`. The sandbox solves this with a file-level bind mount:
after mounting the shared home, it mounts the agent's rendered `prompt.md` over
`$HOME/.copilot/SYSTEM.md`. Each agent sees its own prompt at the shared path.
The same trick covers any other per-agent file a harness insists on reading
from its home, such as a hook config.

Verified in Phase 0 (Claude Code 2.1.310, `docs/compat.md`):

- Trust is not the only start-up dialog. A quiet start needs three things:
  `hasCompletedOnboarding` and `projects[<workspace>].hasTrustDialogAccepted`
  in `.claude.json`, and `skipDangerousModePermissionPrompt` in
  `--settings`. The trust and bypass dialogs default to "No, exit".
- Set `CLAUDE_CONFIG_DIR` always. Without it, `.claude.json` goes to
  `$HOME`, outside the shared home.
- Each agent gets its own `.claude.json`, bound over the shared one.
  Concurrent writes to one file from many instances are not tested.
- `--setting-sources ""` keeps the user's and the repository's settings
  and hooks out. It also keeps the shared home's `CLAUDE.md` out.
- `--bare` is not an option. It drops OAuth and all hooks.
- Claude Code freezes the system prompt at the first request. Always pass
  `--system-prompt-snapshot off`, so a changed template applies on resume.
- `--system-prompt` cannot remove the first line, "You are Claude Code,
  Anthropic's official CLI for Claude." The adapter appends by default.
- `--session-id` on a used UUID fails with "already in use". A restart
  uses `--resume <id>`.

---

## 9. The `terrarium` CLI for agents

The same binary as the operator CLI. Inside a sandbox, `TERRARIUM_AGENT` and
`TERRARIUM_SOCKET` are set, and the socket is the agent's own runner socket.
The runner authenticates the caller by the socket it arrived on. The daemon
enforces the agent's policy on every call.

```
terrarium whoami                             agent, project, harness
terrarium project show                       the agent's project: peers, repos, targets
terrarium agents ls [--all-projects]         peers in the project; --all-projects if policy allows
terrarium repo ls                            repositories attached to the project
terrarium repo checkout <repo> [--target T] [--branch b] [--path p]
                                             clone or update a repo in the workspace, or on a target
terrarium board                              the project board, grouped by column
terrarium card ls [--column c] [--mine] [--unassigned]
terrarium card show <id>                     body, comments, links, dependencies
terrarium card new <title> [--body - ] [--column c] [--label l] [--depends-on id]
terrarium card claim <id>                    assign to self and move to in_progress
terrarium card move <id> <column>
terrarium card assign <id> <agent>
terrarium card comment <id> <text | ->
terrarium card link <id> --thread T | --branch b
terrarium card wait [--mine] [--timeout 300]   block until a card is assigned to me or moves
terrarium mem search <query> [--tag t] [--limit n]      full-text search, newest first on ties
terrarium mem show <id> [--depth 1]          body, sources, and linked memories to depth
terrarium mem new <title> [--body -] [--tag t]... [--source card:TER-42|thread:T|path:p]
                                             [--link <id>:<relation>]...
terrarium mem link <a> <b> <relation> [--note ...]
terrarium mem unlink <a> <b> <relation>      only your own links
terrarium mem edit <id> [--body -] [--tag t]  only your own memories
terrarium mem supersede <old> <new>          marks old as superseded; search hides it by default
terrarium mem neighbors <id> [--relation r]
terrarium mem ls [--tag t] [--mine] [--recent 20]
terrarium wiki ls [path]                     page tree
terrarium wiki show <page> [--rev n]         Markdown body
terrarium wiki search <query>
terrarium wiki new <page> --body - [-m summary]            prints the preview URL
terrarium wiki edit <page> --body - --base <rev> [-m summary]   full replace, refused if base is stale
terrarium wiki section <page> <heading> --body - [-m summary]   replace one section by heading
terrarium wiki append <page> --body - [-m summary]
terrarium wiki history <page> | diff <page> <a> <b>
terrarium wiki mv <old> <new> | rm <page>    admins; history kept
terrarium artifact push <file|dir> [--name n] [--page p | --version v | --card c] [--note ...]
                                             uploads; a dir becomes a static bundle; prints the preview URL
terrarium artifact ls [--page p | --version v | --card c]
terrarium artifact url <id>                  stable preview URL to paste into mail or a page
terrarium artifact rm <id>                   own and unattached only
terrarium version new <name> [--notes -] [--repo r --ref sha] [--card id]...
terrarium version ls | show <name>
terrarium version attach <name> <artifact-id>
terrarium version submit <name>              draft -> in_review; mails the operator with the link
terrarium version wait <name> [--timeout 3600]   block until approved or rejected
terrarium version release <name>             admins; approved -> released
terrarium mail check                         counts, unread first
terrarium mail read [--thread T] [--mark]
terrarium mail send <agent> [-s subject] [-t thread] <body | ->
terrarium mail wait [--timeout 300]          block until mail arrives
terrarium status set <idle|working|blocked> [--note ...]   optional self-report
terrarium targets ls                         targets this agent may use
terrarium exec <target> [--cwd d] -- <cmd>   run a command, stream output
terrarium shell <target>                     interactive shell (we have a PTY)
terrarium cp <src> <target>:<dst>            copy files either way
terrarium hook <event>                       used by harness hooks, reads JSON on stdin
```

Policy per agent (in its definition):

```toml
project = "terrarium"
harness = "pi"
subscription = "anthropic-max"        # or "openai-codex", "copilot-pro", ...
model = "anthropic/claude-opus-5-5"   # optional; pi and codex take a model flag

[policy]
targets = ["buildbox", "lab-win11"]   # project targets and global targets by name
mail_to = ["*"]                       # "*" means every agent in the same project
# cross_project = ["ops/triage"]      # explicit project/agent names outside the project
```

A project definition:

```toml
# projects/terrarium.toml
name = "terrarium"
description = "The Terrarium tool itself"

[[repos]]
name = "terrarium"
url = "git@github.com:wiltaylor/terrarium.git"
default_branch = "main"
credential = "github-deploy"          # key in /state/credentials, never given to agents

[[repos]]
name = "copilot-prompt"
url = "https://github.com/wiltaylor/CopilotCustomSystemPrompt.git"

[checkout]
on_agent_create = ["terrarium"]       # cloned into $HOME/work/terrarium when the agent is made
```

A local repo (a URL under `TERRARIUM_REPOS_DIR`, default `/repos`, for
example `file:///repos/app.git`) is bound read-write into the sandbox of
each agent of its project at the same path, so a push needs no credential.

Repository checkout on a target runs on the host agent, not in the sandbox.
The daemon hands the credential to the host agent for that one clone or fetch.
The agent never sees the token. In the agent's own workspace the daemon does
the clone before the sandbox starts, for the same reason. After that, the
agent uses plain `git` inside its workspace; pushes go through a credential
helper that calls `terrarium repo credential`, which the daemon answers only
for repos in the agent's project.

---

## 10. Roles and leader agents

Every agent is a `member` of its project. Two roles add power. The daemon
checks the role on every CLI and API call, and writes every privileged action
to the audit log with the agent's ID.

| Capability | member | project_admin | sysadmin |
|---|---|---|---|
| Read own project: peers, board, memories, repos | yes | yes | all projects |
| Mail agents in own project | yes | yes | any agent |
| Read the project inbox, reply from it | no | yes | any project |
| Create, start, stop, restart, delete agents | no | in project | anywhere |
| Set an agent's system prompt, harness, policy (project targets and `mail_to` in the project; a global target or a `cross_project` entry needs sysadmin) | no | in project | anywhere |
| Assign, reassign, reopen any card; edit columns and WIP limits | no | in project | anywhere |
| Edit or delete any memory | no | in project | anywhere |
| Attach repos, define project targets | no | in project | anywhere |
| Lease a target to an agent at run time (§19; a global target needs sysadmin) | no | in project | anywhere |
| Create and delete projects | no | no | yes |
| Define global targets | no | no | yes |
| List host agents | yes | yes | yes |
| Register and revoke host agents | no | no | no (operator only) |
| Grant `project_admin` | no | in project | anywhere |
| Grant `sysadmin` | no | no | no (operator only) |
| Manage credentials and host agent tokens | no | no | no (operator only) |

Limits that always hold:

- An agent cannot change its own roles or delete itself.
- Deleting a project, or an agent with a live session, needs `--confirm` and
  is logged. The dashboard shows a banner for the next hour.
- The sandbox does not change with the role. A sysadmin agent still sees only
  its own workspace. Power flows through the CLI and the daemon, never through
  the filesystem.

In agent mode the CLI exposes the operator commands that the role permits,
under `terrarium admin ...`, with the same shape as the operator CLI:

```
terrarium admin agent new <name> --harness claude --project P --prompt leader-worker [--role project_admin]
terrarium admin agent start|stop|restart|rm <name> [--confirm]
terrarium admin agent set <name> --prompt ... | --policy ... 
terrarium admin project new <name> | rm <name> --confirm        sysadmin
terrarium admin role grant|revoke <agent> <role>
terrarium admin target new ... | host ls                        sysadmin for global
terrarium admin target grant|revoke <target> <agent> [--until 30m] [--exclusive]
terrarium admin audit [--since ...]
```

Leader agents are ordinary agents with a role and a leader prompt template.
Two templates ship:

- **Project lead** (`project_admin`): triage the project inbox into cards,
  keep `ready` stocked, create and retire worker agents as the board demands,
  watch for workers idle on `in_progress` cards or stuck in `blocked`, reply to
  other projects when their requests are done.
- **System lead** (`sysadmin`): create projects from operator requests, appoint
  project leads, watch host agents and targets, restart agents in `error`,
  report a daily summary to the operator by mail.

Definition:

```toml
# agents/terrarium-lead.toml
name = "lead"
project = "terrarium"
harness = "claude"
prompt = "project-lead"
roles = ["project_admin"]
```

---

## 11. Sandbox

Default: **bubblewrap** inside the container. Fast to start, no daemon, a PTY
passes straight through. Visible to the harness:

| Path | Mode | Why |
|---|---|---|
| `/usr`, `/lib`, `/bin`, `/etc` (subset), Node, harness binaries | ro | the harness must run |
| `/work/<project>/<agent>` (`TERRARIUM_WORK_DIR`) as `$HOME/work`, and again at `/work/<project>/<agent>` | rw | the agent's files; the second path is where a target sees them ("mode 1", below) |
| `/state/agents/<ulid>/harness` at the same path | ro | `prompt.md` and the adapter's per-agent files. The harness cannot change them. |
| `/auth/claude` as `$HOME/.claude`, `/auth/pi` as `$HOME/.pi`, etc. | rw | shared login, settings, and caches |
| per-agent overlays from `harness/` (`FileOverlay`) | rw or ro | the files that the harness writes itself are rw (Claude's `.claude.json`, `sessions/`, `history.jsonl`, `backups/`, `projects/<cwd>/` with the transcripts and memory, `file-history/`, `todos/`, `session-env/`; Codex's `history.jsonl`, `sessions/`, `session_index.jsonl`, `shell_snapshots/`, and its SQLite home (`-c sqlite_home`); Copilot's settings, history, state files, `session-state/`, `session-store.db` with its `-wal` and `-shm`, `logs/`, cache; pi's `settings.json` and `sessions/`); the rest are ro (Copilot's `SYSTEM.md`, hook dirs, Codex's `hooks.json`, pi's extension). The session files of each agent are its own, so no agent sees the transcripts of another agent, in its project or in another project |
| `/run/terrarium/agents/<ulid>.sock` | rw | hooks and the agent CLI |
| `/usr/local/bin/terrarium` | ro | the CLI |
| `/tmp`, `/proc`, `/dev/pts` | private | normal process needs |
| `/state/agents/<ulid>/private` | not bound | the runner's records; see below |

The agent directory `/state/agents/<ulid>/` has three parts
(`docs/runner.md`, "Files"):

| Directory | Holds | Writers | In the sandbox |
|---|---|---|---|
| `private/` (mode 0700) | `events.jsonl`, `pty.log`, `runner.json`, `runner-argv.json`, `runner-exit.json`, `runner.log`, `session-prompt.sha256` | the daemon and the runner | not bound |
| `harness/` | `prompt.md`, the adapter's per-agent dirs (`claude/`, `codex/`, `copilot/`, `pi/`) | the daemon (adapter) at launch; the harness only through the writable overlays | ro at its own path, with rw binds for the writable overlays |
| `workspace/` | the layout before the work root: the next start moves it to `/work/<project>/<agent>` | | not bound |

Hidden: other agents' workspaces, `/state/terrarium.db`, the daemon socket, the
Docker socket, SSH keys, the host filesystem, and the agent's own
`/state/agents/<ulid>/private/`. That directory holds the runner's records: the
event log (usage and audit), the PTY log, the launch spec with the injected
keys, and the exit record (resume). A yolo agent can neither read nor change
them (`docs/security-review.md`, F1). A daemon start moves an agent dir of the
older flat layout into `private/` and `harness/` once. Network stays on; the harness
must reach its model API.

Requirement: `bwrap` needs user namespaces. The container must run with a
seccomp profile that allows `unshare`/`clone(CLONE_NEWUSER)`, for example
`--security-opt seccomp=unconfined` or a custom profile. Phase 0 verifies this.

**Mode 1: the workspace on the host.** The workspace of an agent is
`<TERRARIUM_WORK_DIR>/<project>/<agent>` (`/work` in the container, the
`./work` bind mount), not a dir in the ULID-keyed agent dir. The sandbox has
no compilers: the harness edits files in the sandbox, and builds and tests
run on a target. A host agent attaches the workspace into each docker or
vmlab target that the agent holds, at the same `/work/<project>/<agent>`,
so paths in error messages match (docs/api.md §17.8, docs/host-protocol.md
"Workspaces").

Fallback: one **sibling container** per agent, started through the mounted
Docker socket, with the same volumes. Slower to start, but needs no namespace
privileges inside the Terrarium container.

Yolo is safe to the extent the sandbox holds. The agent can destroy its own
workspace and its own harness session files. It cannot touch the host except
through `terrarium exec`, which the policy limits.

Verified in Phase 0 (`docs/compat.md`, Docker 29.8.0, bwrap 0.8.0):

- Allowing `unshare` and `clone` is not enough. The seccomp profile also
  needs `clone3`, `mount`, `umount2`, and `pivot_root`, and the container
  needs `--security-opt systempaths=unconfined` for `--proc`. `SYS_ADMIN`
  does not help. Hosts with AppArmor can need `apparmor=unconfined`; that
  is not tested.
- The per-file overlay works. Each sandbox sees its own file, and the
  shared file does not change. If the target file does not exist, bwrap
  leaves an empty file in the shared home. The adapter creates the target
  before the first start.
- Claude Code writes to `.claude.json`, so its overlay is read-write. Not
  tested: a rename-over write onto a file that is a bind mount.
- Claude Code refuses yolo as root. `IS_SANDBOX=1` removes the refusal.
  The adapter sets it only when the harness runs as root.

Verified with Claude Code outside Docker (`docs/compat.md`, "Claude inside
bwrap"; the code is `crates/terrariumd/src/sandbox.rs`):

- The table above holds, with `/etc` limited to name lookup, TLS roots,
  `passwd`/`group`, `localtime`, `alternatives`, and the linker cache. A
  merged-`/usr` host gets `/lib`, `/lib64`, `/bin`, `/sbin` as symlinks.
  `$HOME` is a tmpfs; the workspace is `$HOME/work`; the `harness/` dir is
  bound read-only at its own path, with read-write binds for the writable
  overlays only; `private/` is not bound; the socket is `/run/terrarium/agent.sock`; the CLI is
  `/opt/terrarium/bin/terrarium`. A harness binary outside `/usr` is bound
  read-only at its real path.
- The child env starts empty: `PATH`, `HOME`, `TERM`, `LANG`, the
  config-dir var, `TERRARIUM_SOCKET`, `TERRARIUM_AGENT`, and the adapter's
  env. Nothing from the daemon env leaks.
- `--new-session` is off by default: with it, PTY resizes do not reach the
  harness. Each sandbox has its own PTY, so `TIOCSTI` can only type into
  the agent's own input.
- Claude Code also needs a per-agent `$CLAUDE_CONFIG_DIR/sessions/`
  directory over the shared one: its `sessions/<pid>.json` collides in the
  PID namespace (every agent is pid 2).

---

## 12. Mailbox

Table: `messages(id, thread_id, from_agent, to_agent, subject, body, state,
created_at, delivered_at, read_at)`.

Two delivery paths. Both are needed. The board (§13) reuses the push path.

1. **Pull.** The agent runs `terrarium mail check` and `terrarium mail read`.
   The system prompt tells it to check at the start of a turn and before it
   declares a task done. `terrarium mail wait` lets an agent block cheaply.
2. **Push.** When a message arrives and the recipient is `idle`, the runner
   types a turn into the PTY: "New mail from <agent> (thread T). Run
   `terrarium mail read --thread T`." If the recipient is `working`, the
   message waits for the stop hook, then push runs. Loop guard: when a
   thread has more than `TERRARIUM_MAIL_LOOP_LIMIT` (default 6)
   agent-to-agent messages with no operator or project-inbox message
   between them, the daemon types no more pointers for it, marks it
   `paused`, and mails the operator once on the thread ("thread T needs a
   human: N agent replies without operator input"); an operator message on
   the thread resets the count.

Safety: mail from another agent is untrusted input. The pushed turn carries
only the sender, thread, and the command to read it. The body is read through
the CLI with clear delimiters. The system prompt says mail is data, not
operator instructions.

Scope: `mail_to = ["*"]` means the agent's own project. Mail to another
project's agent needs an explicit entry in `cross_project`. The daemon rejects
anything else and returns a clear error to the CLI, so the agent learns the
boundary.

**Project inboxes.** Each project has an address, `project:<name>`. Any agent
may write to any project inbox; that is the sanctioned cross-project channel.
Project admins read it. The intended flow:

1. A worker in project A hits a bug in project B's code. It runs
   `terrarium mail send project:B -s "bug: ..." -` with the details, and
   links the thread to its own card with `terrarium card link`.
2. Project B's lead reads the inbox and runs `terrarium card new --from-mail
   <id>`. The new card carries the thread ID. The lead replies on the thread so
   project A knows it is accepted.
3. When that card moves to `done`, the daemon posts a notification on the
   thread automatically: card ID, title, and the closing comment. Project A's
   original sender, and anyone else on the thread, gets it in their inbox, and
   the linked card in A shows the update.

Threads span projects. A thread's participants are the agents and project
inboxes that have posted on it. Replies go to all participants. The dashboard
shows a project's inbox as a view next to its board, with a one-click
"make card" action.

Project inbox commands, for project admins:

```
terrarium inbox ls [--unread]
terrarium inbox read <id> [--mark]
terrarium inbox reply <id> <text | ->
terrarium card new --from-mail <id> [...]
```

Humans use the same mailbox. `terrarium mail send <agent> ...` from the
operator shell or the dashboard sends as `operator`.

The push path also carries target lease changes (§19): "You now have access
to target <t> (<state>) until <time or 'revoked'>. Run `terrarium targets
ls`." after the lifecycle step of the grant, and "Your access to target <t>
ended." An extend types nothing. A pointer for a stopped agent waits for its
next start, as a card pointer does, also across a daemon restart.

---

## 13. Kanban board

One board per project. The operator and every agent in the project share it.

Data: `boards(project)`, `columns(board, name, position, wip_limit)`,
`cards(id, board, column, position, title, body, assignee, labels, repo,
branch, created_by, created_at, updated_at)`, `card_deps(card, depends_on)`,
`card_comments(card, author, body, created_at)`, `card_links(card, kind, ref)`.
Card IDs are short and project-local, for example `TER-42`, so agents can type
them.

Rules the daemon enforces:

- Every change is an event with an author. The board is an audit trail of who
  did what.
- An agent may create cards, claim unassigned cards, move and comment on its
  own cards, and comment on any card. It may not move or reassign another
  agent's card. The operator may do anything.
- A card with unmet dependencies cannot enter `in_progress`. The CLI explains
  which cards block it.
- A column with a WIP limit refuses a move past the limit with a clear error.
- Moving a card to `blocked` requires a comment. The daemon mails the operator
  (and the card's creator, if different) with the card ID and the comment.
- `done` is terminal for agents. Only a project admin or the operator reopens
  a card. On `done`, the daemon notifies every mail thread linked to the card
  (§12).

The board protocol in the system prompt tells an agent to:

1. On start and whenever idle, run `terrarium card ls --mine`, then
   `--unassigned --column ready`.
2. Claim one card at a time. Work it. Comment on progress at natural
   checkpoints, with the branch name once there is one.
3. Move it to `review` with a comment that says what to look at, or to
   `blocked` with a comment that says what is needed.
4. Split work it cannot finish into new cards in `backlog`, linked with
   `--depends-on`, rather than leaving notes in chat.

Push: when a card is assigned to an agent, or one of its blocked cards becomes
unblocked, the runner uses the same push path as mail (§12). If the agent is
`idle` it types a pointer turn: "Card TER-42 was assigned to you. Run
`terrarium card show TER-42`." If it is `working`, the pointer waits for the
stop hook. If the agent does not run, the pointer waits for its next start,
also across a daemon restart (`pending_pointers`), and the daemon checks
the card again before it types the line. `terrarium card wait` is the pull
equivalent for an agent that prefers to block.

Lead nudges use the same path. A card with no assignee in `ready`, or a
move to `review` or `blocked`, types a pointer into each `project_admin` of
the project, except the agent that made the change. Each project gets at
most one batch of nudges in 10 s; the changes in that time go out as one
pointer.

Status link: the dashboard shows each agent's current `in_progress` card next
to its status badge, so "working" always answers "on what".

---

## 14. Memory graph

One graph per project. Every agent in the project and the operator share it.
It is the place for knowledge that outlives one session: decisions, gotchas,
how a subsystem works, where things live, what was tried and failed.

Data: `memories(id, project, title, body, tags, author, created_at,
updated_at, superseded_by)`, `memory_sources(memory, kind, ref)`,
`memory_links(from_id, to_id, relation, author, note, created_at)`, plus an
FTS5 index over title, body, and tags. IDs are short and project-local, for
example `MEM-17`.

Rules the daemon enforces:

- Everyone in the project reads everything. Search and traversal are open.
- An agent edits only the memories it wrote. Anyone may add links and tags to
  any memory. Links are attributed.
- Agents never delete. They `supersede`. A superseded memory stays in the
  graph, hidden from search by default, visible with `--all` and in `show`
  through its `supersedes` edge. The operator may delete.
- A memory may cite sources: a card, a mail thread, a repo path or commit.
  The dashboard renders these as links.
- A link needs a relation from the fixed vocabulary (§5). `contradicts` and
  `supersedes` are surfaced first in `show`, so an agent sees disagreement.

The memory protocol in the system prompt tells an agent to:

1. Before starting a card, run `terrarium mem search` with the card's key
   terms, and `mem show --depth 1` on the best hits.
2. When it learns something non-obvious, write one memory per fact. Title it as
   a claim. Keep the body short. Cite the source. Link it to the memories it
   read in step 1 with the right relation.
3. When it finds an existing memory is wrong or stale, write the correction and
   `supersede` the old one, or link with `contradicts` if unsure.
4. Prefer linking to an existing memory over writing a near-duplicate. The
   `new` command prints the top search hits for the title before it writes, so
   the agent sees duplicates.

Retrieval is full-text plus graph traversal in v1. Embedding search is an open
item (§22); the schema leaves room for a vector column.

Dashboard: a graph view per project (force layout), click a node for the body,
sources, and links; search box; filter by tag, author, or relation; a timeline
of recent writes. The card view shows memories that cite that card.

---

## 15. Wiki, artifacts, and versions

These three exist so an agent can hand something to the user and get an answer
back, without leaving the dashboard. The memory graph (§14) is for agents. The
wiki is for the user.

### 15.1 Wiki

One wiki per project. Agents publish Markdown pages for the user to read in
the browser: reports, proposals, design write-ups, test summaries, screenshots
with commentary. Every save is a new revision.

Data: `wiki_pages(id, project, path, title, current_rev, created_at,
deleted_at)`, `wiki_revisions(page, rev, author, summary, body, created_at)`,
plus an FTS5 index over the current revision of each page.

Conventions:

- Paths are slugs with `/` for hierarchy: `index`, `reports/perf-week-3`,
  `designs/runner-v2`. Every project starts with an `index` page that the lead
  keeps as a table of contents.
- Wikilinks: `[[designs/runner-v2]]` a page, `[[TER-42]]` a card, `[[MEM-17]]`
  a memory, `[[v0.3.0]]` a version, `[[project:other/reports/x]]` a page in
  another project. Backlinks are indexed.
- Artifacts embed by ID: `![diagram](artifact:ART-9)` renders an image,
  `[report](artifact:ART-10)` links a file, and a fenced block
  ` ```artifact ART-11``` ` embeds a static bundle or an HTML file in a
  sandboxed frame. So a page can show a built dashboard prototype, a coverage
  report, or a generated chart inline.
- Front matter may carry `status` (`draft`, `ready_for_review`, `reviewed`)
  and `reviewer`. The dashboard lists `ready_for_review` pages on the home
  screen and the page shows a review bar with approve and comment actions.

Rules the daemon enforces:

- Every member may create pages and edit any page.
- `edit` requires `--base <rev>`. A stale base is refused and the CLI prints
  the diff since the base. `section` and `append` rarely conflict, so the
  protocol prefers them.
- Every revision has an author and a non-empty summary.
- Only admins move or delete pages. Deleted pages keep their history.

### 15.2 Artifacts

An artifact is a file, or a directory uploaded as a static bundle, that the
daemon stores by content hash and serves at a stable URL. Agents push
artifacts from their workspace through the CLI; the runner streams the bytes
to the daemon. Typical uses: screenshots, PDFs, generated reports, built
static sites, test output, binaries for a release.

Data: `artifacts(id, project, name, content_type, size, sha256, kind
(file|bundle), created_by, created_at)`, `artifact_links(artifact, kind
(page|version|card), ref)`. Blobs live under `/state/artifacts/<sha256>`;
bundles are unpacked there and served from their `index.html`.

Serving:

- `GET /a/<id>/<name>` for a file; `GET /a/<id>/` and below for a bundle.
- Artifacts are served from a **separate origin** from the dashboard, with
  short-lived signed URLs the dashboard mints. An uploaded HTML page or bundle
  therefore runs with no access to the dashboard's session or API. The
  dashboard embeds them in `<iframe sandbox>` without `allow-same-origin`.
- Content types are sniffed and fixed on upload. Executables download, never
  render.
- Limits: per-file size, per-project quota, both configurable. The dashboard
  shows usage.

Retention: artifacts attached to a `released` version, or embedded in a page
that is not deleted, are kept. Unattached artifacts older than a configurable
age are pruned, with a warning in the audit log a day before.

### 15.3 Versions

A version is a named point in a project's life that the user can review:
`v0.3.0`, `demo-2026-10-31`, `perf-fix-candidate`. It gathers notes, the
cards that landed, a repo ref, and the artifacts to look at.

Data: `versions(id, project, name, status, notes, repo, git_ref, created_by,
created_at, submitted_at, decided_at, decided_by)`, `version_cards(version,
card)`, `version_reviews(version, author, decision, comment, created_at)`.
Statuses: `draft`, `in_review`, `approved`, `rejected`, `released`.

Flow:

1. An agent (often the lead) creates a version, attaches the build and the
   evidence (test report, screenshots, a wiki page with the write-up), links
   the cards, and runs `terrarium version submit`. The daemon mails the
   operator with the dashboard link and sets `in_review`.
2. The user opens the Versions view: notes, cards that landed since the last
   version, artifacts with inline preview, the diff of linked wiki pages since
   the previous version. The user approves or rejects with a comment.
3. The daemon mails the submitter and comments on every linked card with the
   decision. An agent blocked on `terrarium version wait` wakes up. On
   `rejected`, the lead opens follow-up cards from the comment.
4. An admin runs `terrarium version release` once the approved build is
   shipped. Released versions are immutable; artifacts on them are retained.

Dashboard: a Versions timeline per project; a version page with status,
notes, cards, artifacts, reviews, and compare-to-previous; approve and reject
buttons with a comment box; a home-screen queue of everything `in_review`
across projects.

The protocol in the system prompt tells an agent to:

1. Put anything meant for the user's eyes in the wiki or on a version. Never
   start a file server or ask the user to open a path in the workspace.
2. Push evidence as artifacts and embed it. A page that claims a result shows
   the result.
3. Use `ready_for_review` on a page, or `submit` on a version, to ask for a
   decision. Then `version wait` or move on to another card; do not poll.
4. Keep `index` current when adding pages.

Export: mirror a project's wiki and the artifacts it embeds to a `docs/` tree
in a repo (§22). Page paths map to file paths.

---

## 16. Subscriptions and usage

Terrarium is logged in to several plans at once. The operator needs to see,
on the home screen, how much of each is left and who is spending it.

A subscription definition:

```toml
# subscriptions/anthropic-max.toml
name = "anthropic-max"
provider = "anthropic"
plan = "Max 20x"
harnesses = ["claude"]                 # which harnesses can spend it (pi cannot spend an Anthropic plan)
probe = "anthropic-oauth-usage"        # how to read remaining quota
alert_at = 80                          # percent of any window
```

### 16.1 Registering subscriptions from the dashboard

The **Subscriptions** page lists every plan with a status chip: `logged in`,
`expiring`, `expired`, `missing`, with the last time the daemon verified it.
On startup, and on demand, the daemon runs the harness status commands
(`claude auth status`, `codex login status`, `pi auth check --json` per
provider, and Copilot's stored-token check) and offers to create a subscription
definition for each login it finds that has none.

**Add subscription** is a wizard with one path per kind:

| Kind | What the wizard does |
|---|---|
| Claude Code (Anthropic plan) | Opens an embedded terminal running `claude auth login` as the daemon user, outside any sandbox. Extracts the login URL and shows it as a link with a copy button. Gives a paste field for the code and types it into the PTY. Offers `claude setup-token` as the long-lived alternative. |
| Codex (ChatGPT plan) | Runs `codex login --device-auth` in the embedded terminal. Shows the device code and URL large, with copy buttons. Polls `codex login status` until it succeeds. |
| Copilot (GitHub plan) | Runs `copilot login --device-code`. Same device-code pattern. A second tab takes a fine-grained PAT with the Copilot Requests permission and feeds it through `--with-token` on stdin. |
| pi, OAuth provider | Runs `pi` in the embedded terminal with instructions to use `/login` for the chosen provider. The idea to reuse the Claude Code token for Anthropic is dead: Anthropic bills a Pro/Max token used through pi as extra usage, not from the plan (`docs/compat.md`, "pi: facts that contradict the plan"). For Anthropic through pi, use an API-key subscription (next row). A copy of the Codex access token works for `openai-codex`, but only with no refresh token, so it expires. |
| pi, API key | A form: provider (Anthropic, OpenAI, Google, OpenRouter, xAI, Groq, or an OpenAI-compatible endpoint with a base URL and model list). The key is stored under `/state/credentials` with mode 0600, never shown again, and injected as the provider's env var only into agents on this subscription. Optionally written to pi's `auth.json` for `pi auth` compatibility. |

After the login succeeds the wizard:

1. Verifies with the harness status command and `pi auth check`.
2. Fills the subscription definition: name, provider, plan (from the status
   output when it says), which harnesses can spend it, the matching probe.
3. Runs the probe once and shows the first quota numbers.
4. Writes an audit entry and, if the plan has no probe, marks the tile as
   local estimate only.

Each row has **Re-login**, **Logout**, and **Verify now**. Expiring OAuth
tokens that refresh on their own show as `logged in`; ones the harness could
not refresh flip to `expired` and mail the operator.

Rules: the embedded login terminal is available to the operator only, never to
agents, and is torn down on completion or after ten minutes. The daemon never
reads a pasted code or key back to the browser. Device-code and paste-code
flows are used everywhere, because a browser loopback callback cannot reach
into the container.

### 16.2 Measuring usage

Two sources of truth, combined:

1. **Provider probes.** Where a provider exposes usage for a subscription,
   the daemon polls it on a schedule and stores samples. Known or likely
   endpoints, verified in Phase 2f (see compat.md "Usage probes"):
   - Anthropic: the OAuth usage endpoint behind Claude Code's `/usage`, with
     5-hour and 7-day window utilisation and reset times.
   - OpenAI Codex: rate-limit windows that Codex reports in its session
     events and `/status`; primary and secondary windows with reset times.
   - GitHub Copilot: the premium-request quota snapshot behind `copilot`'s
     usage display.
   - Google: the quota the Gemini CLI shows in `/stats`.
   The probe uses the credential in the shared harness home, or
   `pi auth print-bearer-token --provider <p>` for providers pi is logged in
   to, so there is no second login.
2. **Local accounting.** Every harness writes session files with per-message
   token usage and model. The runner tails the current session file for its
   agent and records `usage_samples(agent, project, subscription, model,
   input_tokens, output_tokens, cache_tokens, turn, at)`. This gives the
   breakdown the providers do not: per agent, per project, per card, per hour.
   Where a provider has no probe, local accounting is the only number and the
   dashboard says so.

Data: `subscriptions(name, provider, plan, ...)`, `quota_samples(subscription,
window, used_pct, resets_at, raw, at)`, `usage_samples(...)` as above.

Policy hooks:

- An agent definition names its subscription. For `pi` and `codex` the
  adapter also passes the model. The daemon refuses a start if the harness
  cannot spend that subscription.
- When a window passes `alert_at`, the daemon mails the operator and raises a
  dashboard banner. At 100 percent it pauses new turns on agents on that
  subscription, marks them `blocked` with the reset time, and resumes them
  after the reset. A sysadmin agent may instead move an agent to another
  subscription with `terrarium admin agent set --subscription`, which takes
  effect at the next restart; pi makes this a flag change.
- Per-project budgets are optional: a cap on tokens or turns per day on a
  given subscription. The lead template reads the numbers through
  `terrarium usage` and plans around resets.

Agent CLI:

```
terrarium usage                              my subscription: windows, resets, my share today
terrarium usage project                      per-agent breakdown for my project
terrarium usage subscriptions                all plans, remaining per window (admins)
```

Dashboard home, **Usage** strip at the top:

- One tile per subscription: provider logo, plan name, one bar per window
  (5h, 7d, monthly, whatever the plan has) with percent used and time to
  reset, and a sparkline of the last 24 hours. Grey tile with "local estimate
  only" when no probe exists.
- Click a tile: consumption over time, split by project, then by agent, then
  by model; the top cards by spend; the probe's raw numbers and last poll time.
- A **Spend** view under Projects shows the same split for one project.

---

## 17. API and dashboard

HTTP + WebSocket on `/run/terrarium/api.sock`, and on a TCP port with a bearer
token for the dashboard.

```
GET    /api/projects ; POST /api/projects ; GET|PATCH|DELETE /api/projects/{name}
GET    /api/projects/{name}/agents|targets|repos
POST   /api/projects/{name}/repos        attach a repository
POST   /api/repos/{id}/checkout          {target?, branch?, path?}

GET    /api/projects/{name}/board        columns with cards, positions, assignees
PATCH  /api/projects/{name}/board        rename or reorder columns, set WIP limits
POST   /api/projects/{name}/cards        create
GET|PATCH|DELETE /api/cards/{id}         read, edit, move, assign, archive
POST   /api/cards/{id}/comments
GET    /api/cards/{id}/events            history of this card

GET    /api/projects/{name}/memories?q=&tag=&author=&all=
POST   /api/projects/{name}/memories
GET|PATCH|DELETE /api/memories/{id}
GET    /api/memories/{id}/graph?depth=   node plus neighbours to depth
POST|DELETE /api/memories/{id}/links     {to, relation, note}
POST   /api/memories/{id}/supersede      {by}

GET    /api/projects/{name}/wiki?q=       page tree or search
POST   /api/projects/{name}/wiki          create page
GET|PUT|DELETE /api/wiki/{id}            read current, save new revision (body carries base rev), delete
POST   /api/wiki/{id}/section            {heading, body, base, summary}
GET    /api/wiki/{id}/revisions ; GET /api/wiki/{id}/revisions/{rev} ; GET /api/wiki/{id}/diff?a=&b=
POST   /api/wiki/{id}/restore            {rev}
GET    /api/wiki/{id}/backlinks
POST   /api/wiki/{id}/review             {decision, comment}

POST   /api/projects/{name}/artifacts    multipart upload; dir as tar for a bundle
GET    /api/artifacts/{id} ; DELETE /api/artifacts/{id}
POST   /api/artifacts/{id}/links         {kind, ref}
GET    /a/{id}/{path}                    serving origin, signed URL, separate from the dashboard

GET    /api/projects/{name}/versions ; POST /api/projects/{name}/versions
GET|PATCH /api/versions/{id}
POST   /api/versions/{id}/submit|approve|reject|release   {comment?}
GET    /api/versions/{id}/compare?to=    cards, pages, artifacts changed since another version

GET    /api/subscriptions                 plans with latest quota samples
GET    /api/subscriptions/{name}/quota?since=
GET    /api/usage?subscription=&project=&agent=&model=&since=&bucket=hour
POST   /api/subscriptions/{name}/probe    poll now
GET    /api/subscriptions/detect          logins found in the harness homes with no definition
POST   /api/subscriptions/login           {kind, provider?} -> login session id
WS     /api/subscriptions/login/{id}/pty  embedded terminal for the login flow, operator only
POST   /api/subscriptions/login/{id}/input   {code} typed into the flow
GET    /api/subscriptions/login/{id}      state: url, device code, done, error
POST   /api/subscriptions/{name}/api-key  {provider, key, base_url?, models?}, never readable back
POST   /api/subscriptions/{name}/logout|verify

GET    /api/agents?project=              list with status
POST   /api/agents                       create from a definition
GET    /api/agents/{id}
DELETE /api/agents/{id}
POST   /api/agents/{id}/start|stop|restart
POST   /api/agents/{id}/send             {text} -> typed as a turn
GET    /api/agents/{id}/events           WS or SSE, ?since=
WS     /api/agents/{id}/pty              bidirectional PTY, resize messages
GET    /api/agents/{id}/sessions         history of harness session ids

GET    /api/mail?to=&state=
POST   /api/mail                         {from, to, subject, body, thread_id}
POST   /api/mail/{id}/read|reply
GET    /api/projects/{name}/inbox        project inbox, admins and operator
POST   /api/projects/{name}/inbox        send to a project

GET    /api/agents/{id}/roles ; PUT /api/agents/{id}/roles
GET    /api/audit?since=&actor=

GET    /api/targets ; POST /api/targets ; POST /api/targets/{id}/up|down|snapshot|restore
GET    /api/events                       firehose for the dashboard
```

Dashboard (SolidJS, built with Vite, embedded in `terrariumd` with
`rust-embed`):

- **Projects** sidebar: pick a project; every other view filters to it.
- **Agents** grid: name, harness, status badge, current turn age, last event.
- **Repos** panel per project: attach, set branch, check out onto a target.
- **Board** view per project: columns, drag-and-drop cards, inline edit,
  assign to an agent, comment, filter by label or assignee. Cards show the
  assignee's live status badge. Live updates over the events WebSocket.
- **Inbox** view per project: threads from other projects, make-card action.
- **Roles** on the agent card: badge for `project_admin` and `sysadmin`, edit
  from the agent detail page. **Audit** view: privileged actions, filterable.
- **Wiki** view per project: page tree, rendered page with wikilinks,
  backlinks, and embedded artifacts, review bar, history with diffs, restore,
  inline editor.
- **Artifacts** viewer: images, PDFs, text and code with highlighting,
  Markdown, and HTML or static bundles in a sandboxed frame. Download for
  everything else.
- **Versions** view per project: timeline, version page with notes, cards,
  artifacts, reviews, compare-to-previous, approve and reject.
- **Subscriptions** page: status chips, add-subscription wizard with the
  embedded login terminal, API-key form, re-login, logout, verify.
- **Home** screen: the **Usage** strip (§16) across the top, then everything
  waiting on the user across projects: versions `in_review`, pages
  `ready_for_review`, cards in `blocked`, inbox items.
- **Memory** view per project: force-layout graph, node detail panel, search,
  filters by tag, author, and relation, recent-writes timeline.
- **Terminal** view: xterm.js attached to `/pty`. Read-only by default, a
  toggle for input. Resize follows the browser.
- **Mail** view: threads across agents, compose as operator.
- **Targets** view: state, up/down, snapshot.
- **Event log**: filterable firehose.

Rust crates: `tokio`, `axum`, `portable-pty`, `vt100`, `rusqlite`, `clap`,
`serde`, `rust-embed`, `tracing`.

---

## 18. Durability and resume

- Restart policy per agent: `always`, `on-failure`, `never`. Exponential
  backoff. A crash loop (N restarts in M minutes) sets `error` and stops.
- On restart the runner calls `launch(agent, Some(session_id))`. The harness
  resumes its conversation. If the resume fails, Terrarium starts a fresh
  session, records the old one in history, and types a short recap turn built
  from the last events.
- The daemon restarts without killing agents. The runner holds the PTY. On
  start the daemon reconnects to each runner socket and reconciles state.
- The container restarts with `restart: unless-stopped`. On container start the
  daemon re-launches every agent that was running, with resume.

---

## 19. Targets, host agents, and executors

The Terrarium container cannot see the Docker host, cannot run vmlab, and may
not be on the same machine as the labs. A **host agent** closes that gap.

**Host agent** (`terrarium-host`):

- One static Rust binary plus a systemd unit. Install it on any Linux host you
  want Terrarium to use. A Windows build can come later.
- It dials **out** to the daemon over WebSocket with a per-host token. Hosts
  need no open inbound port and work behind NAT. The daemon multiplexes many
  exec sessions over the one connection.
- The link uses TLS (`wss://`) when the daemon has a certificate
  (`TERRARIUM_HOST_TLS_CERT`, `TERRARIUM_HOST_TLS_KEY`; `just host::cert`
  makes a self-signed one). The host agent can pin the certificate with
  `server_fingerprint_sha256`. Without a certificate, the daemon's host
  listener is plaintext on `127.0.0.1:8082` only, so a remote host cannot
  reach it. A host agent refuses `ws://` to a host that is not loopback,
  unless its config has `allow_insecure = true`
  (`docs/host-protocol.md`, "TLS").
- Tokens: the daemon stores only a hash of each token. The operator can
  rotate a token (`terrarium admin host rotate`) or revoke it. An **enrol
  token** works one time, for 10 minutes: at its first connect, the host
  agent swaps it for a new long-lived token, which the daemon sends in
  `welcome`. So the long-lived token never goes through the operator's
  clipboard. `just host::install <user@host> <name>` does the full install
  over ssh with an enrol token and the certificate pin
  (`docs/host-protocol.md`, "Tokens" and "Install").
- It implements the executor protocol locally: `exec` (streamed, optional
  PTY), `cp`, and `shell`.
- It implements target lifecycle for the tools present on that host:
  - **Docker**: `run`, `exec`, `stop`, `rm`, volume handling.
  - **vmlab**: `up`, `down`, `destroy`, `exec`, `cp`, `shell`, `snapshot`,
    `status`, driven through the vmlab CLI.
- It reports capabilities on connect (has docker, has vmlab, OS, arch), so the
  dashboard and the policy know what each host can do.
- On reconnect it resumes without losing running targets. Running containers
  and VMs are state on the host, not in the connection.

Target kinds:

| Kind | Where it runs | Lifecycle | Notes |
|---|---|---|---|
| `host` | directly on a host agent's machine | permanent | commands run as the host agent's user |
| `ssh` | any machine with SSH | permanent | zero install; no lifecycle features |
| `docker` | a host agent with Docker | ephemeral or permanent | image or existing container |
| `vmlab` | a host agent with vmlab | ephemeral or permanent | lab file or template; `snapshot` gives reset-to-clean |

A target definition names its host agent:

```toml
# targets/lab-win11.toml
kind = "vmlab"
host = "lab1"                 # host agent name
lab  = "/home/wil/labs/win11"          # directory that holds vmlab.wcl; vmlab finds only that file name
machine = "win11"
ephemeral = true
snapshot_before_task = true
```

Ephemeral targets: the host agent creates them on agent start and destroys them
on agent stop unless `keep = true`. With a lease, the grant to a running
agent creates them, and the end of the last running holder destroys them.

Workspaces on targets ("mode 1", §11): the host agent binds each holder's
workspace into a docker target at `/work/<project>/<agent>` (`docker run
-v`), and syncs it into a vmlab machine with an `@dev` decorator in the lab
file of the instance. A vmlab target can also take its lab from a repo of
the project (`lab_repo`, a git worktree on the host for each target) or
from each holder's workspace (`lab_from_workspace`, one instance for each
holder, named `<target>.<agent ULID>`). For `lab_from_workspace`,
`lab_path` is relative to the workspace root `<project>/<agent>`, so a lab
at the root of repo `app` is `lab_path = "app"`. The host agent checks such
a lab against an allowlist, because an agent can write it
(docs/host-protocol.md, "The lab allowlist"). vmlab 0.33.3-alpha keys a
running lab by its name, so two holders of one `lab_from_workspace` target
cannot run at the same time (§21). Caps:
`TERRARIUM_MAX_TARGETS_PER_PROJECT` (8, `409`) and the host agent's
`max_running_targets` (4, `429`). A target has a scope: global, or one
project. A project target is named `<project>-<name>`.
With `snapshot_before_task` (vmlab only), the daemon takes the snapshot
`terrarium-clean` after `up` and before any exec, and restores a kept target
to it on agent stop (docs/api.md §17.3).

**Target leases.** `policy.targets` is static. A lead can also give a
target to an agent at run time, and take it back: `terrarium admin target
grant <target> <agent> [--until 30m] [--exclusive]` and `admin target
revoke`. An agent may use a target that its policy lists or that it holds
an active lease on. An exclusive lease keeps other agents off the target
(`409 lease_held` names the holder and its expiry). On a revoke or the
expiry, the daemon ends the agent's live sessions on the target
(`lease_revoked`) at once. A grant to a running agent brings an ephemeral
target up; the end of the last running holder brings it down, with the
keep and restore rules. The grant answers after the lifecycle verb, and
`terrarium admin target grant` exits 1 when the target did not come up
(the lease stays). The daemon types a pointer to the holder after the
grant's lifecycle and at the end of the lease, not on an extend. The lead
grants a target before it assigns the card, and revokes it after the
merge. The rights follow the policy rule (F13): a
project_admin leases its project's targets to its project's agents; a
global target needs sysadmin (docs/api.md §17.7).

The executor protocol is internal: `exec{target, cmd, cwd, env, timeout,
pty}` with streamed output and exit code, plus `cp` and lifecycle verbs. The
daemon routes each call to the right host agent or SSH connection. `terrarium
exec`, `shell`, and `cp` are thin clients of it. `shell` forwards the agent's
PTY, so a harness can drive an interactive remote session.

---

## 20. Phases

Each phase ends with something that runs end to end.

**Phase 0 — Spikes.** Scripts and a scratch Dockerfile, no product code.
Verify: `bwrap` inside the container and the seccomp settings it needs; typing
a turn into each TUI and detecting the stop hook; hook config injection per
harness without editing shared settings; session ID capture and resume for
each harness; yolo flags in interactive mode; trust, update, and telemetry
suppression; Copilot prompt extension with `--experimental` plus the per-agent `SYSTEM.md` overlay; pi's default tool approval behaviour and a Terrarium extension that posts events; which provider usage endpoints answer with the credentials we hold; each harness's device-code or paste-code login driven through a PTY with the URL and code parsed from the output; shared auth under concurrent use;
a host agent dial-out prototype that streams a PTY exec through the daemon. Output: `docs/compat.md` with facts.

**Phase 1 — Container, daemon, Claude, terminal.** justfile, Dockerfile,
entrypoint, compose file, and bind-mounted config directories (§23). `terrariumd` with SQLite and the API. Claude adapter. Operator CLI:
create, start, send, stop, status, attach. Dashboard skeleton with the agents
grid and the xterm.js terminal. No sandbox yet.

**Phase 2 — Projects, sandbox, agent CLI, mailbox, prompts.** Project
object with agents and repositories. Clone on agent create. `bwrap` sandbox.
Agent mode of the CLI with policy enforcement and project scoping. Mailbox with
pull and push. System prompt templates. Two Claude agents in one project hold a
conversation through the mailbox from inside sandboxes, each with the repo
checked out.

**Phase 2b — Kanban board.** Board tables, rules, agent CLI commands, push on
assignment, board view in the dashboard. End-to-end: the operator drops three
cards in `ready`, two agents claim them, work, comment, and move them to
`review`, and the dashboard shows it live.

**Phase 2c — Memory graph.** Memory and link tables with FTS5, rules, agent
CLI commands, graph view in the dashboard. End-to-end: one agent writes three
linked memories while working a card; a second agent finds them by search
before starting a related card and supersedes one of them.

**Phase 2d — Project inboxes, roles, leader agents.** Project inbox
addressing, thread participants across projects, done-notification on linked
threads. Role checks in the daemon, `terrarium admin`, audit log. Project-lead
and system-lead prompt templates. End-to-end: a worker in project A mails
project B's inbox; B's lead agent makes a card, a B worker finishes it, and A's
worker gets the notification on its linked card without operator action.

**Phase 2e — Wiki, artifacts, versions.** Page and revision tables with
FTS5, wikilinks and backlinks, conflict-checked saves. Artifact store with
hashing, bundles, the separate serving origin with signed URLs, quotas.
Versions with the submit, review, and release flow. Agent CLI and dashboard
views. End-to-end: an agent pushes a built static site and a screenshot set,
writes a report page that embeds them, creates a version, links three cards,
and submits; the user previews the site inline in the dashboard and rejects
with a comment; the agent wakes from `version wait`, the linked cards carry
the comment, and a stale-base wiki save is refused with a diff.

**Phase 2f — Subscriptions and usage.** Subscription definitions, the
Subscriptions page with login detection and the add-subscription wizard
(embedded login terminal, device-code extraction, API-key form), provider
probes that passed Phase 0, local accounting from session files, alert and
pause policy, `terrarium usage`, the home-screen Usage strip. End-to-end: the
operator registers a Claude plan, a Codex plan, and an OpenRouter key for pi
from a fresh container using only the dashboard; then two
agents on different subscriptions work cards for an hour; the home screen
shows both plans' windows moving and the per-agent split matches the session
files.

**Phase 3 — Codex, Copilot, and pi adapters.** Compatibility suite in CI
against pinned versions. pi first, since it unlocks every subscription with
one adapter.

**Phase 4 — Durability.** Runner processes that outlive the daemon. Restart
policies, backoff, crash-loop detection. Reconcile on daemon and container
start. Recap turn on failed resume.

**Phase 5 — Host agents and targets.** `terrarium-host` binary with dial-out,
token auth, and the executor protocol. `ssh` target. `host`, `docker`, and
`vmlab` targets through host agents. `terrarium exec`, `shell`, `cp`.
Ephemeral lifecycle and snapshots. `terrarium repo checkout --target` through
the host agent with daemon-held credentials. Hosts and targets views in the
dashboard.

**Phase 6 — Polish.** Mail and event views. Fleet definition file that
declares many agents and targets at once. Sibling-container sandbox fallback.

### Build status (2026-10-25, v0.19)

From README.md and the tree. "Done" means built and in the tree. The
end-to-end scenarios of each phase are not scripted yet (no `spikes/e2e/`).

| Phase | Done | In flight or not started |
|---|---|---|
| 0 | Spikes for Claude Code, Codex, Copilot CLI, and pi (`spikes/<harness>/`), and the usage probes and login flows. The facts are in `docs/compat.md`. | `just spikes::login`, `spikes::usage-probe`, and `spikes::compat-matrix` are not wired. Only Claude has `just` recipes for its spikes. |
| 1 | Container, justfile, compose, `terrariumd` with SQLite and the API, Claude adapter, operator CLI, dashboard with agents and xterm.js terminals. | — |
| 2 | Projects, `bwrap` sandbox, agent mode of the CLI with policy, mailbox with pull and push, three prompt templates (`default`, `project-lead`, `system-lead`). Repo clone on agent create and on `repo checkout`, with the daemon's credential helper. Local bare repos under `TERRARIUM_REPOS_DIR` bind into the sandboxes of their project. Claude Code, Codex, and Copilot CLI run logged in, in `bwrap`, in Docker, with per-agent session files (verified live on 2026-10-25). | — |
| 2b, 2c | Board and memory graph: tables, rules, CLI, dashboard views. | — |
| 2d | Project inboxes, roles, `terrarium admin`, audit log, lead templates. Leader and worker orchestration verified live on 2026-10-25: system lead, project and lead, one worker for each card, review, merge on the operator's mail, and teardown. | — |
| 2e | Wiki, artifacts with the separate serving origin, versions with review. | — |
| 2f | Subscriptions with file and API definitions, verified probes (Anthropic, OpenAI Codex, GitHub Copilot), local accounting from session files, the quota policy (alerts and holds), daily project budgets, API keys, expired-login mail, and the login wizard (embedded terminal, URL and device-code extraction). `terrarium usage`. Dashboard Subscriptions and Spend pages. | — |
| 3 | Codex, Copilot, and pi adapters, registered in the daemon. `just ci::compat` (crate `terrarium-compat`). Claude passes every step of the suite. Codex and Copilot run live through the daemon (2026-10-25): turns, usage, clean stop, and per-agent sessions. | pi has not run live through the daemon (no login on this host). On this host the suite skips Codex and Copilot (operator hooks in the shared home), and pi runs only on opt-in. |
| 4 | One `terrarium-runner` process per agent (the default runner mode): the agents live through a daemon restart. Reconcile on daemon start (`Supervisor::recover`), reconnect on a dropped control connection, restart policies with backoff and crash-loop detection, resume of the same session, recap turn. The entrypoint supervisor loop starts `terrariumd` again after a crash in Docker, and the runners keep running (verified with `just docker::crash-test`, `docker/README.md`). The `stopped_by` mark in `runner-exit.json` gives resume after a container stop. | — |
| 5 | `terrarium-host` with dial-out, token auth, and the executor protocol. `host`, `ssh`, `docker`, and `vmlab` targets. `terrarium exec`, `shell`, `cp`. Lifecycle verbs, ephemeral targets, snapshots. `repo checkout --target` through the host agent. Hosts and Targets views in the dashboard. TLS on the host link with certificate pinning (`just host::cert`), token rotation and one-time enrol tokens (`terrarium admin host new\|rotate --enrol`), and `just host::install <user@host> <name>` (`scripts/host-install.py`, with `--dry-run`). | `just host::install` over ssh is not tried on a real remote host. |
| 5 · docker | Verified live on 2026-10-25 (`docs/host-protocol.md`, "Verified live"): TLS with a pin, enrol, `up`, `exec` (stdin, PTY, 20 MB of output, timeout), `cp` both ways, ephemeral `down`, and an agent's `exec` through the tunnel. Fixed: `up` keeps a permanent container (`docker.start`), `--init` for a fast `down`, a late `eof` is not an error, and audit rows for the `up` and `down` of ephemeral targets. | — |
| 5 · vmlab | Verified live on 2026-10-25 with vmlab 0.33.3-alpha and an idle container machine (`labs/terrarium-test`): `up`, `status`, `exec`, `cp` both ways, `snapshot` and `restore`, ephemeral `down` (`vmlab destroy`), the `up` on an agent start and the `destroy` on its stop. The status and snapshot parsers pass on fixtures of the real output. | A VM machine (a template) is not tried live. `status` after `destroy` reports `stopped`, not `absent`. |
| 5 · leases | Runtime target leases (§19, `docs/api.md` §17.7): `admin target grant`, `extend`, `revoke`, `leases`; exclusive holders (`409 lease_held`); the end of live sessions at a revoke or an expiry (`lease_revoked`); the lifecycle on a grant and at the lease end; pointer turns after the lifecycle; the leases panel in the dashboard. Verified live on 2026-10-25, with the expiry of a lease of a running holder. | — |
| 5 · mode 1 | Workspaces under `TERRARIUM_WORK_DIR` (`<project>/<agent>`), bound in the sandbox at `$HOME/work` and `/work/<project>/<agent>`, and attached into targets at the same path (a docker bind, a vmlab `@dev` sync). `agent rm` moves the workspace to the trash. The templates split the work: git in the sandbox, builds and tests on the target. Verified live on 2026-10-25: two cards, each worker with its own vmlab instance, edits in the sandbox, tests on the target, merge, and teardown. | Two workers on one `lab_from_workspace` target at the same time (§21). |
| 5 · lab repos | `lab_repo` (a git worktree of a project repo on the host, `lab_ref`, `lab_path`) and `lab_from_workspace` (one instance for each holder), with the lab allowlist in the host agent (`labcheck.rs`). `admin target edit` (`PATCH /api/targets/{name}`). The dashboard shows the lab source and the instances. Verified live on 2026-10-25. | — |
| 5 · caps | `TERRARIUM_MAX_TARGETS_PER_PROJECT` (8, `409 conflict`) and the host agent's `max_running_targets` (4, `429 rate_limited`). The dashboard shows the cap errors and the running count of each host. | — |
| 5 · scope | A target is global or belongs to one project (the `scope` field). A project target gets the prefix `<project>-`. A project_admin makes and leases the targets of its project; a global target needs sysadmin. The dashboard has scope chips and filters, and a targets panel on each project. | — |
| 6 | Mail and event views in the dashboard. The fleet file (`docs/fleet.md`): `terrarium fleet validate`, `plan`, `apply` (with `--prune`), `ls`, `show`, and `export`, ownership as `source: "fleet:<name>"`, and the auto-apply of `fleets/*.toml` at start and on reload. | The sibling-container sandbox fallback is not started. |
| Security | The fixes for findings F1 to F7 of `docs/security-review.md`: the `private/` and `harness/` split of the agent dir (F1, §11); TLS on the host link with a loopback-only plaintext default, pinning, rotation, and enrol (F2, §19); DOMPurify as the one HTML sink, an ESLint guard, adversarial payload tests, and a page CSP in the dashboard (F3); the WebSocket token in the `bearer.<token>` subprotocol, with `TERRARIUM_WS_QUERY_TOKEN=0` to refuse `?token=` (F4); artifacts off the dashboard origin by default, with an origin badge in the viewer (F5); no PTY log for the login flow (F6); a constant-time token compare (F7). Signed `HttpOnly` session cookies for the dashboard, with a CSRF header on mutating calls and an `Origin` check on WebSocket upgrades (`docs/api.md` §1.1); the CSP and other security headers from the daemon (§1.2); `?token=` refused by default. The fixes for F10 to F14 of §9: fleet ownership and overwrite checks (F10); quarantine of an old agent dir with a symlink (F11); transcript path checks with the `usage.rejected_path` event (F12); sysadmin for global targets, `cross_project`, and outside `mail_to` in a policy (F13); a wider ESLint sink rule with a fixture test, and the CSP as a header with `frame-ancestors 'none'` (F14). `DELETE /api/session` needs the CSRF token when a cookie is present. | — |

---

## 21. Risks

| Risk | Mitigation |
|---|---|
| Typing into a TUI is fragile (prompt not ready, paste mangled) | Gate on `idle` plus a screen check. Bracketed paste. Confirm with the prompt-submit hook. Retry once. |
| Hooks do not fire or change shape | Fall back to PTY silence heuristics. Pin versions. Compatibility suite. |
| `bwrap` blocked by Docker seccomp | Document the required `--security-opt`. Sibling-container fallback. |
| Shared harness home under concurrent writers | **Mitigated** for the files that collide: each agent gets per-agent overlays for them (next row, §11). Two Claude agents at once are verified; a test under load is not done. |
| Harnesses that write a per-agent file by rename (Copilot `settings.json`, `command-history-state.json`, `open-sessions-state.json`) get EBUSY on a file bind, and the write is lost | Copilot start fixed by the settings key instead of the flag. Lost state writes are harmless but a per-agent `COPILOT_HOME` with symlinks to the shared `config.json`, `session-state/`, and `extensions/` would remove the file binds; open item. |
| Shared Claude home: `history.jsonl`, `sessions/<pid>.json`, `backups/`, and auto memory are written by every agent | **Mitigated.** Per-agent overlays for all four, verified with two agents at once and a real `--resume` (spikes/claude/concurrency). The overlays live in the agent's `harness/` dir and are its only writable binds there (§11). |
| Shared session files: every agent could read, and write into, the transcripts of every other agent in every project (Claude `projects/<cwd>/`, Codex `sessions/` and its SQLite files, Copilot `session-state/` and `session-store.db`), a path for prompt injection | **Fixed**: per-agent overlays for all of them (§11); the usage collector reads only the agent's own session dir (F12). Verified live on 2026-10-25 for Claude, Codex, and Copilot: a turn on the new binds writes only to the agent's own dir. Transcripts from before the change stay in the shared home, where no agent sees them. |
| Agent-to-agent prompt injection | Mail is untrusted. Push only a pointer, not the body. Delimit on read. Say so in the prompt. |
| Runaway cost with yolo and `always` restart | Per-agent turn and time budgets. Crash-loop detection. Dashboard shows turn counts. |
| Yolo agent escapes through `terrarium exec` | Policy limits targets per agent. Ephemeral targets by default for risky work. |
| An agent writes a lab file (`lab_repo`, `lab_from_workspace`) that reaches the host: a `provision` wscript, a host path, a host port, a host device, raw QEMU flags | **Mitigated** by the allowlist in the host agent (`labcheck.rs`, `docs/host-protocol.md` "The lab allowlist"). The check runs before `vmlab validate`, refuses all that is not in the list, and names each line. The only host paths are the attached workspace and a relative `@dev(workspace)` in the lab's own directory. A lab that needs more is a pre-provisioned `lab` target that the operator writes. |
| vmlab 0.33.3-alpha keys a running lab by the name in its `lab` block, so a second instance of one lab file fails (`exit 5`, "already registered"). Two holders of one `lab_from_workspace` target cannot run at the same time. | Open, until vmlab runs one instance for each directory. Until then, the lead gives such a target to one worker at a time, or the labs use two names. |
| Agents fight over cards or thrash the board | Claim is atomic and one card at a time. Agents move only their own cards. WIP limits. Every change is attributed in the event log. |
| A leader agent runs amok (spawns many agents, deletes work) | Roles are least-privilege. Destructive actions need `--confirm` and are audited. Per-project caps on agent count and on spawns per hour. Dashboard banner on deletes. |
| Cross-project spam through inboxes | Rate limit per sender. Admins can mute a project. Every message is attributed. |
| Wiki edit conflicts between agents | Saves carry a base revision. Section-level and append edits. Refused saves return a diff. Every revision is kept. |
| Login flow output changes and the wizard cannot parse the URL or code | The embedded terminal always shows the raw flow, so the operator can finish it by hand. Parsers are per-harness, pinned, and tested against captured output. |
| API keys or OAuth codes leak through the dashboard | Keys are write-only through the API, stored 0600, injected as env only into the agents on that subscription. The login PTY is operator-only, short-lived, and audited. |
| Provider usage endpoints are undocumented and change | Treat probes as best-effort with a last-success time. Local accounting is always present. Each probe is a small isolated module with a test fixture. |
| A plan hits its limit mid-task and agents thrash | Pause at 100 percent, mark `blocked` with the reset time, resume after reset. Alert at a threshold so the operator or a sysadmin agent can move agents to another subscription first. |
| An uploaded HTML artifact attacks the dashboard | Separate serving origin, signed short-lived URLs, sandboxed iframes without same-origin, sniffed content types, no script access to the API. With the artifact listener on (the default), the dashboard port does not serve `/a/...`; the viewer shows a badge when artifacts share the dashboard origin (F5 below). |
| Artifact storage grows without bound | Content-hash dedupe, per-project quotas, retention tied to released versions and live pages, pruning with a day's warning. |
| Wiki goes stale and misleads | Review status on pages. Backlinks show what depends on a page. The lead template includes a periodic wiki review. |
| Memory bloat and near-duplicates | `mem new` shows top hits before writing. Supersede instead of delete. Dashboard dedupe hints. Operator can prune. |
| A wrong memory misleads every later agent | Attribution on every node and link. `contradicts` and `supersedes` surface first. The protocol asks for a cited source. |
| Board drifts from reality | The protocol asks for comments at checkpoints and a `review` or `blocked` move before idle. The dashboard flags `in_progress` cards whose assignee has been idle for too long. |
| Git credentials reach an agent | Clones and fetches run outside the sandbox, by the daemon or a host agent. Pushes use a credential helper that the daemon answers per project. Tokens never enter the sandbox filesystem. |
| Host agent token leaks or a host is compromised | One token per host, stored as a hash, revocable from the dashboard. Rotation (`admin host rotate`) and one-time enrol tokens (`--enrol`, 10 minutes). `first_used_at` and `last_used_from` show a strange use. TLS on the link (F2 below). The host agent only accepts calls for targets defined on it. |
| Host agent connection drops mid-exec | Exec sessions have IDs. The host agent keeps output buffered until acknowledged. Reconnect resumes or reports the exit code. |
| Harness flags change | One adapter per harness, pinned version, CI. |
| A daemon crash inside Docker stops every runner, because `terrariumd` is the only child of `tini`: when it exits, the container stops | **Mitigated.** The entrypoint is a supervisor loop: it starts `terrariumd` again after a crash, with backoff, and the runners keep running (`docker/README.md`, "The restart loop"; `just docker::crash-test`). Outside Docker, the runner processes live through a daemon restart. |
| F1: a yolo agent rewrites its own event log, PTY log, launch spec, or exit record, and so defeats the budgets, the audit trail, and resume | **Fixed.** The agent dir has `private/`, `harness/`, and `workspace/`. The sandbox never binds `private/`, binds `harness/` read-only, and makes only the per-agent overlays writable (§11, `docs/runner.md` "Files"). A test checks that no bind names `private/`. |
| F2: the host link is plaintext, so the host token and the exec traffic cross the network in clear text and the token replays | **Fixed.** TLS on the host listener with `TERRARIUM_HOST_TLS_CERT` and `TERRARIUM_HOST_TLS_KEY`, certificate pinning in the host agent, rotation, and enrol tokens (§19). Without a certificate, the host listener binds `127.0.0.1:8082` only (`TERRARIUM_HOST_LISTEN=auto`). A plaintext bind to another address logs an `INSECURE` warning. The host agent refuses `ws://` to a host that is not loopback without `allow_insecure`. |
| F3: untrusted agent markdown reaches `innerHTML` through a hand-written sanitizer | **Fixed.** DOMPurify is the one HTML sink (`lib/sanitize.ts`), with an ESLint guard against other sinks, adversarial payload tests, and a page CSP. The dashboard keeps no token after sign-in (next row). |
| F3 residual: a script that gets past the sanitizer reads the dashboard token from storage | **Mitigated.** The dashboard trades the token for a signed `HttpOnly` session cookie (`POST /api/session`) and keeps only the CSRF value. Mutating calls with the cookie need `X-CSRF-Token`. A WebSocket upgrade with the cookie needs the dashboard `Origin`. The daemon sends the CSP as a response header. The cookie ends after `TERRARIUM_SESSION_TTL` (default `12h`) (`docs/api.md` §1.1, `dashboard/src/README.md`). |
| F4: the bearer token rides in `?token=` on WebSocket upgrades, so it reaches proxy logs and browser history | **Fixed.** The token goes in the subprotocol `bearer.<token>`, and the daemon echoes it. The daemon refuses `?token=` by default; `TERRARIUM_WS_QUERY_TOKEN=1` turns it back on, with a warning (`docs/api.md` §1). |
| F5: artifacts share the dashboard origin by default | **Fixed.** With the artifact listener on (the default), the dashboard port does not serve `/a/...`. `TERRARIUM_ARTIFACT_PUBLIC_URL` sets the origin of signed URLs. The viewer shows a badge when artifacts share the dashboard origin. |
| F6: the login PTY log holds the device code until cleanup | **Fixed.** The login flow writes no `pty.log` (`log_max_bytes = 0`, `docs/runner.md` "Files"). |
| F7: the token compare leaks the token length | **Fixed.** The daemon compares SHA-256 hashes of the two values in constant time. |
| F10 to F13: a member overwrites any fleet record; an old agent dir split follows planted symlinks; the usage collector tails any file that a hook names; a project_admin grants global targets | **Fixed.** See `docs/security-review.md` §9 for each fix. |

---

## 22. Open items

1. Codex: confirm the config key for an external instructions file, and where
   the session ID of a new interactive session is first visible.
2. Copilot: confirm the per-agent `SYSTEM.md` overlay works with the extension
   under concurrent agents, and how hooks are registered.
3. Claude: confirm the `~/.claude.json` trust pre-seed, and that `--settings`
   hook injection works in interactive mode.
4. Host agent transport details: WebSocket framing, reconnection, and how
   long to buffer exec output while disconnected.
5. Dashboard auth: single bearer token in v1. Anything more later.
6. Login flows: capture the exact output of `claude auth login`,
   `codex login --device-auth`, and `copilot login --device-code` inside the
   container, and confirm whether pi can reuse the Claude Code and Codex OAuth
   tokens or needs its own `/login`.
7. Usage probes: confirm each provider endpoint, its auth, its rate limit, and
   whether it reports the same numbers the harness shows in its own UI. pi:
   confirm tools run without approval by default and which session file
   format it writes.
8. Memory retrieval: add embedding search after v1 if full-text plus graph
   proves too weak. Decide on a local embedding model versus an API.
9. Export: mirror a project's wiki and embedded artifacts, and later its
   memories, to a repo, so they are reviewable in git. Decide whether this is
   one-way or two-way. Wiki first, since page paths already map to file paths.
10. Artifact serving origin: a second port on the same container versus a
   subdomain. A second port is simplest for a single-container deploy; decide
   how the signed URL carries the token (query string versus cookie on that
   origin).
11. Large artifacts: chunked upload through the runner socket, and whether
   bundles over a size threshold should be served from disk directly rather
   than unpacked into the store.

## 23. Repository layout, build, run, and test

### 23.1 Layout

```
terrarium/
  justfile                      entry point: main, exports, mod declarations, aggregates
  .just/
    shared.just                 require-* helpers (docker, cargo, node, just, uv, ssh)
    ci/mod.just                 the merge bar: check = fmt-check + lint + test + dashboard-check
    docker/mod.just             image build, compose up/down/logs/shell, volume init
    daemon/mod.just             cargo build/run/test for terrariumd and the CLI
    dashboard/mod.just          npm install/dev/build/test for the SolidJS app
    host/mod.just               build and package terrarium-host, install to a host over ssh
    spikes/mod.just             Phase 0 scripts: login flows, bwrap-in-docker, hook capture
  Cargo.toml                    workspace
  crates/
    terrarium-core/             domain, events, adapter trait, executor protocol
    terrariumd/                 daemon, API, supervisor, embedded dashboard
    terrarium/                  CLI, operator and agent modes
    terrarium-runner/           one harness in a PTY: screen, logs, hook socket, status
    terrarium-host/             host agent
    adapters/{claude,codex,copilot,pi}/
  dashboard/                    SolidJS + Vite
  docker/
    Dockerfile                  multi-stage, see 23.3
    entrypoint.sh               volume init, extension install, exec terrariumd
    seccomp-bwrap.json          Docker seccomp profile that permits user namespaces
    seccomp-bwrap.README.md     what the profile changes, and why
  compose.yaml
  .env.example                  ports, token, UID/GID
  config/                       bind-mounted harness homes (gitignored, except README)
    claude/  codex/  copilot/  pi/
  state/                        bind-mounted daemon state (gitignored)
  work/                         agent workspaces <project>/<agent>/, bound at /work (gitignored)
  repos/                        local bare repos, bound at /repos (gitignored)
  labs/                         vmlab test labs for a host agent on this machine
  prompts/                      system prompt templates, mounted read-only
  projects/  agents/  targets/  subscriptions/
                                definition files, bind-mounted under /etc/terrarium (§23.4);
                                `just docker::volumes-init` creates them
  docs/
    PLAN.md  compat.md  cli.md  api.md  agent-rpc.md  runner.md
    decisions/                  planned (§24 step 4), not yet written
  spikes/                       Phase 0 scripts and captures: claude/ codex/ copilot/ pi/
```

### 23.2 justfile

Multi-file under `.just/`, following the house norms: a private `main` in
every file, every public recipe grouped and documented, noun-verb names,
dependency chaining, `require-*` guards from `shared.just`, Python via `uv`
for anything beyond a few lines, and a `ci` module whose `check` is the merge
bar. The top level:

```just
set dotenv-load

export TERRARIUM_IMAGE := env("TERRARIUM_IMAGE", "terrarium:dev")
export TERRARIUM_PORT  := env("TERRARIUM_PORT", "8080")
export TERRARIUM_UID   := env("TERRARIUM_UID", `id -u`)
export TERRARIUM_GID   := env("TERRARIUM_GID", `id -g`)

[default, private]
main:
	@just --list

# Verification gate
mod ci '.just/ci'
# Image and compose lifecycle
mod docker '.just/docker'
# Daemon and CLI (cargo)
mod daemon '.just/daemon'
# SolidJS dashboard
mod dashboard '.just/dashboard'
# Host agent build and install
mod host '.just/host'
# Phase 0 spike scripts
mod spikes '.just/spikes'

# Build everything: crates, dashboard, image
[group('build')]
build: daemon::build dashboard::build docker::image-build

# Test everything that runs without a container
[group('test')]
test: daemon::test dashboard::test

# Build the image and start the stack
[group('run')]
up: docker::image-build docker::up

# Stop the stack
[group('run')]
down: docker::down
```

Recipes the modules provide (names, not bodies):

| Module | Recipes |
|---|---|
| `ci` | `check`, `fmt-check`, `lint` (clippy, `-D warnings`), `test`, `dashboard-check` (typecheck, lint, and the dashboard unit tests: `npm run check`, then `npm run test`), `compat` (adapter compatibility suite against pinned harness versions; excluded from `check` because it needs logins, and the doc comment says so) |
| `docker` | `image-build`, `up`, `down`, `restart`, `logs`, `shell` (exec into the container as the terrarium user), `volumes-init` (creates `config/*` and `state/` with the right ownership), `config-edit harness` (opens `config/<harness>` in `$EDITOR`), `image-push tag` |
| `daemon` | `build`, `run` (local, outside Docker, for fast iteration), `test`, `cli args` (runs the operator CLI against a running daemon) |
| `dashboard` | `install`, `dev` (Vite dev server proxying to the daemon API), `build`, `test`, `check` |
| `host` | `build` (static musl), `package`, `install host name` (binary, unit, config with the pin, and an enrol token over ssh; then a probe; `--dry-run`), `token host [enrol]` (mints a host agent token or an enrol token), `cert name` (the TLS certificate of the host listener) |
| `spikes` | `bwrap-check`, `claude-auth-check`, `claude-bwrap-check`, `claude-detector-check`, `claude-prompt-check *variant`, `claude-session-check`, `claude-sources-check`, `claude-typing-check`, `claude-yolo-check *variants`, `claude-yolo-root-check`, `hooks-capture harness idle_s='70'` (only claude has it), and three that are not wired yet: `login harness`, `usage-probe provider`, `compat-matrix` (`docs/compat.md` is written by hand from the captures) |

### 23.3 Dockerfile

Multi-stage, one image:

1. `rust` stage: `cargo build --release` for `terrariumd` and `terrarium`.
   `terrarium-host` is built in the same stage for `just host::build`, but is
   not part of the image.
2. `node` stage: `npm ci && npm run build` for the dashboard; the output is
   embedded into `terrariumd` at build time with `rust-embed`, so the Rust
   stage depends on it.
3. Runtime stage, Debian slim: Node 22 (22.19+, which pi needs), `bubblewrap`, `git`, `openssh-
   client`, `ca-certificates`, and the harnesses installed with pinned
   versions: `@anthropic-ai/claude-code`, `@openai/codex`, `@github/copilot`,
   `@earendil-works/pi-coding-agent` (pi's package after `@mariozechner/pi-coding-agent` 0.73.1),
   plus `copilot-custom-system-prompt`.
   A non-root user `terrarium` whose UID and GID are build args, so files in
   the bind mounts are owned by you on the host. `HOME=/home/terrarium`, with
   `~/.claude`, `~/.codex`, `~/.copilot`, `~/.pi/agent` as symlinks into
   `/auth/<harness>`, and `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `COPILOT_HOME`,
   `PI_CODING_AGENT_DIR` set to the same paths for harnesses that prefer the
   env var.

`entrypoint.sh`: create missing directories under `/auth` and `/state`, fix
ownership if the mount is empty, run `copilot-custom-system-prompt install`
when the extension is missing or the image version changed, then `exec
terrariumd`.

### 23.4 compose.yaml

```yaml
services:
  terrarium:
    build:
      context: .
      dockerfile: docker/Dockerfile
      args: { UID: "${TERRARIUM_UID}", GID: "${TERRARIUM_GID}" }
    image: ${TERRARIUM_IMAGE}
    restart: unless-stopped
    ports:
      - "${TERRARIUM_PORT}:8080"       # dashboard and API
      - "${TERRARIUM_ARTIFACT_PORT}:8081"  # artifact serving origin (§15.2)
      - "${TERRARIUM_HOST_PORT}:8082"  # host agent dial-in (§19)
    environment:
      TERRARIUM_TOKEN: ${TERRARIUM_TOKEN}
      TERRARIUM_PUBLIC_URL: ${TERRARIUM_PUBLIC_URL}
      TERRARIUM_HOST_LISTEN: ${TERRARIUM_HOST_LISTEN:-auto}   # 127.0.0.1:8082 without TLS (§19)
      TERRARIUM_HOST_TLS_CERT: ${TERRARIUM_HOST_TLS_CERT:-}
      TERRARIUM_HOST_TLS_KEY: ${TERRARIUM_HOST_TLS_KEY:-}
    volumes:
      - ./config/claude:/auth/claude
      - ./config/codex:/auth/codex
      - ./config/copilot:/auth/copilot
      - ./config/pi:/auth/pi
      - ./state:/state
      - ./prompts:/prompts:ro
      - ./subscriptions:/etc/terrarium/subscriptions
      - ./projects:/etc/terrarium/projects
      - ./agents:/etc/terrarium/agents
      - ./targets:/etc/terrarium/targets
      - ./fleets:/etc/terrarium/fleets       # fleet files, applied at start and on reload
      - ./repos:/repos                       # local bare repos (TERRARIUM_REPOS_DIR)
      - ./work:/work                         # agent workspaces (TERRARIUM_WORK_DIR); a host agent's work_dir
      - ./config/tls:/etc/terrarium/tls:ro   # `just host::cert` writes here
    security_opt:
      - seccomp:docker/seccomp-bwrap.json   # user namespaces for bwrap (§11)
      - apparmor:unconfined
      - systempaths=unconfined              # bwrap --proc needs an unmasked /proc
    healthcheck:
      test: ["CMD", "terrarium", "daemon", "status"]
      interval: 30s
```

The seccomp profile alone is not enough for `bwrap`. Docker masks paths
under `/proc` with bind mounts, and the kernel then refuses a new procfs mount
in a user namespace, so `bwrap --proc /proc` fails. `systempaths=unconfined`
removes the masks without extra capabilities (§11).
`docker/seccomp-bwrap.README.md` lists what the profile changes and why.

Why bind mounts and not named volumes: you asked to reach and edit harness
settings from outside. With `./config/<harness>` on the host, `config/claude/
settings.json`, `config/codex/config.toml`, `config/copilot/`, and
`config/pi/settings.json` are ordinary files you open in an editor. The same
holds for the Terrarium definitions (`projects/`, `agents/`, `targets/`,
`subscriptions/`, `prompts/`), so a project can be declared in a file and
committed. The daemon watches those directories and reloads on change.
`config/` and `state/` are gitignored; `.env.example` documents every
variable.

### 23.5 Test strategy

- **Unit** (`just daemon::test`, `just dashboard::test`): domain rules (board,
  roles, mailbox scoping, wiki conflicts), adapters against captured harness
  output and hook payloads, PTY typing against a fake TUI, probes against
  recorded responses.
- **Integration** (`just ci::test`): the daemon with SQLite in a temp dir, the
  API end to end, a fake harness binary that behaves like a TUI with hooks, so
  supervisor, runner, mailbox push, and resume are exercised without a real
  subscription.
- **Compatibility** (`just ci::compat`): the real harnesses at pinned
  versions, inside the image, with real logins from `config/`. One scenario
  per adapter: start, type a turn, see the hooks, resume, system prompt
  applied. Runs on demand and nightly, not in the merge bar.
- **End to end** per phase (§20): the scenarios written in each phase run as a
  script under `spikes/e2e/` against `just up`.

---

## 24. First concrete steps

1. `git init`. Lay out the repo as in §23.1: the Cargo workspace, the
   `dashboard/` app, the `justfile` with its `.just/` modules, `.gitignore`
   for `config/` and `state/`.
2. `docker/Dockerfile`, `docker/entrypoint.sh`, `docker/seccomp-bwrap.json`,
   `compose.yaml`, `.env.example`. `just up` should start an empty daemon with
   the dashboard reachable before any adapter exists.
3. `docs/compat.md` as an empty matrix. `spikes/` scripts fill it.
4. `docs/decisions/` with one short ADR per row of §1.

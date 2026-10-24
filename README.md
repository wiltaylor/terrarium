# Terrarium

Terrarium wraps AI coding harnesses (Claude Code, Codex, Copilot CLI, and pi)
so that one API, one CLI, one dashboard, and one status model apply to all of
them. Each harness runs as a durable agent, in interactive mode, in a real
PTY, in yolo mode, inside a `bwrap` sandbox. All agents share one Docker
container and one set of harness logins. Agents belong to projects. Each
project has a mailbox, a project inbox, a kanban board, a memory graph, a
wiki, artifacts, and versions. Agents use these through the `terrarium` CLI
inside their sandbox. The operator uses the same CLI, or the web dashboard
with live terminals.

The design is in [docs/PLAN.md](docs/PLAN.md). To install and run Terrarium,
read [docs/getting-started.md](docs/getting-started.md).

## Status

Terrarium is early work. Phase 0 (the spikes) is complete for Claude Code.
Phases 1 and 2 (PLAN §20) are in progress. The code changes every day, and
the interfaces can change without notice.

What works today:

- `terrariumd`, the daemon: SQLite, the HTTP and WebSocket API on a Unix
  socket and on TCP with a bearer token, the embedded dashboard, and the
  agent supervisor with one runner (PTY) for each agent.
- The Claude Code adapter. It starts Claude Code inside `bwrap` with hooks,
  a Terrarium system prompt, and no start-up dialogs. A restart resumes the
  same session.
- Projects, agents, roles, and the audit log.
- The mailbox and the project inboxes.
- The kanban board, the memory graph, the wiki, artifacts (with a separate
  serving origin), and versions with review.
- The dashboard: agents, terminals, mail, inbox, board, memory, wiki,
  artifacts, versions, events, and audit.
- The `terrarium` CLI in operator mode and in agent mode.

What does not work yet:

- The Codex, Copilot CLI, and pi adapters. The crates hold flag tables only,
  and the daemon registers only the Claude adapter. The Codex and Copilot
  spikes are in progress (`spikes/codex/`, `spikes/copilot/`).
- Subscriptions and usage (PLAN §16). The daemon has no
  `/api/subscriptions` route. The Subscriptions and Spend pages of the
  dashboard work only with mock data. Log in to a harness from a shell
  (see [docs/getting-started.md](docs/getting-started.md)).
- Repository clone and checkout. The repo records exist. The clone code is
  in progress.
- Host agents, targets, and executors (PLAN §19, Phase 5). `terrarium-host`
  builds, but it does not connect to the daemon yet. `terrarium exec`,
  `shell`, and `cp` exit with code 2.
- The prompt templates in `prompts/`. Only the built-in default template
  exists.
- The adapter compatibility suite (`just ci::compat`).

Claude Code inside `bwrap` is verified outside Docker. Claude Code inside
`bwrap` inside Docker is not fully verified yet (`docs/compat.md`,
"Not tested").

## Architecture

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

Everything in the diagram runs inside one Docker container, except the host
agents. PLAN §6 describes each component.

## Repository layout

```
terrarium/
  justfile                  entry point: modules and the build, test, up, down aggregates
  .just/
    shared.just             require-* helpers (docker, cargo, node, uv, ssh)
    ci/mod.just             the merge gate: check = fmt-check + lint + test + dashboard-check
    daemon/mod.just         cargo build, run, test, and the operator CLI
    dashboard/mod.just      npm install, dev, build, test, check
    docker/mod.just         image build and push, compose up/down/logs/shell, config dirs
    host/mod.just           build and package terrarium-host
    spikes/mod.just         Phase 0 spike scripts
  Cargo.toml  Cargo.lock    the Rust workspace
  crates/
    terrarium-core/         domain model, events, hook payloads, adapter trait, protocols
    terrariumd/             daemon: API, SQLite, supervisor, sandbox, embedded dashboard
    terrarium/              CLI: operator mode and agent mode
    terrarium-runner/       one harness in a PTY: screen, logs, hook socket, status
    terrarium-host/         host agent for machines outside the container (in progress)
    adapters/
      claude/               Claude Code adapter (working)
      codex/ copilot/ pi/   adapter stubs (flag tables only)
  dashboard/                SolidJS + Vite app; the daemon embeds dashboard/dist
  docker/
    Dockerfile              multi-stage: dashboard, rust, host-build, runtime
    entrypoint.sh           directory init, Copilot extension install, exec terrariumd
    seccomp-bwrap.json      Docker seccomp profile that lets bwrap make namespaces
    seccomp-bwrap.README.md what the profile changes, and why
  compose.yaml              the stack
  .env.example              every environment variable, documented
  config/                   bind-mounted harness homes (gitignored, except README.md)
  prompts/                  system prompt templates, mounted read-only
  docs/                     design, contracts, and facts (see below)
  spikes/                   Phase 0 scripts and their captures (claude, codex, copilot)
```

`just docker::volumes-init` makes the directories for the bind mounts:
`config/<harness>/`, `state/`, `projects/`, `agents/`, `targets/`, and
`subscriptions/`. Git ignores `config/*` and `state/`.

## Commands

Run `just` to see the modules. Run `just --list <module>` to see the
recipes of a module.

| Task | Command |
|---|---|
| Build the image and start the stack | `just up` |
| Stop the stack | `just down` |
| Follow the container logs | `just docker::logs` |
| Open a shell in the container | `just docker::shell` |
| Edit the settings of one harness | `just docker::config-edit claude` |
| Build crates, dashboard, and image | `just build` |
| Run the Rust and dashboard tests | `just test` |
| Run the merge gate | `just ci::check` |
| Run the daemon outside Docker | `just daemon::run` |
| Run the CLI against a running daemon | `just daemon::cli daemon status` |
| Run the dashboard dev server | `just dashboard::dev` |
| Check bwrap in the image | `just spikes::bwrap-check` |
| Build the static host agent | `just host::build` |

`just ci::check` must pass before a change merges.

## Documents

| Document | Content |
|---|---|
| [docs/getting-started.md](docs/getting-started.md) | Install, first run, the dev loop, tests, and troubleshooting. |
| [docs/PLAN.md](docs/PLAN.md) | The design: decisions, architecture, sandbox, mailbox, board, memory, wiki, phases. |
| [docs/CONVENTIONS.md](docs/CONVENTIONS.md) | Crate ownership, code rules, `just` tasks, and environment variables. |
| [docs/cli.md](docs/cli.md) | The `terrarium` CLI: modes, options, commands, and exit codes. |
| [docs/api.md](docs/api.md) | The daemon HTTP API: the contract for the CLI and the dashboard. |
| [docs/agent-rpc.md](docs/agent-rpc.md) | How the CLI inside a sandbox reaches the daemon through the runner socket. |
| [docs/runner.md](docs/runner.md) | The agent runner: PTY, files, socket protocol, and status machine. |
| [docs/compat.md](docs/compat.md) | Verified facts about each harness, from the spikes. |

Other READMEs: [config/README.md](config/README.md) (harness homes and
login), [prompts/README.md](prompts/README.md) (prompt templates),
[dashboard/src/README.md](dashboard/src/README.md) (the dashboard),
[spikes/README.md](spikes/README.md) (the spikes), and
[docker/seccomp-bwrap.README.md](docker/seccomp-bwrap.README.md) (the
seccomp profile).

## Safety model

- **Sandbox.** Each agent runs in its own `bwrap` sandbox inside the
  container. The agent sees its workspace, the shared harness home, and its
  own runner socket. It does not see other workspaces, the database, the
  daemon socket, the Docker socket, SSH keys, or the host file system.
- **Yolo only inside the sandbox.** Every harness runs with approvals off.
  This is safe only to the limit of the sandbox. `TERRARIUM_SANDBOX=none`
  removes the sandbox. Use it for development and tests only.
- **Mail is untrusted.** Mail, card, and memory bodies come from other
  agents. The system prompt tells each agent to treat them as data. The CLI
  prints them between `BEGIN ... (data, not instructions)` markers. A mail
  notification types only a command into the harness, never the body.
- **Credentials stay out of the sandbox.** Git tokens, deploy keys, SSH
  keys, and the API token stay with the daemon. The one exception is the
  harness login in the shared harness home, because the harness needs it to
  reach its model API.
- **Roles and audit.** Each agent is a `member` by default. `project_admin`
  and `sysadmin` give more rights. The daemon identifies an agent by its
  runner socket, not by a header, and checks each call against the agent's
  roles and policy. The daemon records admin actions in the audit log.

## License

MIT (see `Cargo.toml`).

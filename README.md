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

Terrarium is early work. Phases 0 to 5 of PLAN §20 are largely built, and
Phase 6 is in part. PLAN §20, "Build status", gives the details. The code
changes every day, and the interfaces can change without notice.

What works today:

- `terrariumd`, the daemon: SQLite, the HTTP and WebSocket API on a Unix
  socket and on TCP with a bearer token, and the embedded dashboard.
- The external runner. Each agent runs in its own `terrarium-runner`
  process, which holds the PTY. The agents live through a daemon restart:
  at start, the daemon connects to each live runner again and replays the
  events that it missed. `TERRARIUM_RUNNER_MODE=inprocess` keeps the
  runners in the daemon, for tests and development.
- Four adapters: Claude Code, Codex, Copilot CLI, and pi. The daemon
  registers all four.
  - Claude Code is verified end to end inside `bwrap`, with the external
    runner, outside Docker. A restart resumes the same session.
  - Codex, Copilot CLI, and pi are built against the Phase 0 captures
    (`spikes/<harness>/`, [docs/compat.md](docs/compat.md)). They have not
    run live through the daemon yet.
- The adapter compatibility suite, `just ci::compat`
  ([docs/compat-suite.md](docs/compat-suite.md)). Claude passes every
  step.
- Projects, agents, roles, and the audit log.
- The mailbox and the project inboxes.
- The kanban board, the memory graph, the wiki, artifacts (with a separate
  serving origin), and versions with review.
- Repository checkout: a clone on agent create and on
  `terrarium repo checkout`, in the workspace or on a target. The daemon
  holds the git credentials and answers the credential helper.
- Subscriptions and usage: file and API definitions, verified quota probes
  for Anthropic, OpenAI Codex, and GitHub Copilot, local accounting from
  the session files, alerts and holds, daily project budgets, and API keys.
  The Subscriptions page has a login wizard with an embedded login
  terminal.
- Host agents and targets: `terrarium-host` dials in to the daemon with a
  per-host token. `host`, `ssh`, `docker`, and `vmlab` targets have
  lifecycle verbs, snapshots, and ephemeral targets. `terrarium exec`,
  `shell`, and `cp` work. The vmlab verbs are verified only against the
  reference output of the vmlab CLI.
- Three prompt templates in `prompts/`: `default`, `project-lead`, and
  `system-lead`.
- The dashboard: home, projects, agents, terminals, mail, inbox, board,
  memory, wiki, artifacts, versions, subscriptions, spend, hosts, targets,
  events, and audit.
- The `terrarium` CLI in operator mode and in agent mode.

What does not work yet:

- A daemon crash inside Docker stops the container, and with it every
  runner, because `terrariumd` is the only child of `tini`. An entrypoint
  supervisor loop is in progress (PLAN §21).
- The fleet definition file and the sibling-container sandbox fallback
  (Phase 6).
- `just host::install` (Phase 5).

Claude Code inside `bwrap` is verified outside Docker. Claude Code inside
`bwrap` inside Docker is not tested yet ([docs/compat.md](docs/compat.md),
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
    ci/mod.just             the merge gate (check = fmt-check + lint + test + dashboard-check) and compat
    daemon/mod.just         cargo build, run, test, and the operator CLI
    dashboard/mod.just      npm install, dev, build, test, check
    docker/mod.just         image build and push, compose up/down/logs/shell, config dirs
    host/mod.just           build, package, and register terrarium-host
    spikes/mod.just         Phase 0 spike scripts
  Cargo.toml  Cargo.lock    the Rust workspace
  crates/
    terrarium-core/         domain model, events, hook payloads, adapter trait, protocols
    terrariumd/             daemon (lib + bin): API, SQLite, supervisor, sandbox, executors, embedded dashboard
    terrarium/              CLI: operator mode and agent mode
    terrarium-runner/       runner (lib + bin): one harness in a PTY, screen, logs, sockets, status
    terrarium-host/         host agent (lib + bin): the wire protocol and the binary for outside machines
    terrarium-compat/       the adapter compatibility suite (just ci::compat)
    adapters/
      claude/ codex/ copilot/ pi/   one adapter for each harness
  dashboard/                SolidJS + Vite app; the daemon embeds dashboard/dist
  docker/
    Dockerfile              multi-stage: dashboard, rust, host-build, runtime
    entrypoint.sh           directory init, Copilot extension install, start terrariumd
    seccomp-bwrap.json      Docker seccomp profile that lets bwrap make namespaces
    seccomp-bwrap.README.md what the profile changes, and why
  compose.yaml              the stack
  .env.example              every environment variable, documented
  config/                   bind-mounted harness homes (gitignored, except README.md)
  prompts/                  system prompt templates (default, project-lead, system-lead), mounted read-only
  docs/                     design, contracts, and facts (see below)
  spikes/                   Phase 0 scripts and their captures (claude, codex, copilot, pi)
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
| Run the compatibility suite for one harness | `just ci::compat claude` |
| Run the daemon outside Docker | `just daemon::run` |
| Run the CLI against a running daemon | `just daemon::cli daemon status` |
| Run the dashboard dev server | `just dashboard::dev` |
| Check bwrap in the image | `just spikes::bwrap-check` |
| Build the static host agent | `just host::build` |
| Register a host agent and print its token | `just host::token lab1` |

`just ci::check` must pass before a change merges.

## Documents

| Document | Content |
|---|---|
| [docs/getting-started.md](docs/getting-started.md) | Install, first run, the dev loop, tests, and troubleshooting. |
| [docs/PLAN.md](docs/PLAN.md) | The design: decisions, architecture, sandbox, mailbox, board, memory, wiki, subscriptions, targets, phases, and build status. |
| [docs/CONVENTIONS.md](docs/CONVENTIONS.md) | Crate ownership, code rules, `just` tasks, and environment variables. |
| [docs/cli.md](docs/cli.md) | The `terrarium` CLI: modes, options, commands, and exit codes. |
| [docs/api.md](docs/api.md) | The daemon HTTP API: the contract for the CLI and the dashboard. |
| [docs/agent-rpc.md](docs/agent-rpc.md) | How the CLI inside a sandbox reaches the daemon through the runner socket. |
| [docs/runner.md](docs/runner.md) | The agent runner: PTY, files, socket protocols, status machine, and reconnect. |
| [docs/host-protocol.md](docs/host-protocol.md) | The wire contract between the daemon and `terrarium-host`. |
| [docs/compat.md](docs/compat.md) | Verified facts about each harness, the usage probes, and the login flows, from the spikes. |
| [docs/compat-suite.md](docs/compat-suite.md) | The adapter compatibility suite: how to run it, its safety rules, and its steps. |

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
  keys, host agent tokens, and the API token stay with the daemon. The one
  exception is the harness credential, because the harness needs it to
  reach its model API. It is the shared harness login, mounted read-write.
  For an API-key subscription, it is the key, in the env of the agents on
  that subscription only.
- **Roles and audit.** Each agent is a `member` by default. `project_admin`
  and `sysadmin` give more rights. The daemon identifies an agent by its
  runner socket, not by a header, and checks each call against the agent's
  roles and policy. The daemon records admin actions in the audit log.

## License

MIT (see `Cargo.toml`).

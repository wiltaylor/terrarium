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

## Quick start

On your machine, in this repository, build the image and start the stack:

```sh
just up
```

On a server, over ssh. The repository is private, so no image is in a
registry: the recipe sends the source (`git archive`) and builds the image
on the server. The server needs Docker with the compose plugin, and no
access to the repository:

```sh
just deploy::server wil@terrarium.lan --public-url http://terrarium.lan:8080 --dry-run
just deploy::server wil@terrarium.lan --public-url http://terrarium.lan:8080
just deploy::update wil@terrarium.lan      # a new commit; the data stays
just deploy::rollback wil@terrarium.lan    # back to the previous deploy
```

The recipe shows the new `TERRARIUM_TOKEN` one time, and the commands to
set the password and to add a host agent. See
[getting-started §2.14](docs/getting-started.md#214-install-on-a-server).

## Status

Terrarium is early work. Phases 0 to 5 of PLAN §20 are largely built, and
Phase 6 is in part. PLAN §20, "Build status", gives the details. The code
changes every day, and the interfaces can change without notice.

### Verified live (2026-10-25)

These runs used the real stack in Docker, with real harness logins:

- **Three harnesses in the container.** Claude Code, Codex, and Copilot CLI
  run logged in, inside `bwrap`, inside the container. Each agent has its
  own session files (transcripts, session databases, history) over the
  shared harness home. An agent cannot read the sessions of another agent.
- **Leader and worker orchestration, end to end, two times.** A system
  lead (`ops/meta`) makes the project and starts its lead. The lead makes
  one worker for each card. Each worker claims its card, works on a
  branch, and moves the card to `review`. The lead reviews the diff and
  merges only after the operator mails "merge <card>". Then the lead stops
  and removes the worker. The second run used mode 1: the workspaces stay
  on the host, and each worker has its own vmlab instance.
- **A host agent on this machine.** `terrarium-host` dials in over TLS with
  a pinned certificate. It runs Docker and vmlab targets: lifecycle verbs,
  `exec`, `cp`, snapshots, and ephemeral targets.
- **Runtime target leases.** Grant, extend, revoke, and expiry. A revoke
  or an expiry ends the live sessions of the holder at once. A grant to a
  running holder brings an ephemeral target up, and the end of the last
  holder brings it down.
- **The dashboard** shows the target scope (global or project), the
  leases panel of each target, and the targets panel of each project.

Not verified live:

- pi through the daemon. pi has no login on this machine yet.
- Two concurrent instances of one vmlab lab. vmlab 0.33.3-alpha keys a
  running lab by its name, so a second instance of the same lab file
  fails. Until vmlab changes, give a `lab_from_workspace` target to one
  worker at a time.
- The sibling-container sandbox fallback (Phase 6). It is not started.

### How the pieces fit (mode 1)

The sandbox holds the harness and the `terrarium` CLI, with no project
toolchain. The workspace of each agent is a directory on the host,
`./work/<project>/<agent>`. The sandbox binds it at `$HOME/work` and at
`/work/<project>/<agent>`. A target carries the toolchain. The host agent
attaches the workspace into the target at the same `/work/<project>/<agent>`
path: a bind for Docker, an `@dev` sync for vmlab. So an agent edits files
and runs `git` in the sandbox, and runs builds and tests on the target with
`terrarium exec`. A lease decides which agent may use a shared target, and
for how long. PLAN §11 and §19 give the details.

### What is built

- `terrariumd`, the daemon: SQLite, the HTTP and WebSocket API on a Unix
  socket and on TCP with a bearer token, and the embedded dashboard.
- The external runner. Each agent runs in its own `terrarium-runner`
  process, which holds the PTY. The agents live through a daemon restart:
  at start, the daemon connects to each live runner again and replays the
  events that it missed. `TERRARIUM_RUNNER_MODE=inprocess` keeps the
  runners in the daemon, for tests and development.
- A crash-safe entrypoint in Docker. A supervisor loop in the entrypoint
  starts `terrariumd` again after a crash, with backoff. The runners and
  their harnesses keep running. `just docker::crash-test` verifies this
  ([docker/README.md](docker/README.md)).
- Four adapters: Claude Code, Codex, Copilot CLI, and pi. The daemon
  registers all four. Claude Code, Codex, and Copilot CLI are verified
  live in the container (see above). pi is built against the Phase 0
  captures (`spikes/pi/`, [docs/compat.md](docs/compat.md)).
- The adapter compatibility suite, `just ci::compat`
  ([docs/compat-suite.md](docs/compat-suite.md)). Claude passes every
  step.
- Projects, agents, roles, and the audit log.
- The mailbox and the project inboxes.
- The kanban board, the memory graph, the wiki, artifacts (with a separate
  serving origin), and versions with review.
- Repository checkout: a clone on agent create and on
  `terrarium repo checkout`, in the workspace or on a target. The daemon
  holds the git credentials and answers the credential helper. A repo
  with `push_deny` goes through the daemon's git proxy, which enforces it
  and keeps the token out of the sandbox. A local
  bare repo under `./repos` binds into the sandboxes of its project.
- Subscriptions and usage: file and API definitions, verified quota probes
  for Anthropic, OpenAI Codex, and GitHub Copilot, local accounting from
  the session files, alerts and holds, daily project budgets, and API keys.
  The Subscriptions page has a login wizard with an embedded login
  terminal.
- Host agents and targets: `terrarium-host` dials in to the daemon with a
  per-host token. `host`, `ssh`, `docker`, and `vmlab` targets have
  lifecycle verbs, snapshots, and ephemeral targets. `terrarium exec`,
  `shell`, and `cp` work.
- Targets at global or project scope. A project target gets the project
  name as a prefix (`<project>-<name>`). A project has at most
  `TERRARIUM_MAX_TARGETS_PER_PROJECT` targets (8), and a host runs at most
  its `max_running_targets` (4) at one time.
- Target leases: `terrarium admin target grant`, `extend`, `revoke`, and
  `leases`, with exclusive holders and expiry.
- Mode 1 workspaces: workspaces under `./work`, attached into docker and
  vmlab targets. A vmlab target can take its lab from a repo of the project
  (`--lab-repo`, a git worktree on the host) or from each holder's
  workspace (`--lab-from-workspace`, one instance for each holder). The
  host agent checks such a lab file against an allowlist.
- Host agent onboarding: one command on the host. `terrarium admin host
  new <name> --enrol`, or "Add host" on the Hosts page, prints
  `curl -fsSL <daemon>/install/host.sh | sudo sh -s -- --token <token>`.
  The script downloads `terrarium-host` from the daemon, checks its
  SHA-256, writes the config with the certificate pin and the enrol
  token, finds Docker and vmlab, starts the systemd unit, and probes the
  link. It also takes `--dry-run`, `--upgrade`, and `--uninstall`
  ([getting-started §2.7](docs/getting-started.md#27-connect-a-host-agent)).
  From the repo, `just host::install <user@host> <name>` does the same
  over ssh with a local build.
- The fleet file: one TOML file declares projects, repos, agents, targets,
  and subscriptions. `terrarium fleet validate`, `plan`, `apply` (with
  `--prune`), `ls`, `show`, and `export` work. The daemon applies the
  files in `fleets/` at start and on reload ([docs/fleet.md](docs/fleet.md)).
- Three prompt templates in `prompts/`: `default`, `project-lead`, and
  `system-lead`.
- The dashboard: home, projects, agents, terminals, mail, inbox, board,
  memory, wiki, artifacts, versions, subscriptions, spend, hosts, targets
  (with scope, leases, and lab instances), events, and audit.
- The `terrarium` CLI in operator mode and in agent mode.

Security fixes from the review ([docs/security-review.md](docs/security-review.md)):

- The sandbox split (F1). The agent directory has `private/` and
  `harness/`, and the workspace is under `./work`. The sandbox never binds
  `private/`, so an agent cannot change its event log, PTY log, launch
  spec, or exit record.
- TLS on the host link (F2). With `TERRARIUM_HOST_TLS_CERT` and
  `TERRARIUM_HOST_TLS_KEY`, the host listener speaks TLS, and a host agent
  can pin the certificate. Without them, the listener is plaintext on loopback
  only. Host tokens rotate, and a one-time enrol token keeps the
  long-lived token off the operator's clipboard.
- The dashboard sanitizer and CSP (F3). DOMPurify cleans every rendered
  HTML string. The daemon sends a Content-Security-Policy and other security
  headers on every answer.
- Operator password and sessions (F3, F15). The dashboard logs in with the
  operator password (`terrariumd password set`; the daemon keeps only an
  Argon2id hash) or the token, and gets a signed `HttpOnly` JWT cookie. The
  page keeps only the CSRF value, in memory. The cookie lives
  `TERRARIUM_SESSION_TTL` (default `30d`) and renews while in use, so you do
  not log in again and again. `terrarium login` keeps a CLI session per
  daemon URL. Failed logins back off per address. Mutating calls send
  `X-CSRF-Token`, and a WebSocket upgrade with the cookie must send the
  dashboard `Origin`.
- WebSocket auth by subprotocol (F4). A client sends the token as
  `Sec-WebSocket-Protocol: bearer.<token>`, not in the URL. The daemon
  refuses the old `?token=` form by default. `TERRARIUM_WS_QUERY_TOKEN=1`
  turns it back on.
- Artifacts off the dashboard origin (F5), no login PTY log (F6), and a
  constant-time token compare (F7).

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
    shared.just             require-* helpers (docker, cargo, node, uv, git, ssh)
    ci/mod.just             the merge gate (check = fmt-check + lint + test + scripts-test + dashboard-check) and compat
    daemon/mod.just         cargo build, run, test, and the operator CLI
    deploy/mod.just         install, update, roll back, and show a server; builds the image there (scripts/deploy.py)
    defaults/mod.just       the packaged defaults: apply (back up, then overwrite config/terrarium) and manifest
    dashboard/mod.just      npm install, dev, build, test, check
    docker/mod.just         image build and push, compose up/down/logs/shell, config dirs
    harness/mod.just        harness versions: check, update (scripts/harness.py)
    host/mod.just           build, package, and register terrarium-host
    spikes/mod.just         Phase 0 spike scripts
  Cargo.toml  Cargo.lock    the Rust workspace
  harnesses.toml            the harness pins: the one place to change a harness version
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
    entrypoint.sh           directory init, the harness install into ./tools, the terrariumd supervisor loop
    crash-test.py           kill terrariumd and check that the runners live (just docker::crash-test)
    seccomp-bwrap.json      Docker seccomp profile that lets bwrap make namespaces
    seccomp-bwrap.README.md what the profile changes, and why
  compose.yaml              the stack
  .env.example              every environment variable, documented
  config/                   bind-mounted harness homes, config/tls, and the config repo config/terrarium (gitignored, except README.md)
  fleets/                   fleet files that the daemon applies at start and on reload
  labs/                     vmlab test labs (labs/terrarium-test), for a host agent on this machine
  tools/                    the harnesses (one npm prefix per version), bound at /opt/terrarium/tools (gitignored)
  repos/                    local bare git repos, bound at /repos (gitignored)
  work/                     agent workspaces, <project>/<agent>/, bound at /work (gitignored)
  examples/fleet/           an example fleet file
  scripts/                  install-host.sh (served at /install/host.sh), host-install.py (just host::install), deploy.py (just deploy::...), and their tests
  defaults/                 the packaged defaults: personas and protocol templates (prompts/), harness settings (harness/), manifest.json; copied into config/terrarium at each start
  docs/                     design, contracts, and facts (see below)
  spikes/                   Phase 0 scripts and their captures (claude, codex, copilot, pi)
```

`just docker::volumes-init` makes the directories for the bind mounts:
`config/<harness>/`, `config/tls/`, `config/terrarium/`, `state/`, `tools/`, `work/`, `repos/`,
`projects/`, `agents/`, `targets/`, `subscriptions/`, and `fleets/`. Git
ignores `config/*`, `state/`, `tools/`, `work/*`, `repos/*`, and `fleets/*` (except the
README files). `work/` holds the agent workspaces, `<project>/<agent>/`.
A host agent on this machine attaches them to targets: its `work_dir` is
this `./work`.

## Commands

Run `just` to see the modules. Run `just --list <module>` to see the
recipes of a module.

| Task | Command |
|---|---|
| Build the image and start the stack | `just up` |
| Install or re-install on a server over ssh (builds the image there; [getting-started §2.14](docs/getting-started.md#214-install-on-a-server)) | `just deploy::server wil@terrarium.lan` |
| Update a server, roll it back, or show its deployed ref and health | `just deploy::update wil@terrarium.lan`, `just deploy::rollback ...`, `just deploy::status ...` |
| Install in another dir on this machine (not this repo) | `just deploy::local --dir /tmp/tt-deploy --port 18090` |
| Stop the stack | `just down` |
| Follow the container logs | `just docker::logs` |
| Open a shell in the container | `just docker::shell` |
| Edit the settings of one harness | `just docker::config-edit claude` |
| Build crates, dashboard, and image | `just build` |
| Run the Rust and dashboard tests | `just test` |
| Run the merge gate | `just ci::check` |
| Show, commit, or sync the config repo (prompts, harness settings, export; [getting-started §2.12](docs/getting-started.md#212-config-repo-share-your-prompts-and-settings-between-instances)) | `just daemon::cli admin config status`, `... config commit -m "..."`, `... config sync` |
| Back up the config repo and write the packaged defaults over it (development) | `just defaults::apply` |
| Run the compatibility suite for one harness | `just ci::compat claude` |
| Check or update a harness in the running container (installed into `./tools`, one version dir each; running agents keep their version; pins in `harnesses.toml`) | `just harness::check`, `just harness::latest`, `just harness::update claude` |
| Run the daemon outside Docker | `just daemon::run` |
| Run the CLI against a running daemon | `just daemon::cli daemon status` |
| Run the dashboard dev server | `just dashboard::dev` |
| Check bwrap in the image | `just spikes::bwrap-check` |
| Build the static host agent | `just host::build` |
| Make the TLS certificate for the host listener | `just host::cert terrarium.lan` |
| Install a host agent on the host (prints the command) | `just daemon::cli admin host new lab1 --enrol` |
| Install a host agent over ssh | `just host::install root@lab1 lab1` |
| Register a host agent and print its token | `just host::token lab1` |
| Make a webhook source for GitHub or Forgejo (prints the secret and the hook URL; [getting-started §2.11](docs/getting-started.md#211-send-ci-events-from-github-or-forgejo)) | `just daemon::cli event source add gh --kind github -p terrarium` |

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
| [docs/host-protocol.md](docs/host-protocol.md) | The wire contract between the daemon and `terrarium-host`, TLS, tokens, and install. |
| [docs/fleet.md](docs/fleet.md) | The fleet file: format, ownership, commands, and the auto-apply from the config dir. |
| [docs/security-review.md](docs/security-review.md) | The security review: findings F1 to F17 and their fixes, with the secret controls of the config repo (§12). |
| [docs/compat.md](docs/compat.md) | Verified facts about each harness, the usage probes, and the login flows, from the spikes. |
| [docs/compat-suite.md](docs/compat-suite.md) | The adapter compatibility suite: how to run it, its safety rules, and its steps. |

Other READMEs: [config/README.md](config/README.md) (harness homes and
login), [defaults/prompts/README.md](defaults/prompts/README.md) (prompt templates and personas),
[defaults/README.md](defaults/README.md) (the config repo),
[dashboard/src/README.md](dashboard/src/README.md) (the dashboard),
[spikes/README.md](spikes/README.md) (the spikes), and
[docker/seccomp-bwrap.README.md](docker/seccomp-bwrap.README.md) (the
seccomp profile).

## Safety model

- **Sandbox.** Each agent runs in its own `bwrap` sandbox inside the
  container. The harness sees its workspace (read-write), its `harness/`
  directory (read-only), the per-agent writable overlays (the few files
  that the harness writes itself), the shared harness home, and its own
  runner socket. It never sees `private/`, where the runner keeps the
  event log, the PTY log, the launch spec, and the exit record. It does
  not see other agents, the database, the daemon socket, the Docker
  socket, SSH keys, or the host file system (PLAN §11).
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
- **No secret in the config repo.** The prompts and the harness settings
  live in a git repo that instances share (`config/terrarium`, PLAN §8.3).
  Its `.gitignore` allows only the known paths and file types, a secret
  scan blocks each commit with a token, key, or password in it (also a
  commit by hand, through the `pre-commit` hook), and the remote URL and
  its credential stay in `state/`.
- **Roles and audit.** Each agent is a `member` by default. `project_admin`
  and `sysadmin` give more rights. The daemon identifies an agent by its
  runner socket, not by a header, and checks each call against the agent's
  roles and policy. The daemon records admin actions in the audit log.

## License

MIT (see `Cargo.toml`).

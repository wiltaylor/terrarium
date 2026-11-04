# Conventions

This file tells you who owns which code, how to write it, and how to run it.
The design is in [PLAN.md](PLAN.md).

## Crate ownership

| Path                          | Crate                       | Owns                                                                 |
|-------------------------------|-----------------------------|----------------------------------------------------------------------|
| `crates/terrarium-core`       | `terrarium-core` (lib)      | The domain model, events, hook payloads, the `Adapter` trait, the executor protocol, and the TOML definitions. No network and no database. |
| `crates/terrariumd`           | `terrariumd` (lib + bin)    | The daemon: the HTTP API on the Unix socket and TCP, SQLite and migrations, config loading, the supervisor, the mailbox, the board, executors and the host registry, usage accounting, the embedded dashboard. The integration tests use the lib. |
| `crates/terrarium-runner`     | `terrarium-runner` (lib + bin) | The agent runner: the PTY, the status machine, the runner socket, `events.jsonl`. The lib holds `Runner`, `RemoteRunner`, and `RunnerHandle`. The bin is the runner process, one per agent (feature `bin`, on by default). See [runner.md](runner.md). |
| `crates/terrarium`            | `terrarium` (bin)           | The CLI. Operator mode talks to the daemon. Agent mode talks to the runner socket. |
| `crates/terrarium-host`       | `terrarium-host` (lib + bin)| The host agent that runs on machines outside the container. The lib holds the wire protocol that the daemon uses. |
| `crates/terrarium-version`    | `terrarium-version` (lib)   | The build version of the binaries: `<crate version>+<git ref>[-dirty]`, the build time, and the stale check (see "Build version"). Only the bins and their libs depend on it. |
| `crates/terrarium-compat`     | `terrarium-compat` (lib)    | The adapter compatibility suite: real-harness scenarios against the pinned versions (`just ci::compat`, [compat-suite.md](compat-suite.md)). |
| `crates/adapters/<harness>`   | `terrarium-adapter-<harness>` (lib) | One adapter per harness: launch flags, hook mapping, session ID capture. |
| `dashboard/`                  | npm package                 | The SolidJS dashboard. The daemon embeds `dashboard/dist`.           |
| `docker/`, `compose.yaml`     |                             | The image, the entrypoint, the seccomp profile, and the stack.        |

Rules for dependencies:

- `terrarium-core` depends on no other Terrarium crate.
- Adapters depend only on `terrarium-core`.
- Bins depend on `terrarium-core` and on adapters. A bin never depends on
  another bin's binary at build time. `terrarium-host` is also a library: the
  daemon uses its `protocol` module for the host wire format, so it may
  depend on the lib. `terrariumd` depends on the `terrarium-runner` lib, and
  starts the `terrarium-runner` binary at run time (`TERRARIUM_RUNNER`).
- Put each shared dependency version in `[workspace.dependencies]` in the root
  `Cargo.toml`. A crate refers to it with `foo.workspace = true`.

## Code

- Format with `rustfmt` (default settings). `just ci::fmt-check` checks it.
- Keep clippy clean. `just ci::lint` runs clippy with `-D warnings` on all
  targets.
- Do not use `unwrap()` or `expect()` in daemon code paths. `terrariumd` warns
  on both. Return an error, or handle the case. Tests may use them.
- Libraries define their errors with `thiserror`. Binaries use `anyhow` and add
  context with `.context(...)`.
- Do not write `unsafe` code. The workspace forbids it.
- Log with `tracing`. Do not use `println!` in the daemon.
- Serde names are `snake_case` for enums and fields, except restart policies,
  which are `kebab-case` (`on-failure`), as in the plan.
- Never edit a database migration that has shipped. Add a new one to
  `crates/terrariumd/src/db.rs`.
- Test each TOML example from the plan with a round-trip test in
  `terrarium-core`.

## Harness versions

`harnesses.toml` at the repo root is the only place to change a harness
pin, the Node image, or the Copilot prompt extension pin.
Do not edit a version in `docker/Dockerfile` or in an adapter's `flags.rs`:

- The image installs no harness. It copies the file to
  `/etc/terrarium/harnesses.toml`, and `terrariumd harness install` (the
  entrypoint, and the daemon at each start) installs each missing pin into
  the tools dir. `just ci::check` fails when the Dockerfile installs a
  harness, or when its `NODE_IMAGE` default differs from `[node] image`.
- `crates/terrarium-core/build.rs` makes `terrarium_core::harnesses` from
  the file, and each adapter's `verified` comes from it.

Terrarium keeps no range of supported harness versions: the harnesses
release too often. Any installed version can start an agent, with no
warning. To test a new version, run the compatibility suite
(`just ci::compat`, [compat-suite.md](compat-suite.md)). The old setting
`TERRARIUM_HARNESS_VERSION_POLICY` has no effect.

**The tools dir.** The harnesses live in the tools dir, not in the image:
`TERRARIUM_TOOLS_DIR`, which is `/opt/terrarium/tools` in the container
(compose binds `./tools`) and `./tools` for `just daemon::run`. Each
version of each harness is its own npm prefix,
`versions/<name>/<version>/`, and does not change after it is complete.
`current/<name>` and `bin/<binary>` are symlinks to the active version;
an update switches them with an atomic `rename`. `bin/` is first on
`PATH`, and the sandbox binds the dir read-only. The manifest
`terrarium-manifest.json` records the active version and the version
dirs of each package. Only `crate::harness_tools` knows the layout:
other code finds a harness on `PATH`, or asks
`harness_tools::active_version`. An agent launch resolves the active
version dir and runs the harness from it, so an update never changes a
running agent (`sandbox::AgentLayout::harness_bin`).

Use `just harness::update <name>` (one harness) or `just harness::latest`
(every harness) to update in the running container, with no image build.
Running agents keep their version until their next restart. Add `--pin`
to also change `harnesses.toml`
([getting-started.md](getting-started.md), "Updating a harness").

## Run things with just

Run `just` to see the modules. Run `just <module>` to see its recipes.

| Task                                   | Command                         |
|----------------------------------------|---------------------------------|
| Run the merge bar                      | `just ci::check`                |
| Build all crates                       | `just daemon::build`            |
| Run the Rust tests                     | `just daemon::test`             |
| Run the daemon on this machine         | `just daemon::run`              |
| Run the CLI against a running daemon   | `just daemon::cli daemon status`|
| Build the dashboard                    | `just dashboard::build`         |
| Build the image                        | `just docker::image-build`      |
| Build the image and start the stack    | `just up`                       |
| Stop the stack                         | `just down`                     |
| Open a shell in the container          | `just docker::shell`            |
| Check bwrap in the image               | `just spikes::bwrap-check`      |
| Build the static host agent            | `just host::build`              |
| Compare the harness pins with npm      | `just harness::check`           |
| Update every harness to the latest     | `just harness::latest`          |
| Update one harness in the container    | `just harness::update <name>`   |
| Write the defaults manifest            | `just defaults::manifest`       |
| Back up and reset the config repo (dev) | `just defaults::apply`         |
| Install or re-install on a server      | `just deploy::server user@host` |
| Update or roll back a server           | `just deploy::update user@host`, `just deploy::rollback user@host` |
| Show the deployed ref and the health   | `just deploy::status user@host` |
| Install in another dir on this machine | `just deploy::local --dir <dir>` |

`just ci::check` must pass before a change merges. It runs `fmt-check`,
`lint`, `test`, `scripts-test`, and `dashboard-check`. `ci::compat` is not in the gate, because
it needs real logins.

## Install and release

The repo is private, so CI pushes no image to a registry. A release (a
`release: true` or `pre-release: true` trailer on `main`) makes a tag, a
GitHub release, and the `terrarium-host` asset only. A server builds its
own image from the source: the `deploy` module (`.just/deploy/mod.just`,
`scripts/deploy.py`, tests in `scripts/test_deploy.py`) sends the source
with `git archive` over ssh, builds the image on the target, and swaps the
code dirs only after a good build (PLAN §23.6,
[getting-started.md](getting-started.md) §2.14).

Rules for the deploy:

- The target keeps the code in `<dir>/src` and the data in `<dir>`.
  `compose.yaml` binds each data dir as `${TERRARIUM_DATA_DIR:-.}/<dir>`.
  Keep this form for a new bind mount, and add the dir to `DATA_DIRS` in
  `scripts/deploy.py` and to `docker::volumes-init` (a test compares the
  three lists).
- A deploy never changes an existing `.env`. Put a new setting in
  `.env.example` with a default in `compose.yaml`: the deploy then reports
  the key as missing, and the default applies.
- Never print a secret. The new `TERRARIUM_TOKEN` goes to the target on
  stdin, and the deploy shows it one time, at the end.
- Make each command on the target with `Target.argv()`, and each hint for
  the operator with `Target.shell_hint()` or `recipe_hint()`. Then
  `--ssh-option` and `--sudo-user` apply to all of them.

## Build version

The daemon, the CLI, the runner, and the host agent report one build
version: `<crate version>+<git ref>`, for example `0.1.0+6e31754`, with
`-dirty` when the build tree had changes to tracked files. `--version`
adds the build time. `GET /api/system/version` serves the daemon's
(docs/api.md §4).

- `crates/terrarium-version/build.rs` makes the version. The ref is
  `TERRARIUM_GIT_REF` when it is set, else it comes from git. Without
  either, the ref is `unknown` (`0.1.0+unknown`). The build time is
  `SOURCE_DATE_EPOCH` when it is set, else the time of the build.
- The build script runs again only when `TERRARIUM_GIT_REF` or
  `SOURCE_DATE_EPOCH` changes, or when git moves `HEAD`, the branch, or the
  index. So `-dirty` shows the tree at that time, not at each edit. Only
  the crates that depend on `terrarium-version` build again: keep
  `terrarium-core` and the adapters free of it.
- An image build has no `.git` (the deploy sends `git archive`). Give it the
  ref as the `TERRARIUM_GIT_REF` build arg: `scripts/build-ref.sh` for
  `just docker::*` and `just host::build`, `src/.deployed-ref` in the
  deploy wrapper (`terrarium-compose`), and `github.sha` in CI.
- A host agent whose commit or crate version differs from the daemon's is
  `stale` (`GET /api/hosts`, `admin host ls`, the Hosts tab). The `-dirty`
  mark does not count.

## Environment

`.env.example` documents every variable. `just up` makes `.env` from it, with
a new token, when `.env` does not exist. The daemon reads:

| Variable               | Default                                           |
|------------------------|---------------------------------------------------|
| `TERRARIUM_API_SOCKET` | `$XDG_RUNTIME_DIR/terrarium/api.sock`, else `/run/terrarium/api.sock` |
| `TERRARIUM_LISTEN`     | `0.0.0.0:8080`                                    |
| `TERRARIUM_TOKEN`      | none. Without a token, the TCP listener stays off. |
| `TERRARIUM_STATE_DIR`  | `/state`                                          |
| `TERRARIUM_CONFIG_DIR` | `/etc/terrarium`                                  |
| `TERRARIUM_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/terrarium`, else `/run/terrarium`. See "The runtime directory". |
| `TERRARIUM_RUNNER_MODE` | `process`: one `terrarium-runner` process per agent; the agents live through a daemon restart. `inprocess`: the runners run in the daemon and end with it (tests and development only). |
| `TERRARIUM_RUNNER`     | `terrarium-runner` next to `terrariumd`, else on `PATH`. The image sets `/usr/local/bin/terrarium-runner`. |
| `TERRARIUM_PROMPTS_DIR` | `<config repo>/prompts`: the prompt templates and the personas. |
| `TERRARIUM_CONFIG_REPO` | `/config/terrarium`. The config repo (PLAN §8.3): `prompts/`, `harness/<harness>/`, `export/`, in git. The daemon copies the packaged defaults into it at each start, and makes it a git repo. `just daemon::run` sets `target/dev-config-repo`. |
| `TERRARIUM_DEFAULTS_DIR` | `/opt/terrarium/defaults`: the packaged defaults and `manifest.json`. `just daemon::run` sets `<repo>/defaults`. A dir with no manifest: no seeding. |
| `TERRARIUM_CONFIG_SYNC` | `off`. `pull` or `sync`: the config repo pulls (or commits, pulls, and pushes) every `TERRARIUM_CONFIG_SYNC_INTERVAL` (default `15m`). |
| `TERRARIUM_SANDBOX`    | `bwrap`. `none` runs harnesses with no sandbox (development and tests only). |
| `TERRARIUM_SANDBOX_NEW_SESSION` | off. On: pass `--new-session` to bwrap (the harness then misses PTY resizes). |
| `TERRARIUM_REPOS_DIR` | `/repos`. A project repo whose URL is a path under it is bound read-write into the sandbox of each agent of that project (`docs/api.md` §5.1). |
| `TERRARIUM_WORK_DIR` | `/work`. The work root: the workspace of each agent is `<dir>/<project>/<agent>`. Compose binds `./work` here. A host agent on the same machine attaches the workspaces to targets: its `work_dir` is the host path of this dir. |
| `TERRARIUM_MAX_TARGETS_PER_PROJECT` | `8`. The most targets in one project. A create past it is `409 conflict`. |
| `TERRARIUM_MAX_SCHEDULES_PER_PROJECT` | `20`. The most schedules in one project (docs/api.md §19). A create past it is `409 conflict`. |
| `TERRARIUM_MAIL_LOOP_LIMIT` | `6`. The agent-to-agent messages that a thread may have after the last operator or project-inbox message. Then the loop guard pauses the thread (PLAN §12). |
| `TERRARIUM_WAIT_DEFAULT_TIMEOUT` | `24h`. The timeout of an `agent`, `process`, or `card` wait, and of a `terrarium run`, that gives none (PLAN §12d): seconds, or a number with `s`, `m`, `h`, or `d`. |
| `TERRARIUM_WAIT_MAX_TIMEOUT` | `7d`. The longest timeout of a wait or a run, and the longest `time` wait. A longer one is `422 invalid`. |
| `TERRARIUM_CARD_FILE_MAX_BYTES` | `10M`. The most bytes in one card file (docs/api.md §8.4): bytes, or a number with `K` or `M`. A bigger upload is `413 too_large`. |
| `TERRARIUM_CARD_FILES_MAX_BYTES` | `50M`. The most bytes in all the live versions of the files of one card. |
| `TERRARIUM_TRASH_DAYS` | `7`. The days that a deleted agent stays in `<state>/trash/`, with its workspace. |
| `TERRARIUM_SYSTEM_PROMPT_MAX` | `32K`. The most bytes in a system prompt: an agent's `system_prompt` and a persona file of `system-prompts/`, on every route, the agent start too. At most `96K` (Claude Code gets the prompt as one command line argument; Linux refuses one over 128 KiB). |
| `TERRARIUM_IDLE_STOP` | `10m`. An agent at rest (`idle` or `waiting`) this long stops by itself, with the status reason `idle`; work wakes it again (PLAN §18). A duration (`30m`, `1h30m`), or `off` (also `0`). The `idle_stop` of an agent wins over it. |
| `TERRARIUM_COMPACT_ON_IDLE` | off. On (`1`, `true`, `yes`, `on`): an agent compacts its session at its idle stop, before the stop; an agent with no idle stop compacts once after the same rest (PLAN §18, "Auto compact"). Each compaction is one model call. The `compact_on_idle` of an agent wins over it. |
| `TERRARIUM_COMPACT_MAX_AGE` | `off`. A duration (`24h`): an agent at rest whose session context is older compacts, then goes on (PLAN §18, "Auto compact"). The age counts from the last compaction, else from the start of the session. The `compact_max_age` of an agent wins over it. |
| `TERRARIUM_HARNESS_HOME` | `/auth/<harness>`. Override as `claude=/path,codex=/path`. |
| `TERRARIUM_TOOLS_DIR` | `tools` (from the cwd). The image sets `/opt/terrarium/tools`; `just daemon::run` sets `<repo>/tools`. The harnesses, one npm prefix for each version (`versions/<name>/<version>`): the daemon installs the pins with no active version there at start, puts `<dir>/bin` first on `PATH`, and the sandbox binds it read-only. Empty: no tools dir; the harnesses come from `PATH`, and `POST /api/harnesses/{name}/update` answers `501`. |
| `TERRARIUM_NO_HARNESS_INSTALL` | off. On (or `--no-harness-install`): the daemon does not install the missing pins at start. |
| `TERRARIUM_NPM` | `npm`. The npm program for the tools dir. |
| `TERRARIUM_HARNESS_INSTALL` | `1`. The entrypoint only: `0` skips its `terrariumd harness install`. |
| `TERRARIUM_CLI`        | `terrarium` next to `terrariumd`, else on `PATH`. The sandbox binds it at `/opt/terrarium/bin/terrarium`. |
| `TERRARIUM_ARTIFACT_LISTEN` | `0.0.0.0:8081`. Empty: no artifact listener of its own, and the dashboard port serves `/a/...`. With a listener, the dashboard port does not serve artifacts. |
| `TERRARIUM_HOST_LISTEN` | `auto`, the host agent dial-in: `0.0.0.0:8082` with TLS, `127.0.0.1:8082` without. Empty: no listener of its own; the main router still serves `/hosts/connect`. A plaintext bind to a non-loopback address logs an `INSECURE` warning. |
| `TERRARIUM_HOST_TLS_CERT`, `TERRARIUM_HOST_TLS_KEY` | unset. Both set: the host listener speaks TLS with this PEM certificate chain and key (`just host::cert`). One alone stops the daemon at start. |
| `TERRARIUM_DATA_DIR` | `.` (the project dir). compose.yaml only: the dir that holds the bind-mount sources (`config/`, `state/`, `tools/`, ...). `<dir>/terrarium-compose` of a deploy sets it to the install dir, so the data is not in `<dir>/src`. The daemon does not read it. |
| `TERRARIUM_PUBLIC_URL` | unset (mail links use `http://localhost:8080`). The dashboard as the operator opens it, for the links in operator mail, `Version.url`, the cookie `Origin` check on WebSocket upgrades, and CORS on `/a/...`. The host installer (`GET /install/host.sh`) downloads from it, and it and `just host::install` take the host agent's `daemon_url` from its host. |
| `TERRARIUM_HOST_DAEMON_URL` | unset: `wss://<host of TERRARIUM_PUBLIC_URL>:<port of the host listener>` when the host listener speaks TLS. The `daemon_url` that the host installer writes (`docs/api.md` §17.9). `just host::install` reads it too. |
| `TERRARIUM_HOST_DIST_DIR` | `/opt/terrarium/dist`. The static host agent binaries, `terrarium-host-<arch>-linux` and their `.sha256`, for `GET /install/terrarium-host`. The image fills it. For `just daemon::run`, run `just host::build` and set `target/host`. |
| `TERRARIUM_ARTIFACT_PUBLIC_URL` | unset: the host of the request with the port of `TERRARIUM_ARTIFACT_LISTEN`. The artifact origin as a browser opens it, for signed artifact URLs and the dashboard CSP. |
| `TERRARIUM_INGEST_ADDR` | unset: no ingest listener. The webhook listener of the event sources: only `POST /hooks/{project}/{source}` and `GET /healthz` ([api.md](api.md) §20). Compose sets `0.0.0.0:8083`. |
| `TERRARIUM_INGEST_PUBLIC_URL` | unset: made from `TERRARIUM_INGEST_ADDR`. The ingest listener as a forge reaches it, for the `hook_url` of each event source. |
| `TERRARIUM_PIPELINE_PARALLEL` | `4`. The most pipeline polls at once in the daemon ([api.md](api.md) §31.4). |
| `TERRARIUM_WEBHOOK_PRIVATE` | `allow`. Which addresses a `webhook` action can reach ([api.md](api.md) §19.5). `allow`: any address, the LAN and this host too (self-hosted receivers are common). `deny`: refuse loopback, link-local (the cloud metadata address too), private (RFC 1918, CGNAT), unique-local, and the ports of the daemon's own listeners. Set `deny` on a public deployment where untrusted people are project_admins. Another value is `deny`, with a warning. |
| `TERRARIUM_GIT_PROXY_LISTEN` | `127.0.0.1:8084`. The git proxy (PLAN §9, "The git proxy"; [api.md](api.md) §5.1): the agent checkouts of a proxied repo fetch and push through it, so `push_deny` is a hard rule and the forge token stays in the daemon. Keep it on loopback: the sandboxes share the daemon's network, and a non-loopback bind logs an `INSECURE` warning. Empty: no proxy, and every repo is `direct`. The key of the agent proxy credentials is `<state>/git-proxy.key` (mode 0600; remove it and restart to change every credential). |
| `TERRARIUM_WS_QUERY_TOKEN` | `0`: refuse the bearer token in `?token=` on a WebSocket upgrade (`401`). `1` (or `true`, `yes`, `on`): take it, with a warning in the log. Clients send the token as the subprotocol `bearer.<token>`. |
| `TERRARIUM_SESSION_TTL` | `30d`. The life of a dashboard session cookie: seconds, or a number with `s`, `m`, `h`, or `d`. The daemon renews the cookie past half its life. |
| `TERRARIUM_CLI_SESSION_TTL` | `90d`. The life of a CLI session token (`terrarium login`). |
| `TERRARIUM_SESSION_MAX_AGE` | `90d`. The longest life of a dashboard session through its renewals, from the login. |
| `TERRARIUM_PASSWORD`, `TERRARIUM_PASSWORD_FILE` | unset. The first operator password, set at the start when none is set. The daemon then starts again without both variables, so no child process inherits them. Remove them after the first start; use `terrariumd password set`. |
| `TERRARIUM_SSH` | `ssh`. The ssh binary for `ssh` targets. |
| `TERRARIUM_INSECURE_AGENT_HEADER` | off. On: trust `X-Terrarium-Agent` on the Unix socket (tests only). |
| `TERRARIUM_AUTO_SWAP` | `on`. When the provider refuses a turn with a usage-limit error, an agent with an account pool moves to the next account that did not hit a limit (PLAN §16.2, §16.3). A probe at 100 percent swaps nothing. `off` (or `0`, `false`, `no`): the agent stays, with a hold until the reset. |
| `TERRARIUM_QUESTION_GUARD` | `on`. A member whose turn ends with a question in plain text, and that reported nothing, becomes `blocked`, and its lead (or the operator) gets the question by mail (PLAN §8.4). `off` (or `0`, `false`, `no`): no check. |
| `TERRARIUM_HARNESS_AUTO_UPDATE` | `daily`. The background check for harness updates (PLAN §6, "Auto-update and pins"): for each harness with the policy `auto`, the daemon installs the latest release on npm (majors too) into a new version dir and switches to it. Running agents keep their version until their next start. `off`, or a duration of `1h` or more (`6h`). The first check is 5 minutes after the start, or one interval after the last check when that is later. A pin (`POST /api/harnesses/{name}/pin`) stops the update of one harness. |
| `TERRARIUM_HARNESS_UPDATE_MAIL` | `off`. An installed auto-update sends no mail; the audit row (`harness.auto_update`), the `harness` event, and the dashboard table (Settings → Subscriptions) show it. `on`: one Inbox mail for each installed update, with the rollback command. A failed install sends one mail for each failed version with both values. |
| `TERRARIUM_BACKUP_DIR` | `<state>/backups`. The backup zip files ([api.md](api.md) §32). The walk of a backup skips this dir. |
| `TERRARIUM_BACKUP_SCHEDULE` | `off`. `daily`, `weekly`, or a duration of `1h` or more: the daemon makes a backup one interval after the newest one ([api.md](api.md) §32.4). |
| `TERRARIUM_BACKUP_FULL_EVERY` | `7d`. A scheduled backup is full when the newest full backup is older than this; else it is a diff. |
| `TERRARIUM_BACKUP_KEEP` | `4`. The scheduled backup keeps this many full backups and their diffs, and deletes the older ones. |
| `TERRARIUM_BACKUP_WORK` | `off`. On: the scheduled backups have the workspaces too. |
| `TERRARIUM_BACKUP_PASSPHRASE_FILE` | unset. The first line of the file seals each scheduled backup. |

### The directories

Compose binds these directories of the repo into the container
(`just docker::volumes-init` makes them). Git ignores the ones that hold
data.

| Repo path | Container path | Content |
|---|---|---|
| `config/<harness>/` | `/auth/<harness>` | The shared harness homes: settings and caches. The plan logins are per account in `state/credentials/accounts/` (PLAN §16.3). Gitignored. |
| `config/tls/` | `/etc/terrarium/tls` (ro) | The certificate and key of the host listener. Gitignored. |
| `state/` | `/state` (`TERRARIUM_STATE_DIR`) | SQLite, the agent dirs `agents/<ulid>/`, the trash, artifacts, the card files (`card-files/<sha256>`), credentials (git credentials, stored API keys in `credentials/subscriptions/`, the plan accounts in `credentials/accounts/<subscription>/`, and the migration backups in `credentials/backup/`), the session key `session.key`, and the operator password hash `operator.password`. Gitignored. |
| `work/` | `/work` (`TERRARIUM_WORK_DIR`) | The agent workspaces, `<project>/<agent>/`. A host agent on this machine attaches them to targets (its `work_dir`). Gitignored. |
| `repos/` | `/repos` (`TERRARIUM_REPOS_DIR`) | Local bare repos, `<name>.git`, bound into the sandboxes of their project. A host agent reads them for a local `lab_repo` (its `repos_dir`). Gitignored. |
| `labs/` | not bound | vmlab test labs (`labs/terrarium-test/vmlab.wcl`) for a host agent on this machine. In git. |
| `config/terrarium/` | `/config/terrarium` (`TERRARIUM_CONFIG_REPO`) | The config repo (PLAN §8.3): the personas and protocol templates, the harness settings of the agents, the export. Its own git repo, shared between instances; never a secret. Gitignored in this repo. |
| `defaults/` | `/opt/terrarium/defaults` (in the image) | The packaged defaults of the config repo, with `manifest.json`. In git. After a change, run `just defaults::manifest`. |
| `projects/`, `agents/`, `targets/`, `subscriptions/` | `/etc/terrarium/...` | Definition files. |
| `fleets/` | `/etc/terrarium/fleets` | Fleet files. Gitignored, except `README.md`. |

A host agent has three directories of its own (`docs/host-protocol.md`,
"Host agent configuration"): `work_dir` (default `./work`), `repos_dir`
(default `./repos`), and `instances_dir` (default
`$XDG_DATA_HOME/terrarium-host/instances`, one directory for each lab
instance). A relative path is relative to the working directory of the host
agent. `max_running_targets` (default `4`) limits the docker and vmlab
targets that run on the host at one time (`429 rate_limited`).

### The runtime directory

`<run>` is `TERRARIUM_RUNTIME_DIR`. The files in it do not need to live
through a reboot.

| Path | Content |
|---|---|
| `<run>/api.sock` | The API socket, when `TERRARIUM_API_SOCKET` points here (the image does). |
| `<run>/agents/<ulid>.sock` | The runner socket of each agent: hooks and agent RPC (mode `0600`). |
| `<run>/runners/<ulid>.sock` | The control socket of each runner process, for the daemon (mode `0600`). |
| `<run>/runners/<ulid>.pid` | The pid of each runner process. The live runner holds a `flock` on it; the daemon reads it at start to find the runners that still run. |

### The agent directory

Each agent has `<state>/agents/<ulid>/` with `private/` and `harness/`,
and a workspace under the work root ([runner.md](runner.md), "Files";
PLAN §11). Keep each file in the right part. A file that the harness must
not change never goes in `harness/` or the workspace.

| Directory | Content | May write | In the sandbox |
|---|---|---|---|
| `private/` | `events.jsonl`, `pty.log`, `pty.log.1`, `runner.json` (the launch spec with the injected keys), `runner-argv.json`, `runner-exit.json`, `runner.log`, `session-prompt.sha256`, `harness-version.json` (the harness version of the last launch), `accounts.json` (the plan account of the last launch), `skills.json` (the skills of the last launch and their hash, PLAN §8.6). Mode `0700`. | The daemon and the runner only. | Not bound. The harness cannot read or change it. |
| `harness/` | `prompt.md`, `skills/` (a copy of the agent's skills at each start, PLAN §8.6), the per-agent files of each adapter (`claude/`, `codex/`, `copilot/`, `pi/`), and `cred/account`: the symlink to the store of the agent's plan account, which the daemon swaps (PLAN §16.3). | The daemon (through the adapter) when it builds the launch. The harness writes only the files that an adapter declares as a `FileOverlay` with `writable: true`. | Read-only at its own path. Each writable overlay has its own read-write bind. |
| `<work>/<project>/<agent>/` | The agent's files and repository checkouts, and `.terrarium/`: the turn attachments and the card files (`.terrarium/cards/<card>/`, docs/api.md §8.4) that the daemon writes, fd-relative, with no symlink followed. An agent dir of the older layout has `workspace/`; the next start moves it here once. | The harness, the daemon for clones, and a target that has the workspace attached. | Read-write at `$HOME/work` and at `/work/<project>/<agent>`. A host agent attaches it to the agent's targets at `/work/<project>/<agent>`. `agent rm` moves it to `<state>/trash/` with the agent dir. |

Use `terrarium_runner::{private_dir, harness_dir}` and
`terrariumd::workspace::dir` for the paths. A daemon start moves an agent directory of the older flat layout
into `private/` and `harness/` once (`migrate_legacy_layout`). The test
`the_sandbox_never_binds_the_private_dir` in `sandbox.rs` checks that no
bind names `private/`.

The CLI uses the Unix socket. Set `TERRARIUM_URL` and `TERRARIUM_TOKEN` to use
TCP instead. The CLI takes `https://` (rustls, the system roots, and
`TERRARIUM_CA_FILE`) and `http://`. `terrarium login` refuses a plain
`http://` URL to another host unless `--insecure-http` is given.

The TCP listener asks for the bearer token on `/api/*` only. The dashboard
files are public, because a browser cannot send a header when it loads a page.
A WebSocket upgrade sends the token as the subprotocol `bearer.<token>`
(`Sec-WebSocket-Protocol`), not in the URL ([api.md](api.md) §1).
The dashboard uses a session cookie instead: `POST /api/session` trades the
operator password (or the token) for a signed `HttpOnly` cookie and a CSRF
value ([api.md](api.md) §1.1). `terrarium login` trades the password for a
CLI session token, which the CLI keeps per URL in
`$XDG_CONFIG_HOME/terrarium/credentials.json`. `TERRARIUM_TOKEN` wins over
it.

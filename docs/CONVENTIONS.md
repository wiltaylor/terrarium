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

`just ci::check` must pass before a change merges. It runs `fmt-check`,
`lint`, `test`, and `dashboard-check`. `ci::compat` is not in the gate, because
it needs real logins.

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
| `TERRARIUM_PROMPTS_DIR` | `/prompts`                                       |
| `TERRARIUM_SANDBOX`    | `bwrap`. `none` runs harnesses with no sandbox (development and tests only). |
| `TERRARIUM_SANDBOX_NEW_SESSION` | off. On: pass `--new-session` to bwrap (the harness then misses PTY resizes). |
| `TERRARIUM_HARNESS_HOME` | `/auth/<harness>`. Override as `claude=/path,codex=/path`. |
| `TERRARIUM_CLI`        | `terrarium` next to `terrariumd`, else on `PATH`. The sandbox binds it at `/opt/terrarium/bin/terrarium`. |
| `TERRARIUM_ARTIFACT_LISTEN` | `0.0.0.0:8081`. Empty: no artifact listener of its own. |
| `TERRARIUM_INSECURE_AGENT_HEADER` | off. On: trust `X-Terrarium-Agent` on the Unix socket (tests only). |

### The runtime directory

`<run>` is `TERRARIUM_RUNTIME_DIR`. The files in it do not need to live
through a reboot.

| Path | Content |
|---|---|
| `<run>/api.sock` | The API socket, when `TERRARIUM_API_SOCKET` points here (the image does). |
| `<run>/agents/<ulid>.sock` | The runner socket of each agent: hooks and agent RPC (mode `0600`). |
| `<run>/runners/<ulid>.sock` | The control socket of each runner process, for the daemon (mode `0600`). |
| `<run>/runners/<ulid>.pid` | The pid of each runner process. The live runner holds a `flock` on it; the daemon reads it at start to find the runners that still run. |

A runner process also writes `runner.json`, `runner.log`, and
`runner-exit.json` in `<state>/agents/<ulid>/` ([runner.md](runner.md)).

The CLI uses the Unix socket. Set `TERRARIUM_URL` and `TERRARIUM_TOKEN` to use
TCP instead. The CLI supports `http://` only.

The TCP listener asks for the bearer token on `/api/*` only. The dashboard
files are public, because a browser cannot send a header when it loads a page.

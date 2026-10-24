# Getting started

This guide tells you how to run Terrarium in Docker, how to run it on your
machine for development, how to run the tests, and how to fix common
problems. Terrarium is early work. Only the Claude Code adapter works today
(see the status in [../README.md](../README.md#status)).

Run every command from the root of the repository.

## 1. Prerequisites

To run Terrarium in Docker:

- Docker Engine with the compose plugin (`docker compose version`).
- [just](https://github.com/casey/just).
- A Claude subscription (Pro or Max), or another login that `claude auth
  login` accepts.

To develop Terrarium outside Docker, you also need:

- A stable Rust toolchain from rustup. The code uses `let` chains, so use
  Rust 1.88 or later. The image builds with Rust 1.97.
- Node 22. The dashboard alone needs Node 20.19 or later. The image uses
  Node 22, because Claude Code and pi need it.
- [uv](https://docs.astral.sh/uv/), for the spike scripts.
- `bwrap` (bubblewrap) with unprivileged user namespaces, for the sandbox.
- Claude Code on your `PATH`, for a real agent.

## 2. First run with Docker

### 2.1 Set the token

1. Copy the example environment file:

   ```sh
   cp .env.example .env
   ```

2. Make a random token:

   ```sh
   openssl rand -hex 32
   ```

3. Open `.env` and set `TERRARIUM_TOKEN` to that value. Do not keep
   `change-me`.

If `.env` does not exist, `just up` makes it from `.env.example` with a new
random token. Steps 1 to 3 are then not necessary.

### 2.2 Start the stack

```sh
just up
```

`just up` builds the image (`just docker::image-build`), makes the bind-mount
directories (`just docker::volumes-init`), and starts the stack. It waits
until the health check passes. The first build takes some minutes.

To see the logs, run `just docker::logs`. To stop the stack, run `just down`.
The bind-mounted data stays.

### 2.3 Open the dashboard

Open <http://localhost:8080>. If you changed `TERRARIUM_PORT` in `.env`, use
that port. The dashboard shows a token screen. Paste the value of
`TERRARIUM_TOKEN`.

### 2.4 Log in to Claude

All agents share one Claude home, so you log in once:

```sh
docker compose exec terrarium claude auth login
```

The command prints a URL. Open the URL in a browser, then paste the code
back into the terminal. To check the login, run:

```sh
docker compose exec terrarium claude auth status
```

The command exits with 0 when you are logged in.

The dashboard has a Subscriptions page for logins (PLAN §16.1). The daemon
does not serve it yet, so the page shows "Not available yet". Use the shell
command.

### 2.5 Make a project and an agent

The CLI is in the container. Make a shell function so that the commands are
short:

```sh
t() { docker compose exec terrarium terrarium "$@"; }
```

Check the daemon:

```sh
t daemon status
```

Make a project and a Claude agent in it:

```sh
t project new demo -d "My first project"
t -p demo agent new worker --harness claude --model haiku
t agent start demo/worker
```

`--model` is optional. `haiku` keeps each turn cheap.

Give the agent a turn, and look at its status:

```sh
t agent send demo/worker "Make a file hello.txt that says hello."
t -p demo agent ls
t agent logs demo/worker
```

Send mail to the agent, and read the answer:

```sh
t mail send demo/worker -s "first task" "Tell me what is in your workspace."
t mail ls --to operator --unread
```

You can also do these steps in the dashboard. On the Projects page, click
**New project**. On the Agents page, click **New agent**. Then open the
agent and click **Start**.

To use the CLI from the host over TCP, build it and set `TERRARIUM_URL`.
`just` reads `TERRARIUM_TOKEN` from `.env`:

```sh
TERRARIUM_URL=http://localhost:8080 just daemon::cli -p demo agent ls
```

### 2.6 Watch the agent

Open the agent in the dashboard and go to the **Terminal** tab. The terminal
shows the real Claude Code TUI. It is read-only until you select the input
check box.

To attach from a shell, run:

```sh
docker compose exec terrarium terrarium agent attach demo/worker
```

Press Ctrl-] to detach. The agent continues to run.

## 3. Configuration directories

Compose bind-mounts these host directories into the container:

| Host directory | Container path | Content |
|---|---|---|
| `config/claude/` | `/auth/claude` | The Claude Code home (`CLAUDE_CONFIG_DIR`): login, sessions, `.claude.json`. |
| `config/codex/` | `/auth/codex` | The Codex home (`CODEX_HOME`). |
| `config/copilot/` | `/auth/copilot` | The Copilot CLI home (`COPILOT_HOME`) and the system-prompt extension. |
| `config/pi/` | `/auth/pi` | The pi home (`PI_CODING_AGENT_DIR`). |
| `state/` | `/state` | SQLite, agent workspaces and logs, artifacts, credentials. |
| `prompts/` | `/prompts` (read-only) | System prompt templates. |
| `projects/`, `agents/`, `targets/`, `subscriptions/` | `/etc/terrarium/...` | Definition files. |

These directories contain credentials. Do not commit them.

To edit the settings of one harness from the host, run:

```sh
just docker::config-edit claude
```

The recipe opens `config/claude/` in `$EDITOR`. The argument is `claude`,
`codex`, `copilot`, or `pi`. You can also open the files with any editor.

Claude Code agents start with `--setting-sources ""`. They do not read
`config/claude/settings.json` or `config/claude/CLAUDE.md`. Those files
apply only when you start `claude` yourself in the container. Terrarium
gives each agent its hooks and quiet settings with `--settings`.

After you change a definition file, tell the daemon to read the files again:

```sh
t daemon reload
```

If the entrypoint says that a directory "is not writable", run
`just docker::volumes-init`. Then make sure that `TERRARIUM_UID` and
`TERRARIUM_GID` are the owner of `config/` and `state/`. See
[../config/README.md](../config/README.md).

## 4. Development loop outside Docker

### 4.1 Run the daemon

`just daemon::run` runs `terrariumd` with `cargo run`. It sets these values:

| Variable | Value |
|---|---|
| `TERRARIUM_LISTEN` | `127.0.0.1:8090` (change it with `TERRARIUM_DEV_LISTEN`) |
| `TERRARIUM_STATE_DIR` | `target/dev-state` (change it with `TERRARIUM_DEV_STATE_DIR`) |
| `TERRARIUM_CONFIG_DIR` | `.` (the definition directories in the repository root) |

It does not set the harness home or the prompts directory. Their defaults
(`/auth/<harness>` and `/prompts`) are container paths. Set them yourself.

1. Build the CLI. The daemon binds `target/debug/terrarium` into each
   sandbox for the hooks:

   ```sh
   just daemon::build
   ```

2. Start the daemon with your Claude home and the prompts directory:

   ```sh
   TERRARIUM_HARNESS_HOME=claude=$HOME/.claude \
   TERRARIUM_PROMPTS_DIR=prompts \
   TERRARIUM_DEV_STATE_DIR=$PWD/target/dev-state \
     just daemon::run
   ```

3. In a second shell, use the CLI:

   ```sh
   just daemon::cli daemon status
   just daemon::cli project new demo
   just daemon::cli -p demo agent new worker --harness claude --model haiku
   just daemon::cli agent start demo/worker
   ```

The CLI finds the daemon through the Unix socket
(`$XDG_RUNTIME_DIR/terrarium/api.sock`). It needs no token there.

Agents write to the shared Claude home: `history.jsonl`, `projects/`,
`backups/`, and other files. To keep them out of your own `~/.claude`, make
a separate directory, log in to it with `CLAUDE_CONFIG_DIR=<dir> claude auth
login`, and give that directory in `TERRARIUM_HARNESS_HOME`.

The daemon also opens the artifact origin on `0.0.0.0:8081`. If the Docker
stack runs at the same time, the port is in use. Set
`TERRARIUM_ARTIFACT_LISTEN=127.0.0.1:8091`.

### 4.2 Run without the sandbox

If your machine cannot run `bwrap`, add `TERRARIUM_SANDBOX=none`. The
harness then runs in yolo mode with full access to your user account. Use it
for development and tests only. See also section 7.4.

### 4.3 Run the dashboard

Install the dependencies once:

```sh
just dashboard::install
```

Run the dev server against the local daemon:

```sh
TERRARIUM_DAEMON_URL=http://127.0.0.1:8090 just dashboard::dev
```

Open <http://localhost:5173>. Vite sends `/api` and `/a/` to the daemon.
Without `TERRARIUM_DAEMON_URL`, Vite uses `http://localhost:8080`, which is
the Docker stack. The token screen needs the `TERRARIUM_TOKEN` of that
daemon. `just daemon::run` reads it from `.env`. Without a token, the daemon
does not open its TCP listener.

To work on the dashboard without a daemon, use the mock API:

```sh
VITE_MOCK=1 just dashboard::dev
```

A `mock` pill shows next to the logo. See
[../dashboard/src/README.md](../dashboard/src/README.md).

To put a new dashboard build into the daemon, run `just dashboard::build`,
then build the daemon again. Without `dashboard/dist`, the daemon serves a
placeholder page.

## 5. Run the gate

```sh
just ci::check
```

The gate runs `ci::fmt-check` (rustfmt), `ci::lint` (clippy with
`-D warnings`), `ci::test` (`cargo test --workspace`), and
`ci::dashboard-check` (TypeScript and ESLint). A change must pass it before
it merges.

The gate does not run the dashboard unit tests. Run them with
`just dashboard::test`, or run `just test` for the Rust tests and the
dashboard tests together.

## 6. Run the real Claude test

`crates/terrariumd/tests/e2e_claude.rs` runs one real Claude Code agent
inside `bwrap`, through the whole daemon. Cargo ignores it by default,
because it needs a logged-in Claude home and spends model turns. It is not
in `just ci::check`.

```sh
cargo build -p terrarium
TERRARIUM_E2E_CLAUDE_HOME=$HOME/.claude \
  cargo test -p terrariumd --test e2e_claude -- --ignored --nocapture
```

The test sends one model turn, on `haiku`. It binds the Claude home
read-write, as the container does. It does not copy or change the
credentials. Set `TERRARIUM_E2E_CLI` to use a CLI other than
`target/debug/terrarium`. The results are in
[compat.md](compat.md), "Claude inside bwrap".

## 7. Troubleshooting

### 7.1 bwrap cannot make a sandbox

The daemon checks `bwrap` when it starts. When the check fails, the daemon
stops with "bwrap cannot make a sandbox here".

In Docker, `bwrap` needs three security options. `compose.yaml` sets them:

- `seccomp:docker/seccomp-bwrap.json`: allows `clone`, `clone3`,
  `unshare`, `mount`, `umount2`, and `pivot_root`.
- `apparmor:unconfined`.
- `systempaths=unconfined`: without it, `bwrap --proc` fails with
  "Can't mount proc".

`SYS_ADMIN` does not help. If you start the container without compose, give
the same options. To test them, run:

```sh
just spikes::bwrap-check
```

It prints `bwrap ok` when the sandbox works. See
[compat.md](compat.md), "bwrap inside Docker", and
[../docker/seccomp-bwrap.README.md](../docker/seccomp-bwrap.README.md).

Outside Docker, the host must allow unprivileged user namespaces. If it does
not, use `TERRARIUM_SANDBOX=none` for development.

### 7.2 Claude Code refuses yolo as root

Claude Code exits with "`--dangerously-skip-permissions` cannot be used with
root/sudo privileges". The image runs as the `terrarium` user, so this does
not occur in a normal stack. The adapter sets `IS_SANDBOX=1` when the
harness runs as root. That removes the refusal for agents.

If you see the message:

- Do not run `docker compose exec -u root terrarium claude ...`. Use the
  default user.
- Do not run `just daemon::run` with `sudo`.
- Check the user in the container with
  `docker compose exec terrarium id -u`.

### 7.3 "Session ID ... is already in use"

Claude Code exits at once with `Error: Session ID <uuid> is already in
use.` when `--session-id` gets a UUID that it saw before. The daemon makes a
new UUID for each fresh start and uses `--resume` for a known session. Thus
the daemon does not cause this error.

The usual cause is a launch command that you ran by hand two times, for
example the output of `terrariumd sandbox-dry-run <agent>`. Use
`--resume <uuid>` to continue that session, or start the agent through the
daemon with `terrarium agent start`.

### 7.4 The trust dialog in sandbox mode `none`

In mode `bwrap`, the sandbox binds a per-agent `.claude.json` over the
shared one. That file marks the workspace as trusted and onboarding as
done. Mode `none` cannot bind files. The daemon logs "sandbox mode `none`
cannot bind ... per-agent file(s)", and Claude Code reads the shared
`<home>/.claude.json`. It can then show the theme picker or the "Yes, I
trust this folder" dialog. In the trust dialog, the default choice is
"No, exit". An Enter typed into it stops the agent.

To fix it, mark the agents directory as trusted in the shared file. Trust on
a parent directory covers the workspaces under it. This example uses `jq`:

```sh
home=$HOME/.claude                         # the dir in TERRARIUM_HARNESS_HOME
agents=$PWD/target/dev-state/agents        # <state dir>/agents
f=$home/.claude.json
[ -f "$f" ] || echo '{}' > "$f"
jq --arg d "$agents" \
  '.hasCompletedOnboarding = true | .projects[$d].hasTrustDialogAccepted = true' \
  "$f" > "$f.tmp" && mv "$f.tmp" "$f"
```

You can also accept the dialog by hand. Open the agent's terminal in the
dashboard, select the input check box, and select "Yes, I trust this
folder".

### 7.5 Other messages

| Message | Fix |
|---|---|
| `set TERRARIUM_TOKEN in .env` (from compose) | Set `TERRARIUM_TOKEN` in `.env` (section 2.1). |
| `the claude home /auth/claude does not exist` | Outside Docker, set `TERRARIUM_HARNESS_HOME=claude=<dir>` (section 4.1). In Docker, log in first (section 2.4). |
| `cannot read the prompt template` | The agent names a template that is not in the prompts directory. Remove `--prompt`, or add `prompts/<name>.md`. |
| `no terrarium CLI found` (daemon log) | Run `just daemon::build`, or set `TERRARIUM_CLI`. |
| `awaiting server: this daemon does not serve this endpoint yet` | The CLI has the command, but the daemon does not have the route yet. |
| `... is not implemented yet` (from `just`) | The recipe belongs to a later phase. |

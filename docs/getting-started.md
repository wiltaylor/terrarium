# Getting started

This guide tells you how to run Terrarium in Docker, how to run it on your
machine for development, how to run the tests, and how to fix common
problems. Terrarium is early work. All four adapters exist, but only the
Claude Code adapter is verified end to end (see the status in
[../README.md](../README.md#status)).

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
- `bwrap` (bubblewrap) with unprivileged user namespaces, if you want the
  sandbox. The dev loop runs without it by default (section 4.2).
- Claude Code on your `PATH`, for a real agent. For the other adapters,
  `codex`, `copilot`, or `pi` on your `PATH`.

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

At sign-in, the dashboard trades the token for a session cookie
(`POST /api/session`) and then drops the token. The cookie is `HttpOnly`,
so no script on the page can read it. The dashboard keeps only the CSRF
value, in memory and in the `sessionStorage` of the tab. A reload keeps the
session. A new tab asks for the token again. If you select **Remember on this
device**, the dashboard also puts the CSRF value in `localStorage`. Then a
new tab stays signed in until the session ends. The CSRF value is not a
credential without the cookie. The session ends after
`TERRARIUM_SESSION_TTL` (default `12h`). **sign out** in the sidebar ends the
session and removes every copy
([../dashboard/src/README.md](../dashboard/src/README.md)).

Set `TERRARIUM_PUBLIC_URL` in `.env` to the dashboard URL as you open it.
The daemon puts this URL in the links of operator mail (the default is
`http://localhost:8080`).

### 2.4 Log in to Claude

All agents share one Claude home, so you log in once. Use the dashboard, or
use a shell in the container.

To log in from the dashboard:

1. Open the **Subscriptions** page.
2. Click **Add subscription**, then click **Claude Code**.
3. Click the login URL that the wizard shows, and sign in.
4. Paste the code from the browser into the code field, and click
   **Send code**.

The wizard runs `claude auth login` in an embedded terminal, outside the
sandbox. When the login is done, the daemon makes the subscription,
verifies the login, and runs the quota probe one time. The **Verify now**
button checks the login again. The wizard has the same flow for Codex and
Copilot (a device code), and for pi. To store an API key, click **API
key**.

To log in from a shell:

```sh
docker compose exec terrarium claude auth login
```

The command prints a URL. Open the URL in a browser, then paste the code
back into the terminal. To check the login, run:

```sh
docker compose exec terrarium claude auth status
```

The command exits with 0 when you are logged in. The Subscriptions page
finds the login and offers to make a subscription for it.

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

### 2.7 Connect a host agent

A host agent (`terrarium-host`) runs on a machine outside the container. It
dials in to the daemon on port 8082 and runs the commands of `host`,
`docker`, and `vmlab` targets (PLAN §19,
[host-protocol.md](host-protocol.md)).

The default host listener is plaintext on `127.0.0.1:8082` in the
container. The published port cannot reach it, so a remote host agent
cannot connect. This default keeps the host tokens and the exec traffic off
the network in clear text. For a remote host agent, turn on TLS:

1. Make a certificate for the name or the address that the host agents
   dial:

   ```sh
   just host::cert terrarium.lan
   ```

   The recipe writes `config/tls/host-cert.pem` and
   `config/tls/host-key.pem`, and prints the SHA-256 of the certificate.

2. Set the two paths in `.env`. Compose mounts `config/tls` at
   `/etc/terrarium/tls`:

   ```sh
   TERRARIUM_HOST_TLS_CERT=/etc/terrarium/tls/host-cert.pem
   TERRARIUM_HOST_TLS_KEY=/etc/terrarium/tls/host-key.pem
   ```

3. Start the stack again with `just up`. The daemon now speaks TLS on
   `0.0.0.0:8082`. The start log shows the SHA-256 of the certificate.

4. Install the host agent on the remote machine over ssh:

   ```sh
   just host::install root@lab1.lan lab1
   ```

   The ssh user must be `root`, or must have `sudo` with no password
   prompt. The recipe builds `terrarium-host`, copies the binary and the
   systemd unit, and writes a config with `daemon_url` and the certificate
   pin. Then it gets a one-time enrol token from the daemon, writes it to
   the token file on the host, starts the service, and probes the link.
   Add `--dry-run` to see each command first. Nothing runs then.

The recipe finds `daemon_url` in `--daemon-url`, then in
`TERRARIUM_HOST_DAEMON_URL`, then in the host of `TERRARIUM_PUBLIC_URL`
(port 8082). A `localhost` URL stops the install, because a remote machine
cannot dial it.

The enrol token works one time, for 10 minutes. At its first connect, the
host agent swaps it for a long-lived token and writes that token to its
token file. So the long-lived token never goes through your clipboard. To
do the steps by hand, get an enrol token with `admin host new <name>
--enrol`, and give a host a new token with `admin host rotate <name>`. See
[cli.md](cli.md) for the exact commands, and
[host-protocol.md](host-protocol.md), "Tokens" and "Install", for each
step.

A host agent on the Docker host itself can stay on plaintext. Set
`TERRARIUM_HOST_LISTEN=0.0.0.0:8082` and `TERRARIUM_HOST_PORT=127.0.0.1:8082`
in `.env`, so that only the local machine can reach the port. The daemon
logs a warning that starts with `INSECURE` for a plaintext bind to a
non-loopback address. The warning is correct here, because the port is
published on loopback only.

### 2.8 Declare a fleet

A fleet file declares projects, repos, agents, targets, and subscriptions
in one TOML file ([fleet.md](fleet.md)). The example is
[`examples/fleet/terrarium.fleet.toml`](../examples/fleet/terrarium.fleet.toml).

1. Copy the example to `fleets/` and edit it:

   ```sh
   cp examples/fleet/terrarium.fleet.toml fleets/terrarium.toml
   ```

2. Check the file and see the changes. The `t` function of section 2.5
   runs the CLI in the container, so give the path in the container:

   ```sh
   t fleet validate /etc/terrarium/fleets/terrarium.toml
   t fleet plan /etc/terrarium/fleets/terrarium.toml
   ```

3. Apply it:

   ```sh
   t fleet apply /etc/terrarium/fleets/terrarium.toml
   ```

Compose mounts `./fleets` at `/etc/terrarium/fleets`. The daemon applies
each `*.toml` file there at start and on `t daemon reload`. This
auto-apply does not prune, does not start agents, and does not clone
repos. Use `t fleet apply --prune` and `--start` for that.

### 2.9 Write your own API client

A script uses the bearer token. Send `Authorization: Bearer <token>` on
each request to `/api/*`. A browser cannot set this header on a WebSocket.
So a WebSocket client sends the token as a subprotocol:
`Sec-WebSocket-Protocol: bearer.<token>`. In a browser, that is
`new WebSocket(url, ["bearer." + token])`. The daemon echoes the same
subprotocol in its `101` answer.

A browser client uses a cookie session, as the dashboard does:

1. Send `POST /api/session` with `Authorization: Bearer <token>`. The daemon
   sets the `terrarium_session` cookie and answers
   `{"csrf", "exp", "expires_at"}`.
2. Drop the token. Keep the `csrf` value.
3. Send `X-CSRF-Token: <csrf>` on each `POST`, `PUT`, `PATCH`, and `DELETE`.
   Without it, the daemon answers `403`.
4. Open a WebSocket with no subprotocol. The browser sends the cookie and
   the page's `Origin`. The daemon refuses an `Origin` that is not the
   dashboard origin.
5. On a `401`, send `POST /api/session` again. `DELETE /api/session`
   ends the session.

Do not put the token in the `?token=` query string. A query string goes into
proxy logs and browser history. The daemon refuses that form by default
(`401`). `TERRARIUM_WS_QUERY_TOKEN=1` turns it back on for an old client,
with a warning in the log. See [api.md](api.md), section 1.

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
| `fleets/` | `/etc/terrarium/fleets` | Fleet files (section 2.8). |
| `repos/` | `/repos` | Local bare git repos that agents push to (see "Local repos" below). |
| `config/tls/` | `/etc/terrarium/tls` (read-only) | The certificate and key of the host listener (section 2.7). |

These directories contain credentials. Do not commit them.

**Local repos.** To give a project a git remote with no credential, put a
bare repo in `repos/`, for example with `git init --bare -b main
repos/app.git`. Then attach it as a sysadmin or the operator:

```sh
t -p demo project repo-add file:///repos/app.git --name app --branch main
```

At each start, the sandbox of each agent of `demo` binds `/repos/app.git`
read-write at the same path. So `git push origin <branch>` from the
workspace works. The agents of other projects do not see the repo. The
daemon binds only paths under `TERRARIUM_REPOS_DIR` (default `/repos`). The
agents can change every file of the bare repo, so treat `repos/` as data
that agents write.

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

`just daemon::run` builds `terrarium-runner`, then runs `terrariumd` with
`cargo run`. It sets these values. Each value gives way to the variable
when you set it:

| Variable | Value |
|---|---|
| `TERRARIUM_LISTEN` | `127.0.0.1:8090` (change it with `TERRARIUM_DEV_LISTEN`) |
| `TERRARIUM_STATE_DIR` | `<repo>/target/dev-state` (change it with `TERRARIUM_DEV_STATE_DIR`) |
| `TERRARIUM_CONFIG_DIR` | `<repo>` (the definition directories in the repository root) |
| `TERRARIUM_SANDBOX` | `none` (no sandbox; see section 4.2) |
| `TERRARIUM_PROMPTS_DIR` | `<repo>/prompts` |
| `TERRARIUM_ARTIFACT_LISTEN` | `127.0.0.1:8091`, so that it does not collide with the Docker stack on 8081 |
| `TERRARIUM_HARNESS_HOME` | `claude=$HOME/.claude,codex=$HOME/.codex,copilot=$HOME/.copilot,pi=$HOME/.pi/agent` |

To see the full command, run `just --dry-run daemon::run`.

The daemon runs each agent in its own `terrarium-runner` process
(`TERRARIUM_RUNNER_MODE=process`, the default). The agents live through a
daemon restart: when the daemon starts again, it connects to each runner
that still runs. The daemon finds `terrarium-runner` next to `terrariumd`
(`target/debug/`), else on `PATH`. Set `TERRARIUM_RUNNER` to use another
binary. Set `TERRARIUM_RUNNER_MODE=inprocess` to run the runners in the
daemon. Then the agents stop with the daemon.

1. Build the CLI once. The agents use `target/debug/terrarium` for the
   hooks and the agent commands:

   ```sh
   just daemon::build
   ```

2. Start the daemon:

   ```sh
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

The agents use your own harness logins under `$HOME`, and they write to
those homes. Claude agents write `history.jsonl`, `projects/`, `backups/`,
and other files to `~/.claude`. To keep them out of your own home, make a
separate directory, and log in to it with `CLAUDE_CONFIG_DIR=<dir> claude
auth login`. Then give that directory in `TERRARIUM_HARNESS_HOME`, for
example `TERRARIUM_HARNESS_HOME=claude=<dir> just daemon::run`. When you
set `TERRARIUM_HARNESS_HOME`, it replaces the whole list.

To use the dashboard login wizard (section 2.4) against this daemon, open
the dev dashboard (section 4.3). The wizard writes the login to the
harness home in `TERRARIUM_HARNESS_HOME`. With the default homes, a new
login replaces your own login in that home.

### 4.2 Run with the sandbox

`just daemon::run` uses `TERRARIUM_SANDBOX=none`. The harness then runs in
yolo mode with full access to your user account. Use it for development
and tests only. See also section 7.4.

If your machine can run `bwrap`, turn the sandbox on:

```sh
TERRARIUM_SANDBOX=bwrap just daemon::run
```

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

## 6. Run the real-harness tests

These tests start real harnesses and spend model turns. They are not in
`just ci::check`.

### 6.1 The real Claude test

`crates/terrariumd/tests/e2e_claude.rs` runs one real Claude Code agent
inside `bwrap`, through the whole daemon. Cargo ignores it by default,
because it needs a logged-in Claude home and spends model turns.

```sh
cargo build -p terrarium -p terrarium-runner
TERRARIUM_E2E_CLAUDE_HOME=$HOME/.claude \
  cargo test -p terrariumd --test e2e_claude -- --ignored --exact claude_in_bwrap_answers_one_turn --nocapture
```

The test sends one model turn, on `haiku`. The agent runs in a
`terrarium-runner` process, as in the daemon. Set
`TERRARIUM_E2E_RUNNER_MODE=inprocess` to run it in the test process. The
test binds the Claude home read-write, as the container does. It does not
copy or change the credentials. Set `TERRARIUM_E2E_CLI` to use a CLI other
than `target/debug/terrarium`.

The same file has `two_claude_agents_share_one_home`, which spends four
model turns. Without `--exact`, `--ignored` runs both tests. The results
are in [compat.md](compat.md), "Claude inside bwrap".

### 6.2 The compatibility suite

The suite starts each real harness through its adapter and checks the
facts of [compat.md](compat.md). It does not use the daemon. See
[compat-suite.md](compat-suite.md).

```sh
just ci::compat claude
```

The argument is `all` (the default), `claude`, `codex`, `copilot`, `pi`, or
a comma list. The report goes to stdout and to `target/compat-report.md`.

- Cost: at most 3 model turns for each harness, on the cheapest model
  (Claude `haiku`, Codex `gpt-6-luna`, Copilot `claude-haiku-4.5`).
- The suite uses your real harness homes under `$HOME`, in sandbox mode
  `none`. It never runs a login or a logout. A harness that is not logged
  in is skipped.
- The user-hooks gate: when the shared home of a harness holds your own
  hooks (for example `~/.codex/hooks.json` or `~/.copilot/hooks/*.json`),
  the suite skips that harness. In mode `none` those hooks fire in the test
  agent. Set `TERRARIUM_COMPAT_ALLOW_USER_HOOKS=1` to run it anyway.
- pi does not run by default. To run it, set `TERRARIUM_COMPAT_PI=1`,
  `TERRARIUM_COMPAT_PI_PROVIDER` (an API-key provider, for example
  `openai`), `TERRARIUM_COMPAT_PI_MODEL`, and the API key of that provider
  (`OPENAI_API_KEY` for `openai`).

`cargo test -p terrarium-compat` without `--ignored` runs only the dry run.
It starts no harness.

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

### 7.5 Codex leaves a daemon behind

A Codex start with no `-c` override starts a detached
`codex app-server --managed-daemon`. It outlives the TUI, copies about
736 MB into `$CODEX_HOME/packages/`, and runs an update loop. The adapter
always passes `--no-daemon`, so agents do not cause it.

The usual cause is `codex` that you start yourself against the shared home.
Add `--no-daemon` when you do. Stop a leftover `codex app-server
--managed-daemon` process by hand. See [compat.md](compat.md), "Codex:
facts that contradict the plan".

### 7.6 Copilot is logged out in the sandbox

On a host, `copilot login` puts the OAuth token in the system keychain
(libsecret over D-Bus). A sandbox has no D-Bus, so Copilot starts logged
out there.

Give the agent a token in `COPILOT_GITHUB_TOKEN`:

1. Make a fine-grained GitHub token with the Copilot Requests permission.
2. On the Subscriptions page, click **Add subscription**, then **API key**.
   Select the GitHub provider, and paste the token.

The daemon stores the token with mode `0600`. At each start, it puts the
token into `COPILOT_GITHUB_TOKEN` for the agents on that subscription only.
The API is `POST /api/subscriptions/{name}/api-key` with
`{"provider":"github","key":"..."}` ([api.md](api.md) §16.5).

### 7.7 pi refuses a Claude plan

The daemon refuses to start a pi agent on an Anthropic plan subscription:
"pi cannot spend a Claude plan". Anthropic bills a Pro/Max login that pi
uses as extra usage, not from the plan ([compat.md](compat.md), "pi: facts
that contradict the plan"). Use the `claude` harness for the plan. To use
Anthropic through pi, make an API-key subscription with an Anthropic API
key.

### 7.8 Other messages

| Message | Fix |
|---|---|
| `set TERRARIUM_TOKEN in .env` (from compose) | Set `TERRARIUM_TOKEN` in `.env` (section 2.1). |
| `the claude home <dir> does not exist` | Log in to that harness first (section 2.4), or set `TERRARIUM_HARNESS_HOME=claude=<dir>` (section 4.1). The same message comes for `codex`, `copilot`, and `pi`. |
| ``no `terrarium-runner` binary next to terrariumd or on PATH`` | Run `cargo build -p terrarium-runner` (`just daemon::run` does it), set `TERRARIUM_RUNNER`, or set `TERRARIUM_RUNNER_MODE=inprocess`. |
| `cannot read the prompt template` | The agent names a template that is not in the prompts directory. Remove `--prompt`, or add `prompts/<name>.md`. |
| `no terrarium CLI found` (daemon log) | Run `just daemon::build`, or set `TERRARIUM_CLI`. |
| `awaiting server: this daemon does not serve this endpoint yet` | The CLI has the command, but the daemon does not have the route yet. |
| `... is not implemented yet` (from `just`) | The recipe belongs to a later phase. |

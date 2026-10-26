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

The image holds no harness. At the first start, the entrypoint installs
the pinned harnesses (`harnesses.toml`) and the Copilot prompt extension
into `tools/` (about 1.1 GB; 15 s on a fast line). Each later start finds
them there and goes on at once. See section 7.

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

#### Add a Z.ai (GLM) key

Z.ai sells the GLM models with an API key. Claude Code and pi can both
spend it. To add the key:

1. Make a key at `https://z.ai/manage-apikey/apikey-list`.
2. Open the **Subscriptions** page and click **Add subscription**.
3. Click **API key**, and set **Provider** to **Z.ai (GLM)**.
4. Set **Plan** to **GLM Coding Plan** or **Pay as you go (API)**.
5. Paste the key, and click **Save key**.

Or use the API:

```sh
curl -X POST http://localhost:8080/api/subscriptions/glm/api-key \
  -H "Authorization: Bearer $TERRARIUM_TOKEN" -H 'Content-Type: application/json' \
  -d '{"provider": "zai", "plan": "coding", "key": "<your key>"}'
```

Then give an agent `subscription = "glm"`, with `harness = "claude"` or
`harness = "pi"`. Set `model` to a GLM id, for example `glm-5.3`, or leave
it empty. Z.ai has no usage probe, so the tile shows "Local estimate
only". docs/api.md §16.5 lists the env that the agents get.

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

To make more agents with the same settings, use an agent template. Make
the template one time, then give only the name of each agent:

```sh
t -p demo template new worker --harness claude --model haiku --prompt default
t -p demo agent new w1 --template worker
t -p demo agent new w2 --template worker --model sonnet
```

A flag on `agent new` changes that field of the template. `t template ls`
lists the templates of the project and the global ones. The agent copies
the values: a later `template edit` does not change `w1` or `w2`. In the
dashboard, use the **Templates** page, the **Template** select of the
**New agent** form, or **Save as template** on an agent page
([api.md §21](api.md#21-agent-templates)).

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

### 2.8 Mode 1 workflow

In mode 1, the workspaces stay on the host and the targets carry the
toolchain (PLAN §11 and §19). The sandbox holds the harness, the CLI, and
`git`. An agent edits files and runs `git` in the sandbox. It runs builds
and tests on a target with `terrarium exec`. The host agent attaches the
workspace `./work/<project>/<agent>` into the target at
`/work/<project>/<agent>`.

These steps come from the live run of 2026-10-25 (project `svc`, repo
`hello-svc`, host agent `local`). The lead does steps 4 to 7 itself when it
runs the `project-lead` template. You can also do them by hand.

1. Run the host agent on the Docker host, and give it the host path of
   `./work`. A host agent that runs from the repo root uses `./work` by
   default. Otherwise, set `work_dir` in its config:

   ```sh
   terrarium-host --config state/host-local/config.toml check
   terrarium-host --config state/host-local/config.toml run
   ```

   `check` prints `work_dir`, `instances_dir`, and `repos_dir`, and marks a
   missing path. Run `t admin host ls` to see the host online.

2. Put the lab file at the root of the repo, as `vmlab.wcl`, and commit it
   to `main`. Use an image that has every tool of the build and the tests,
   because a lab file may not use `provision` (see the rules below).

3. Make the project, attach the repo, and start the lead:

   ```sh
   t project new svc --prefix SVC
   t -p svc project repo-add file:///repos/hello-svc.git --name hello-svc --branch main
   t -p svc admin agent new lead --harness claude --model sonnet \
     --prompt project-lead --role project_admin --subscription anthropic-max-20x
   t agent start svc/lead
   ```

4. Make a project target. A project target gets the project name as a
   prefix, so `dev` becomes `svc-dev`:

   ```sh
   t -p svc admin target new dev --kind vmlab --lab-from-workspace \
     --lab-path hello-svc --machine box --host local --ephemeral
   ```

   `--lab-path` is relative to the workspace root `work/<project>/<agent>`,
   and `repo checkout` puts the repo in `<repo>` under that root. So for a
   lab file at the root of the repo, use `--lab-path <repo>`. Never use
   `--lab-path .`. To fix a target, use `t admin target edit <target>
   --lab-path <dir>`. A running target takes only `--keep`, `--no-keep`,
   and `--snapshot-before-task`.

5. Grant the target to the worker before you assign the card:

   ```sh
   t admin target grant svc-dev w-svc-1 --until 4h --reason SVC-1 && \
     t card assign SVC-1 w-svc-1
   ```

   The grant brings the worker's own lab instance up
   (`svc-dev.<agent ULID>`). When the target does not come up, the grant
   exits 1, so the `&&` chain stops before the assign. The lease is made
   all the same: fix the target, revoke the lease, and grant again. Do not
   use `--exclusive` on a `lab_from_workspace` target (`422`).

6. Give the target to one worker at a time. vmlab 0.33.3-alpha keys a
   running lab by the name in its `lab` block. Each checkout has the same
   lab file, so a second instance fails ("lab ... is already
   registered"). Keep this rule until vmlab runs two instances of one lab.

7. Revoke the lease after the merge, not at `review`: the worker can need
   the target for a fix. The revoke destroys the worker's instance. Then
   `t admin agent rm <worker> --confirm` moves the agent dir and its
   workspace to `state/trash`.

**Settings for mode 1.**

| Setting | Where | Default | Content |
|---|---|---|---|
| `TERRARIUM_WORK_DIR` | daemon (`.env`) | `/work` | The work root. Each workspace is `<dir>/<project>/<agent>`. Compose binds `./work` here. |
| `TERRARIUM_MAX_TARGETS_PER_PROJECT` | daemon (`.env`) | `8` | The most targets in one project. A create past it is `409 conflict`. |
| `work_dir` | host agent config | `./work` in the working directory of the host agent | The host path of the daemon's work root. On the Docker host, set it to the `./work` of the stack. |
| `instances_dir` | host agent config | `$XDG_DATA_HOME/terrarium-host/instances` | Where `vmlab.prepare` makes the lab instances. |
| `repos_dir` | host agent config | `./repos` in the working directory of the host agent | The local bare repos, for a `--lab-repo` with a `file://` URL. |
| `max_running_targets` | host agent config | `4` | The most docker and vmlab targets that run on the host at one time. An `up` past it is `429 rate_limited`. |

**Lab file rules.** A `--lab-repo` or `--lab-from-workspace` lab is a file
that an agent can write. The host agent checks it against an allowlist
before `vmlab validate` ([host-protocol.md](host-protocol.md), "The lab
allowlist"). A refused lab gives `422` with `details.code` `refused`, and
the message names each line. The file must start with `import <vmlab.wcl>`
and hold one `lab` block. A lab file may not contain:

- another `import`;
- `provision` or `on` (a wscript runs on the host);
- `playbook`;
- a `template` block;
- a host path: `share`, `volume { host }`, `media`, `disk { from }`,
  `cdrom`, or `floppy`;
- `qemu_args` or `gpu`;
- a host port: `forward` or `port`;
- `connect` or `global`;
- `gui`;
- an `@dev(workspace)` that is absolute, has `..`, or leaves the lab's
  directory;
- a value that is not a literal, or a string with `${`;
- any other block, attribute, or decorator that is not in the allowlist.

A lab that needs more (an ISO, a share, a provision script) is a
pre-provisioned lab. Give it to the operator as a target with `--lab`.

### 2.9 Declare a fleet

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

### 2.10 Write your own API client

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

### 2.11 Send CI events from GitHub or Forgejo

An event source lets a forge post webhooks to Terrarium. Each webhook
becomes one event in the log, for example `ci.run.failed`. A trigger can
then mail an agent (docs/api.md §19 and §20).

The stack serves the webhooks on a port of their own: container port
8083, published as `TERRARIUM_INGEST_PORT` (default `8083`). This port
serves only `/hooks/...` and `/healthz`, not the API. Open this port to
the forge. Keep the API port (8080) closed.

1. Set the URL that the forge can reach in `.env`, and start the stack
   again:

   ```sh
   TERRARIUM_INGEST_PUBLIC_URL=https://hooks.example.com
   ```

   Without it, the hook URL is `http://localhost:8083/...`. That URL is
   correct only when the forge runs on the same machine.

2. Make a source for the project. The command prints the secret one time,
   the hook URL, and the setup steps:

   ```sh
   just daemon::cli event source add gh --kind github --project terrarium
   ```

   Keep the secret. When you lose it, run `event source rotate
   terrarium/gh`, and give the forge the new one.

3. **GitHub.** Open the repository, then Settings > Webhooks > Add
   webhook.
   - Payload URL: the hook URL, for example
     `https://hooks.example.com/hooks/terrarium/gh`.
   - Content type: `application/json`. The form type gets `415`.
   - Secret: the secret from step 2.
   - Events: "Let me select individual events". Select Workflow runs,
     Workflow jobs, Pushes, and Pull requests.

   GitHub sends a `ping` at once. Run `event source show terrarium/gh`:
   `last seen` shows the time of the ping.

4. **Forgejo.** Make a source with `--kind forgejo`. Open the repository,
   then Settings > Webhooks > Add webhook > Forgejo.
   - Target URL: the hook URL. HTTP method: `POST`. POST content type:
     `application/json`.
   - Secret: the secret from step 2.
   - Trigger on: custom events. Select Push and Pull request. On Forgejo
     17 and later, also select the workflow run and workflow job events.
     On Forgejo 12 to 16, select the Action run events (failure, recover,
     success). Do not select both kinds of run events on Forgejo 17: then
     each run makes two events.

   Forgejo has no ping. Click "Test delivery": Forgejo sends a `push`, and
   the log shows a `git.push` event.

5. Check the events:

   ```sh
   just daemon::cli events --kind ci.run.failed
   ```

A request with a bad signature gets `401` with no detail. The source
keeps the reason: `event source show` prints it as `last error`.

## 3. Configuration directories

Compose bind-mounts these host directories into the container:

| Host directory | Container path | Content |
|---|---|---|
| `config/claude/` | `/auth/claude` | The Claude Code home (`CLAUDE_CONFIG_DIR`): login, sessions, `.claude.json`. |
| `config/codex/` | `/auth/codex` | The Codex home (`CODEX_HOME`). |
| `config/copilot/` | `/auth/copilot` | The Copilot CLI home (`COPILOT_HOME`) and the system-prompt extension. |
| `config/pi/` | `/auth/pi` | The pi home (`PI_CODING_AGENT_DIR`). |
| `state/` | `/state` | SQLite, agent logs and harness files, artifacts, credentials. |
| `tools/` | `/opt/terrarium/tools` | The harnesses: the npm global prefix (`TERRARIUM_TOOLS_DIR`, `NPM_CONFIG_PREFIX`). The entrypoint installs the pins here at the first start; updates go here in place (section 7). The sandbox binds it read-only. Delete it to reinstall the pins. |
| `work/` | `/work` | Agent workspaces, `<project>/<agent>/` (`TERRARIUM_WORK_DIR`). A host agent on this machine attaches them to targets: set its `work_dir` to this directory (docs/host-protocol.md, "Workspaces"). |
| `prompts/` | `/prompts` (read-only) | System prompt templates. |
| `projects/`, `agents/`, `targets/`, `subscriptions/` | `/etc/terrarium/...` | Definition files. |
| `fleets/` | `/etc/terrarium/fleets` | Fleet files (section 2.9). |
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

**Tools in the sandbox.** The sandbox uses the tools of the image: `git`,
`ssh`, Node 22, and `python3` (Python 3.13, the Debian trixie package, with
no `pip` and no `venv`). The harnesses come from `tools/`:
`/opt/terrarium/tools/bin` is on `PATH`, and the dir is read-only. The sandbox has no project toolchain. Run builds
and tests on a target (section 2.8), and keep the toolchain in the image
of the target.

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
| `TERRARIUM_TOOLS_DIR` | `<repo>/tools` |

To see the full command, run `just --dry-run daemon::run`.

**The harnesses.** At start, the daemon installs each pin of
`harnesses.toml` that is missing from `TERRARIUM_TOOLS_DIR` (with the
`npm` on your `PATH`, or `TERRARIUM_NPM`). Then it starts again with
`<tools>/bin` first on `PATH`. So a dev run uses the pinned versions, not
the harnesses that you installed yourself. It also installs the Copilot
prompt extension into the Copilot home of `TERRARIUM_HARNESS_HOME`. This is
the same `tools/` dir that the Docker stack uses. To skip the install, run
`just daemon::run --no-harness-install`. To use the harnesses on your
`PATH` as they are, set `TERRARIUM_TOOLS_DIR=` (empty). To install with no
daemon, run `cargo run -p terrariumd -- harness install`.

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
and tests only. See also section 8.4.

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
`-D warnings`), `ci::test` (`cargo test --workspace`), `ci::scripts-test`
(the Python tests under `scripts/`, which also check that the Dockerfile is
in step with `harnesses.toml`), and `ci::dashboard-check` (TypeScript,
ESLint, and the dashboard unit tests). A change must pass it before it
merges.

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

### 6.3 The Z.ai test

The Z.ai test runs one turn on `glm-5.3-flash` through pi. It spends a
little of your Z.ai plan. Run it with your key:

```sh
ZAI_API_KEY=<key> cargo test -p terrarium-adapter-pi --test zai_live -- --ignored
```

Set `ZAI_PLAN=api` to use the pay-as-you-go endpoint. Set `PI_BIN` to
choose the pi binary (default: `pi` on `PATH`, for example
`./tools/bin/pi`). Without `ZAI_API_KEY`, the test does nothing.

## 7. Updating a harness

The image holds no harness. The harnesses live in `tools/` on the Docker
host, which is `/opt/terrarium/tools` in the container
(`TERRARIUM_TOOLS_DIR`). That dir is the npm global prefix
(`NPM_CONFIG_PREFIX`), and its `bin/` comes first on `PATH` for the
daemon, the runners, and the sandbox. The sandbox binds it read-only, so
an agent cannot change a harness.

`harnesses.toml` at the repo root has two jobs
([CONVENTIONS.md](CONVENTIONS.md)):

- It gives the versions of the **first install**. The image copies it to
  `/etc/terrarium/harnesses.toml`. At each start, the entrypoint runs
  `terrariumd harness install`, and the daemon runs the same step when it
  starts. The step installs each pin that is missing from the tools dir,
  checks `<binary> --version`, and installs the Copilot prompt extension
  into the Copilot home when its version or the Copilot version changed.
  A harness that is present stays at its version, also when it is not the
  pin. So an update survives a restart and a new image. The step writes
  the manifest `tools/terrarium-manifest.json`:
  `{"<name>": {"package", "version", "installed_at"}}`.
- It gives the **tested range** of each adapter. `crates/terrarium-core/build.rs`
  reads the file at compile time, and each adapter's `flags.rs` takes
  `verified` and `tested` from it. At each agent start, the daemon compares
  the installed version with that range (section 7.5).

An update runs `npm install -g <package>@<version>` in the running
container. It needs no image build, no restart, and no host agent.

### 7.1 Check

```sh
just harness::check           # a table
just harness::check --json
```

For each harness and the extension, the table shows the pin, the tested
range, the version in the container (from the manifest), the version on
the host, the latest version on npm with its publish date, and whether the
latest version is in the tested range (`LATEST IN TESTED RANGE`). The
`STATUS` column compares the version in the container with the latest:
`latest`, `update available` (the same major version), or
`update available (major)`. The command changes nothing.

The dashboard's Subscriptions page shows the same status. A row with a
newer release has an "Update" button, and the page has "Update all" when
more than one row has one. The buttons call
[`POST /api/harnesses/{name}/update`](api.md#post-apiharnessesnameupdate-and-post-apiharnessesupdate),
show npm's output, and refresh the table.

### 7.2 Update one harness

```sh
just harness::update claude --dry-run    # print the plan, change nothing
just harness::update claude              # the latest version on npm
just harness::update claude 2.1.320      # a given version
just harness::update claude --pin        # also write the new pin
```

The command needs the stack to run. It calls the daemon through the CLI
in the container (`terrarium admin harness update`), so you can also run
that command yourself:

```sh
docker compose exec -u terrarium terrarium terrarium admin harness update claude --version 2.1.320
```

The daemon runs npm, reads the new `--version`, refreshes its version
cache, and writes the audit entry `harness.update` and a `harness` event.
The command prints npm's output and `claude 2.1.310 -> 2.1.320`. The next
start of each agent runs the new version.

- **Running agents.** While an agent on the harness runs, the daemon
  refuses the update (`409`) and names the agents. A running harness keeps
  the files that it loaded, but a swap in the middle of a turn is risky.
  Stop the agents, or add `--force`.
- **A major bump.** From the version in the container, a major bump needs
  `--allow-major`. The command uses the semver rule for "major": the first
  part that is not zero. Thus, for a 0.x harness, 0.164.0 to 0.165.0 is a
  major bump, and 0.164.0 to 0.164.1 is a patch bump.
- **`--pin`.** After a done update, the command also sets the new version
  in `harnesses.toml`, widens `tested` for a patch or minor bump (`<2.2.0`
  becomes `<2.3.0`), and adds a row to the "Version history" table in
  [compat.md](compat.md). So a fresh install (an empty `tools/`) gets the
  same version. The adapters read the new `tested` range at the next image
  build (`just up`). Commit the files.

Without `--pin`, a version outside the tested range is a warning, not an
error: the agents still start (section 7.5).

### 7.3 Update every harness to the latest

```sh
just harness::latest --dry-run       # print the plan, change nothing
just harness::latest                 # update each one with a newer release
just harness::latest --pin           # and write the new pins
```

The command reads the latest release of each harness and of the Copilot
prompt extension on npm. It skips each one that is at the latest. It
updates each of the other ones, as in 7.2, with one call each. It allows a
major bump, and it prints a one-line warning for each: hooks and flags may
have changed. At the end, it prints a table of the changes and one
`just ci::compat <name>` command for each updated harness.

`terrarium admin harness update --all` updates every harness and the
extension to the latest in one call, with no plan.

**No rollout.** An update changes the files in `tools/` in place. The
container does not restart, and no agent stops. There is no
`just harness::rollout`.

**With the daemon stopped.** The update needs the daemon. When the
daemon does not run, open a shell in the container (`just docker::shell`)
and run `npm install -g <package>@<version>`. `NPM_CONFIG_PREFIX` points
npm at the tools dir. The dashboard shows this line for each row with a
newer release.

### 7.4 Prove the new version

The compatibility suite runs the harness binary on the host, with your
real logins. Install the new version on the host first (with npm, or with
the installer that you use for the harness). Then run the suite:

```sh
npm install -g @anthropic-ai/claude-code@2.1.320
just ci::compat claude
```

Write the result in the "Compat" cell of the new row in
[compat.md](compat.md), "Version history".

When a step fails, compare the failure with the facts for the old version
in [compat.md](compat.md). Look at these items:

- The hook names and the hook payload fields ("Hooks"). An adapter maps
  them in its `hooks.rs`.
- The flags and the env vars ("Quiet start", "Summary"). They are in the
  adapter's `flags.rs`.
- The session file and the usage fields ("Session file and usage
  accounting").
- The screen text that the adapter reads ("Typing into the TUI").

When a fact changes, add a section for the new version to
[compat.md](compat.md), and change the adapter table in `flags.rs`.

### 7.5 The version guard

At each agent start, the daemon compares the installed version with the
tested range. Outside the range, the daemon logs a warning and writes a
`warning` event for the agent, and the agent starts. Set
`TERRARIUM_HARNESS_VERSION_POLICY=refuse` to refuse the start
(`422 invalid`) in its place. `terrarium admin harness ls`,
`GET /api/harnesses`, and the Subscriptions page of the dashboard show
the installed version of each harness and its range.

### 7.6 Roll back

Update to the old version:

```sh
just harness::update claude 2.1.310
```

If you used `--pin`, `git checkout harnesses.toml docs/compat.md` undoes
the file changes before a commit. To go back to the pins of
`harnesses.toml` for every harness, stop the stack (`just down`), delete
`tools/`, and start it (`just up`): the entrypoint installs the pins again.

`just ci::check` fails when the `NODE_IMAGE` default in
`docker/Dockerfile` differs from `[node] image` in `harnesses.toml`, or
when the Dockerfile installs a harness. To fix the default, run
`uv run python scripts/harness-versions.py sync`.

## 8. Troubleshooting

### 8.1 bwrap cannot make a sandbox

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

### 8.2 Claude Code refuses yolo as root

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

### 8.3 "Session ID ... is already in use"

Claude Code exits at once with `Error: Session ID <uuid> is already in
use.` when `--session-id` gets a UUID that it saw before. The daemon makes a
new UUID for each fresh start and uses `--resume` for a known session. Thus
the daemon does not cause this error.

The usual cause is a launch command that you ran by hand two times, for
example the output of `terrariumd sandbox-dry-run <agent>`. Use
`--resume <uuid>` to continue that session, or start the agent through the
daemon with `terrarium agent start`.

### 8.4 The trust dialog in sandbox mode `none`

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

### 8.5 Codex leaves a daemon behind

A Codex start with no `-c` override starts a detached
`codex app-server --managed-daemon`. It outlives the TUI, copies about
736 MB into `$CODEX_HOME/packages/`, and runs an update loop. The adapter
always passes `--no-daemon`, so agents do not cause it.

The usual cause is `codex` that you start yourself against the shared home.
Add `--no-daemon` when you do. Stop a leftover `codex app-server
--managed-daemon` process by hand. See [compat.md](compat.md), "Codex:
facts that contradict the plan".

### 8.6 Copilot is logged out in the sandbox

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

### 8.7 pi refuses a Claude plan

The daemon refuses to start a pi agent on an Anthropic plan subscription:
"pi cannot spend a Claude plan". Anthropic bills a Pro/Max login that pi
uses as extra usage, not from the plan ([compat.md](compat.md), "pi: facts
that contradict the plan"). Use the `claude` harness for the plan. To use
Anthropic through pi, make an API-key subscription with an Anthropic API
key.

### 8.8 Other messages

| Message | Fix |
|---|---|
| `set TERRARIUM_TOKEN in .env` (from compose) | Set `TERRARIUM_TOKEN` in `.env` (section 2.1). |
| `the claude home <dir> does not exist` | Log in to that harness first (section 2.4), or set `TERRARIUM_HARNESS_HOME=claude=<dir>` (section 4.1). The same message comes for `codex`, `copilot`, and `pi`. |
| ``no `terrarium-runner` binary next to terrariumd or on PATH`` | Run `cargo build -p terrarium-runner` (`just daemon::run` does it), set `TERRARIUM_RUNNER`, or set `TERRARIUM_RUNNER_MODE=inprocess`. |
| `cannot read the prompt template` | The agent names a template that is not in the prompts directory. Remove `--prompt`, or add `prompts/<name>.md`. |
| `no terrarium CLI found` (daemon log) | Run `just daemon::build`, or set `TERRARIUM_CLI`. |
| `awaiting server: this daemon does not serve this endpoint yet` | The CLI has the command, but the daemon does not have the route yet. |
| `... is not implemented yet` (from `just`) | The recipe belongs to a later phase. |

# Getting started

This guide tells you how to run Terrarium in Docker, how to run it on your
machine for development, how to run the tests, and how to fix common
problems. Terrarium is early work. All four adapters exist, but only the
Claude Code adapter is verified end to end (see the status in
[../README.md](../README.md#status)).

Run every command from the root of the repository.

To install Terrarium on a server, use `just deploy::server` (section
2.14). It builds the image on the server. No image is in a registry.

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

### 2.3 Set the password and open the dashboard

The dashboard and `terrarium login` use the operator password. Set it once:

```sh
docker compose exec terrarium terrariumd password set
```

The command asks for the password twice, with no echo. Use 12 characters or
more. The daemon keeps only an Argon2id hash, in `state/operator.password`.
A running daemon sees the new password at once. To change the password
later, run the command again, or use Settings, Account in the dashboard.
Each change ends every session.

For a first start with no prompt, put `TERRARIUM_PASSWORD=...` in `.env`.
The daemon sets the password at the start when none is set. Then remove the
line from `.env`: after the first start the daemon ignores it.

Then open <http://localhost:8080>. If you changed `TERRARIUM_PORT` in `.env`, use
that port. The dashboard shows a login screen. Enter the operator password.
If you did not set a password, select **Use the token instead** and paste
the value of `TERRARIUM_TOKEN`.

At sign-in, the dashboard sends the password (or the token) to the daemon
once (`POST /api/session`). The daemon answers with a session cookie and
the dashboard drops the password. The cookie is `HttpOnly`, so no script on
the page can read it. The cookie is the cache: it stays through a reload, a
new tab, and a browser restart, so you do not log in again. It lives
`TERRARIUM_SESSION_TTL` (default 30 days), and the daemon renews it while
you use the dashboard. The dashboard keeps only the CSRF value, in memory.
**sign out** in the sidebar ends the session. **Log out everywhere** on the
Account tab of Settings ends every session, in each browser and each CLI
([../dashboard/src/README.md](../dashboard/src/README.md)).

After five wrong passwords from one address, the daemon makes you wait:
30 s, then twice as long after each more failure, up to 15 min. A session
lives at most `TERRARIUM_SESSION_MAX_AGE` (default 90 days) from the login,
however often it renews.

To use the CLI from another machine, log in over `https://` (a TLS proxy in
front of the daemon): `terrarium --url https://terrarium.example.com login`.
`terrarium login` refuses plain `http://` to another host unless you add
`--insecure-http` ([cli.md](cli.md) §2.1).

Set `TERRARIUM_PUBLIC_URL` in `.env` to the dashboard URL as you open it.
The daemon puts this URL in the links of operator mail (the default is
`http://localhost:8080`).

### 2.4 Log in to Claude

All agents share one Claude settings home (`config/claude`). Each plan
login is an **account**: its own subscription, with its credential under
`state/credentials/accounts/<subscription>/` (PLAN §16.3). Use the
dashboard to log in. A shell login also works for the first account.

To log in from the dashboard:

1. Open the **Subscriptions** page.
2. Click **Add subscription**, then click **Claude Code**.
3. Click the login URL that the wizard shows, and sign in.
4. Paste the code from the browser into the code field, and click
   **Send code**.

The wizard runs `claude auth login` in an embedded terminal, outside the
sandbox, in a scratch home. When the login is done, the daemon moves the
credential into the account of the subscription, makes the subscription,
verifies the login, and runs the quota probe one time. The **Verify now**
button checks the login again. The wizard has the same flow for Codex and
Copilot (a device code). To store an API key, click **API key**.

pi uses API keys only. Use Claude Code for a Claude plan, and Codex for a
ChatGPT plan. Give a pi agent an API-key subscription: Z.ai, OpenRouter,
an Anthropic or OpenAI API key, or another provider of the **API key**
form. The daemon refuses a pi agent, a pi template, or a fleet pi agent on
a plan login (`422`).

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
finds the login and offers to make a subscription for it. A shell login
goes into the shared home. The daemon moves it into the account of the
one subscription that fits it, when no Claude agent runs (PLAN §16.3, "The
migration").

#### Add a second account

You can have more than one login of a provider, for example two Claude Max
logins. Each login is its own subscription. To add a second login:

1. Open the **Subscriptions** page.
2. Click **Add subscription**, then click **Claude Code** (or **Codex**, or
   **Copilot**).
3. Keep the **Name** field empty, or type a name such as `max-work`.
4. Sign in with the **other** account in the browser. Paste the code, or
   enter the device code.

When the default subscription (for example `anthropic-max-20x`) has an
account, the daemon names the new one `anthropic-max-20x-2`. Each account
has its own card, with its quota bars and the agents on it. **Re-login**
on a card replaces the login of that account only.

The first account of a harness stays the **default** account. Its card
has the `default` badge. An agent with no subscription runs on the default
account of its harness. To change the default:

1. Find the card of the other account.
2. Click **Make default**.

You can also run `terrarium admin subscription default anthropic-max-20x-2`.
When you delete or log out the default account, the oldest account left
becomes the default. When one account is left, it becomes the default.

#### Move agents between accounts

Give an agent a pool of accounts, then move it when an account is used up:

```sh
terrarium admin agent set terra/lead --pool anthropic-max-20x,anthropic-max-20x-2
terrarium admin agent swap terra/lead anthropic-max-20x-2
```

Claude Code moves live, at its next request. Codex and Copilot restart
with resume when the turn ends. A pi agent moves between API keys by a
restart with resume. A probe at 100 percent only sends you a mail: a
probe can show 100 percent while the plan still serves turns. When the
provider refuses a turn with a usage-limit error (Claude Code only, for
now), the daemon moves an agent with a pool to the next account that did
not hit a limit (`TERRARIUM_AUTO_SWAP=off` turns this off). An agent with
no account left is `blocked` until the reset, and you get a mail. The
agent page of the dashboard has the pool editor and **Swap**.

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

#### Add a Moonshot (Kimi) key

Moonshot AI sells the Kimi models with an API key. Only pi agents can
spend a Moonshot key. The daemon refuses a Claude Code, Codex, or Copilot
agent on it. Moonshot has two products, and each has its own keys:

- **Kimi Code**, a membership. Make a key in the Kimi Code Console at
  `https://www.kimi.com/code/console`.
- **Open Platform**, pay as you go. Make a key at `https://platform.kimi.ai`
  (China: `https://platform.kimi.com`).

A key works only in the region where you made it. To add the key:

1. Open the **Subscriptions** page and click **Add subscription**.
2. Click **API key**, and set **Provider** to **Moonshot AI / Kimi (pi
   only)**.
3. Set **Plan** to the product and the region of your key.
4. Paste the key, and click **Save key**.

Or use the API. The plans are `coding` (Kimi Code), `coding-cn` (Kimi
Code, China), `api` (Open Platform), and `api-cn` (Open Platform, China):

```sh
curl -X POST http://localhost:8080/api/subscriptions/kimi/api-key \
  -H "Authorization: Bearer $TERRARIUM_TOKEN" -H 'Content-Type: application/json' \
  -d '{"provider": "moonshot", "plan": "coding", "key": "<your key>"}'
```

Then give a pi agent `subscription = "kimi"`. Set `model` to a Kimi id,
for example `k3` or `k3.1-pro`, or leave it empty. An Open Platform plan
has a balance probe. A Kimi Code plan has no usage probe, so the tile
shows "Local
estimate only". docs/api.md §16.5 lists the env that the agents get.

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
```

`--model` is optional. `haiku` keeps each turn cheap.

You do not start the agent. Its first turn or mail wakes it: the daemon
starts it, and types the turn when it is ready. After 10 minutes at rest
(`idle` or `waiting`), the agent stops by itself, and its status shows
`stopped (idle)`. The next turn wakes it again, with the same
conversation. Change the time for one agent with `--idle-stop 30m` (or
`off`), or for all agents with `TERRARIUM_IDLE_STOP` in `.env`.

A long conversation fills the context of the harness. The daemon can
compact it for you: `--compact-on-idle on` compacts the session before the
idle stop, and `--compact-max-age 24h` compacts a session that is older
than 24 hours at its next rest. Each compaction is one model call, so both
are off by default (`TERRARIUM_COMPACT_ON_IDLE`,
`TERRARIUM_COMPACT_MAX_AGE` in `.env`). An automatic compaction runs only
after a new turn. To compact now, run `t agent compact demo/worker`, or
click **Compact** on the agent page. While the session compacts, the
agent shows `working (compacting)`, and new turns wait. To start a clean
session instead (not a resume), run `t agent new-session demo/worker`, or
click **New session**. Both can carry a prompt that goes after the reset
(`--then "..."`, or **Then send…** in the dialog); §2.13f has the details.

Give the agent a turn, and look at its status:

```sh
t agent send demo/worker "Make a file hello.txt that says hello."
t -p demo agent ls
t agent logs demo/worker
```

Send mail to the agent, and read the answer:

```sh
t mail send demo/worker -s "first task" "Tell me what is in your workspace."
t admin inbox ls --kind mail
```

You can also do these steps in the dashboard. On the Projects page, click
**New project**. In the sidebar, open the project and click **Agents**.
Then click **New agent**, and pick a template or **Custom**. With no
template, the form opens at once. Then open the agent and type a turn in
the composer: the turn wakes the agent. The agent page has no Start or
Stop buttons. **Pause** stops the agent and keeps it stopped: turns and
mail wait, and the page shows how many. **Resume** lets it wake again.
**Restart** starts a new launch with resume.

To add a repo in the dashboard, open the project in the sidebar and click
**Repos**. Then click **Add repo**, and give the URL. The project settings
(the cog of the project row) do not have the repos any more. Click the name
of a repo to open its browser: select `main` or the checkout of an agent,
then use the **Files**, **Changes**, and **Commits** tabs. From the shell,
use `t -p demo project repo-add <url>`.

Each project has a board. A new board has the columns `backlog`, `ready`,
`in_progress`, `review`, `blocked`, and `done`. To change them, open
**Board** and click **Edit columns**, or use the CLI:

```sh
t -p demo board columns
t -p demo board column add qa --title QA --wip 2 --after review
t -p demo board column set in_progress --rename doing --title "Doing now"
t -p demo board column rm ready
```

`backlog` stays first and `done` stays last. One column has the role
`working` (`card claim` moves cards there), and at most one has the role
`blocked` (a move there needs a comment). A column with cards cannot go.
A lead agent (project_admin) can change the columns too.

To see the cards by version, set **Rows** to **version** on the board.
Each version that is not archived gets a row. Drag a card into another row
to move it to that version. Click a version name to open the version:
review it, add or remove cards, or archive it.

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
dashboard, use the **Templates** tab of **Settings**, or **Save as template** on an agent
page ([api.md §21](api.md#21-agent-templates)). **New agent** first shows
the templates of the project and the global ones. Click a template, give
the name, and click **Create agent**. To change a field of the template,
click **Change settings…** before the create. To set every field yourself,
click **Custom**. The **Templates** tab of the
project settings (the cog of the project in the sidebar) shows the
templates of one project.

Each template is a WCL file in the config repo (section 2.12):
`config/terrarium/templates/worker.wcl` for a global template,
`config/terrarium/templates/demo/worker.wcl` for a template of `demo`.
`t template show --wcl demo/worker` prints it, and **View WCL** on the
**Templates** tab shows it. You can edit the file by hand: the daemon reads
it again within 10 seconds. To make a template from a file, run
`t -p demo template new --from-file worker.wcl`. Terrarium ships eight
global templates (`claude-lead`, `claude-worker`, `codex-worker`, and
others) that name a subscription (`anthropic-max-20x`, `openai-pro`, and others, section 2.4); an agent create from
one of them needs that subscription, or a `--subscription` flag.

Each harness keeps its own default system prompt, and Terrarium adds its
protocol prompt after it. To replace the harness's default prompt, give
the agent or the template a base system prompt: `--system-prompt <text>`
or `--system-prompt-file <path>`, or the **System prompt** field in the
dashboard. Terrarium's protocol prompt still comes after it. The change
applies at the next start or restart (`defaults/prompts/README.md`).

**Pick or save a system prompt.** The system prompt library holds
premade system prompts: `config/terrarium/system-prompts/<name>.md`
(PLAN §8.1). `leader` and `worker` come with Terrarium: an agent with
`project_admin` or `sysadmin` gets `leader` by role, a member gets
`worker`. In the dashboard, the **System prompt** field of an agent or a
template is one selector:

- **By role**: `leader` or `worker`, by the roles of the agent.
- **Harness default**: no system prompt (`persona = none`).
- A library entry, with **Preview** to read its text.
- **Custom…**: your own text in a text box. **Save as new system prompt**
  asks for a name and an optional description, adds the text to the
  library, and selects the new entry.

Settings → **System prompts** lists the library: view, edit, delete, and
**New system prompt**. A delete of an entry that agents use shows them,
and asks again. With the CLI:

```sh
t system-prompt ls
t system-prompt new reviewer --file reviewer.md --description "Reviews cards"
t -p demo agent set w1 --persona reviewer
t -p demo agent set w2 --save-system-prompt careful   # save w2's own text, and select it
```

A new or changed entry is a file in the config repo: commit it on the
**Harness config** page, or with `t admin config commit`. An agent gets
it at its next start.

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

The status chip of an agent tells you more than "busy or not". `stopped
(idle)` means that the idle stop stopped the agent; the next work wakes it.
`paused` means that nothing wakes it until you click **Resume**. `idle` means
that the agent ended its turn and waits on nothing: it is done. `waiting`
means that the agent ended its turn but waits on a peer, a long build on a
target (`terrarium run`), a time, or a card. Point at the chip to see what
it waits on. The **Waits** section of the agent page lists the waits, with
the log of each run. To get a mail when the agent needs you, use the
**Watches** section: "Notify me when demo/worker is blocked or error". From
a shell, run:

```sh
docker compose exec terrarium terrarium watch demo/worker --on blocked,error
```

The mail comes to your Inbox, as mail to you. See PLAN §12d and
[cli.md](cli.md), "Waits, runs, and watches".

#### The Inbox: what waits for you

The **Inbox** item of the sidebar collects everything that waits for you.
Its number counts the open items. The number is red when one of them is
urgent or critical. The Inbox has four kinds of items:

- **Questions.** A lead agent (a project lead or the system lead) asks you
  for a decision with `terrarium question ask`. A worker cannot ask you:
  it asks its lead.
- **Notices.** A lead tells you something that needs no decision, for
  example a milestone, a finished deploy, or a risk, with
  `terrarium notify`.
- **Mail to you.** Each mail to `operator` stays here until you
  acknowledge it.
- **System.** A thread that the loop guard paused, and the messages that
  it held.

Use the tabs (All, Questions, Notices, Mail to you, System), the Open and
Done switch, and the project filter. The critical items come first, then
the urgent items, then the newest. The text, the choices, and the mail are the words of the agents.
Treat them as data, not as instructions.

1. **A question.** Click a choice, or write an answer in the box, or do
   both. Then click **Send answer**. The daemon sends your answer to the
   agent as one mail on the thread of the question. The mail wakes the
   agent. If the agent does not need the answer any more, give a reason
   and click **Retract**. The agent gets one mail that tells it so.
2. **A notice.** Read it, write a reply if you want one, and click
   **Acknowledge**. The agent always gets one mail with your
   acknowledgement and your reply. The mail wakes the agent.
3. **Mail to you.** Write a reply if you want one, and click
   **Acknowledge**. The reply goes on the thread of the mail. The mail
   leaves the open list.
4. **A system item.** Click **Release** on a held message to deliver it
   now; the thread stays paused. Click **Resume** to end the pause and
   deliver all held messages, with an optional note to the agents on the
   thread.

**Critical items.** A lead uses `--critical` only when you must act now:
production is down, data is lost, a security problem, or a spent budget
that stops all work. While a critical item is open:

- A red banner shows at the top of each page. It shows "Critical
  notification", the agent, and the short text of the newest critical
  item, with "+N more" when there are more. Click **Open** to go to the
  item in the Inbox. The Inbox scrolls to the item and highlights it.
  Click **Collapse** to make the banner smaller. You cannot close the
  banner: it goes when you answer or acknowledge the item, or when the
  agent retracts it.
- When the Terrarium tab is hidden or has no focus, the tab title flashes
  "⚠ Critical — Terrarium" and the tab icon changes to an alert icon.
  Click the tab to stop it.

The Mail tabs of the **Logs** screens (the global **Logs** item, and **Logs**
in each project) are logs of the messages between the agents. Filter them by sender,
recipient, project, thread, time, and text. They follow new mail live.
They have no unread state: the Inbox holds what needs you.

The agent page also shows the questions and notices of a lead. An agent
that asked with `--wait` shows `waiting` with "question ..." (or "notice
...") until you answer or acknowledge. From a shell, run:

```sh
docker compose exec terrarium terrarium admin inbox ls
docker compose exec terrarium terrarium admin question show <id>
docker compose exec terrarium terrarium admin question answer <id> "Ship it on Friday."
docker compose exec terrarium terrarium admin question answer <id> --option 2
docker compose exec terrarium terrarium admin inbox ack <id> "Thanks."
docker compose exec terrarium terrarium admin inbox resume <thread> -m "Stop the loop."
```

A question or notice stays on the agent. A new session of the agent, or
the agent after a compaction, reads it again with `terrarium question ls`.
See PLAN §12e and [cli.md](cli.md), "Questions and notices to the
operator".

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

4. Set `TERRARIUM_PUBLIC_URL` in `.env` to a name that the hosts can
   reach, for example `http://terrarium.lan:8080`, and start the stack
   again. The installer downloads from this URL. Its host, with port 8082,
   is the `daemon_url` of the host agents. When the hosts dial another
   name or port (a proxy, or another published port), also set
   `TERRARIUM_HOST_DAEMON_URL`, for example `wss://terrarium.lan:9082`.

#### Add a host

Do these steps for each machine that runs `docker` or `vmlab` targets, for
example a physical machine with Docker and vmlab. The machine needs Linux
on x86_64, `sudo`, `curl` (or `wget`), and systemd for the service.

1. Get the install command. Run this on the Docker host, or click
   **Add host** on the Hosts page of the dashboard and give the name:

   ```sh
   docker compose exec terrarium terrarium admin host new lab1 --enrol
   ```

   The command prints a one-time enrol token, and on the next lines the
   install command:

   ```sh
   curl -fsSL http://terrarium.lan:8080/install/host.sh | sudo sh -s -- --token the_... --name lab1
   ```

2. On the machine, log in as the user who owns the labs, and run the
   install command. Add `--dry-run` to see the plan first. Nothing changes
   then.

3. The script does these steps:
   - It downloads `terrarium-host` from the daemon, checks its SHA-256, and
     installs it as `/var/lib/terrarium-host/bin/terrarium-host` (owned by
     the service user, so the agent can update itself when the daemon has
     a new build), with a symlink at `/usr/local/bin/terrarium-host`.
   - It writes `/etc/terrarium-host/config.toml` with `daemon_url`, the
     name, and the certificate pin, and writes the enrol token to
     `/etc/terrarium-host/token`.
   - It finds Docker and vmlab. When Docker is on the machine, it adds the
     service user to the group `docker`. This group is as strong as root on
     the machine. When vmlab is on the machine, it adds the user to `kvm`.
   - It installs, enables, and starts the systemd unit `terrarium-host`,
     and probes the link to the daemon.
   - It prints a summary: the name, the daemon, and `docker` and `vmlab`
     (`yes` or `no`).

4. Run `terrarium admin host ls`, or open the Hosts page. The host shows
   `online`.

The host agent runs as the user who ran `sudo`, not as root. vmlab runs as
the user who owns the labs, and the host agent runs each command of a
target as its own user. Use `--user <name>` to pick another user. When you
log in as root with no `sudo`, the script makes the system user
`terrarium`.

To update the host agent, run the same command with `--upgrade` in place of
`--token ...`. It replaces the binary, keeps the config and the token, and
restarts the service. To remove the host agent, use `--uninstall`. It stops
and removes the unit and the binary, and keeps `/etc/terrarium-host`. Add
`--purge` to remove that too. Then run `terrarium admin host rm lab1` to
revoke the token. A second run of the install is safe: it keeps the config,
and it keeps the token file unless you give a new `--token`.

Without systemd, the script prints the command that runs the host agent.
Start it yourself, for example from your init system.

#### Add a host over ssh

From a checkout of the repo, `just host::install` builds `terrarium-host`
locally, copies it to the machine over ssh, and does the same set-up. It
gets its own enrol token from the daemon:

```sh
just host::install root@lab1.lan lab1
```

The ssh user must be `root`, or must have `sudo` with no password prompt.
The default service user is `terrarium`; use `--user` to change it. Add
`--dry-run` to see each command first. Nothing runs then. The recipe finds
`daemon_url` in `--daemon-url`, then in `TERRARIUM_HOST_DAEMON_URL`, then
in the host of `TERRARIUM_PUBLIC_URL` (port 8082). A `localhost` URL stops
the install, because a remote machine cannot dial it.

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

#### Add a screen target (VNC or RDP)

A host agent can show an agent the screen of a machine that it reaches
over VNC or RDP. The host agent has the VNC and RDP clients in it, so it
needs no other program. Give each target a host agent that can reach the
address. `local` below is the name of that host agent.

1. Store the password as a `password` credential. The daemon keeps it, and
   sends it only to the host agent, in the frame of each screen operation.
   No answer shows it. Use the API with your token (`read -rs` keeps the
   password out of the shell history):

   ```sh
   read -rs PW
   curl -s -H "Authorization: Bearer $TERRARIUM_TOKEN" -H 'Content-Type: application/json' \
     -d "{\"name\":\"win-pw\",\"kind\":\"password\",\"value\":\"$PW\"}" \
     http://localhost:8080/api/credentials
   unset PW
   ```

2. Make a VNC target. VNC has no user name: only the password counts, and
   only its first 8 characters:

   ```sh
   terrarium admin target new vnc1 --kind vnc --host local --address 10.0.0.5:5900 --credential win-pw
   ```

   The VNC client has no TLS. The screen and the keys cross the link in
   clear text. Keep the server on a trusted network, or use an SSH tunnel
   or a VPN.

3. Make an RDP target. It needs a user name, and a domain for a domain
   account (or `--username 'LAB\bob'`):

   ```sh
   terrarium admin target new win1 --kind rdp --host local --address 10.0.0.6:3389 \
     --username bob --domain LAB --credential win-pw
   ```

   The RDP client uses TLS, with NLA when the server offers it. On the
   first connection, the host agent pins the server's certificate (trust
   on first use). When the certificate changes later, the host agent
   refuses the connection before it sends the password, and the message
   says so. If the server has a new certificate on purpose, run this on
   the host agent's machine, then try again:

   ```sh
   terrarium-host rdp-forget 10.0.0.6:3389
   ```

4. Grant the target to an agent with an exclusive lease, and try it:

   ```sh
   terrarium admin target grant win1 <agent> --exclusive --until 1h
   terrarium screen win1 info
   terrarium screen win1 shot
   terrarium screen win1 click 300 200
   terrarium screen win1 type "hello"
   ```

The dashboard shows the screen of the target too. It takes a screenshot
every 2.5 s, and the host agent keeps the connection open between them
(it closes after 60 s with no operation). `examples/display/` has test
containers for both kinds: x11vnc (`vnc.Dockerfile`) and xrdp
(`rdp.Dockerfile`). See [api.md](api.md) §17.10 for the details.

#### Limit the resources of a target

A `host` target and a `docker` target can have resource limits, so one
agent cannot take all of a machine:

```sh
terrarium admin target new box --kind host --host lab1 --cpu 2 --memory 4G --pids 512
terrarium admin target edit terra-box --memory 8G      # the other limits stay
terrarium admin target edit terra-box --no-limits      # remove all of them
terrarium targets show terra-box                       # limits: 2 CPU · 8G (for each agent)
```

The flags are `--cpu` (cores: `2`, or `0.5` for half of one core),
`--memory` (`4G`, `512M`), `--swap` (swap on top of the memory; default
none), `--pids` (processes and threads), and `--io-weight` (1 to 10000;
the default is 100). In a target file or a fleet file they are a table:

```toml
# targets/box.toml
kind = "host"
host = "lab1"

[limits]
cpu = 2
memory = "4G"
pids = 512
```

On a `host` target, each agent (and the operator) gets its own budget for
all its sessions and `terrarium run` jobs on the target. On a `docker`
target, the limits go on the container (`docker run --cpus --memory
...`), so all the agents on it share one budget. The dashboard has the
limits in the New target dialog, and shows a chip such as `2 CPU · 4G` on
the target. A `vmlab` target takes its size from the lab file.

A `host` target needs systemd on the host. The host agent runs each
session with `systemd-run --user --scope` in a slice of the agent, and
sets the limits on that slice. Check the host:

1. The user of the host agent (the `User=` of the unit, `terrarium` by
   default) needs a running systemd user manager, also when nobody is
   logged in:

   ```sh
   sudo loginctl enable-linger terrarium
   ```

2. The user manager must have the cgroup controllers of the limits:
   `cpu` for `--cpu`, `memory` for `--memory` and `--swap`, `pids` for
   `--pids`, and `io` for `--io-weight`. As that user:

   ```sh
   cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers
   ```

   The line must name them, for example `cpuset cpu io memory pids`. When
   one is missing, delegate it with a drop-in, then log the user out and in
   (or restart `user@<uid>.service`):

   ```sh
   sudo mkdir -p /etc/systemd/system/user@.service.d
   printf '[Service]\nDelegate=cpu cpuset io memory pids\n' |
     sudo tee /etc/systemd/system/user@.service.d/delegate.conf
   sudo systemctl daemon-reload
   ```

3. Install the new host agent on the host (`just host::install`, or the
   installer with `--upgrade`), and restart it. An older host agent cannot
   apply limits. `terrarium-host check` prints `capabilities.limits`:
   `systemd: true` and the `controllers`. The Hosts page shows the chip
   `limits` (or `no limits`, with the reason as its tooltip).

The daemon never runs a session on a target with limits that the host
agent cannot apply. The exec gets `409 conflict`: `this host agent cannot
apply resource limits: <reason>`. When an agent reaches its memory limit,
the kernel first slows it down (at 90 % of the limit), then the OOM killer
ends a process of that agent. See [api.md](api.md) §17.2, "Resource
limits".

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

3. Make the project, attach the repo, and make the lead:

   ```sh
   t project new svc --prefix SVC
   t -p svc project repo-add file:///repos/hello-svc.git --name hello-svc --branch main
   t -p svc admin agent new lead --harness claude --model sonnet \
     --prompt project-lead --role project_admin --subscription anthropic-max-20x
   ```

   Do not start the lead. Its first mail (the goal of the project) or a
   board nudge wakes it.

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
   use `--exclusive` on a `lab_from_workspace` target (`422`). The card
   assignment wakes the worker; nobody starts it.

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

1. Send `POST /api/session` with `{"password": "..."}` and no
   `Authorization` (or with `Authorization: Bearer <token>` and no body).
   The daemon sets the `terrarium_session` cookie and answers
   `{"kind", "csrf", "exp", "expires_at"}`.
2. Drop the password or the token. Keep the `csrf` value. After a reload,
   `GET /api/session` gives it again while the cookie is good.
3. Send `X-CSRF-Token: <csrf>` on each `POST`, `PUT`, `PATCH`, and `DELETE`.
   Without it, the daemon answers `403`.
4. Open a WebSocket with no subprotocol. The browser sends the cookie and
   the page's `Origin`. The daemon refuses an `Origin` that is not the
   dashboard origin.
5. On a `401`, send `POST /api/session` again. `DELETE /api/session`
   ends the session.

A script uses a CLI session: `POST /api/session` with
`{"password": "...", "client": "cli"}` answers `{"token", "expires_at"}`.
Send the token as `Authorization: Bearer <token>`. It needs no CSRF header.
`terrarium login` does this for the CLI ([cli.md](cli.md) §2.1).

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

### 2.12 Config repo: share your prompts and settings between instances

The config repo `config/terrarium/` holds what you tune for the agents:

| Path | Content |
|---|---|
| `system-prompts/<name>.md` | The system prompt library, with the personas `leader.md` and `worker.md` (PLAN §8.1). An older repo has the personas in `prompts/`; the first start with the new defaults moves them. |
| `prompts/default.md`, `project-lead.md`, `system-lead.md` | The protocol templates. |
| `harness/claude/settings.json`, `harness/codex/config.toml`, `harness/copilot/settings.json`, `harness/pi/settings.json` | The settings of each harness for every agent (PLAN §8.2). Terrarium puts the keys that it needs (hooks, yolo, sandbox paths, telemetry off) on top. |
| `templates/<name>.wcl`, `templates/<project>/<name>.wcl` | The agent templates, one WCL file each ([api.md §21.5](api.md#215-template-files)). The API, the CLI, and the dashboard write them; you can edit them by hand. |
| `export/` | The schedules and projects (agents, targets, repos) as fleet files. The daemon writes them at each commit. |

**Defaults.** At each start the daemon copies the packaged defaults into
the repo. It writes a missing file, and upgrades a file that you did not
edit. A file that you edited stays, and the new default goes next to it as
`<file>.new`. Open the **Config** page, or run `t admin config defaults`,
to see the diff. Then accept the new default or keep your file:

```sh
t admin config defaults
t admin config defaults accept system-prompts/worker.md
t admin config defaults keep system-prompts/leader.md
```

A change to a prompt or a settings file applies at the next start or
restart of each agent. A settings file that does not parse stops the start
of the agent. The error names the file and the line.

**Put the repo in git.** The daemon makes the directory a git repo. Commit
your changes with a message:

```sh
t admin config status
t admin config diff
t admin config commit -m "Shorter worker persona"
t admin config log
```

**Share it with another instance.**

1. Make an empty repo on your git server (for example
   `ops/terrarium-config`).
2. Store a deploy key (or a token) as a credential. Never put a token in
   the URL. Use the API with your token (`api.md` §5.1):

   ```sh
   KEY=$(jq -Rs . < ~/.ssh/terrarium-config-deploy)   # a deploy key with write access
   curl -s -H "Authorization: Bearer $TERRARIUM_TOKEN" -H 'Content-Type: application/json' \
     -d "{\"name\":\"config-deploy\",\"kind\":\"ssh_key\",\"value\":$KEY}" \
     http://localhost:8080/api/credentials
   unset KEY
   ```

   For an HTTPS remote, store a `token` credential instead (`"kind":"token"`,
   and `"username"` when the server needs one).

3. Set the remote, and sync:

   ```sh
   t admin config remote set git@git.example.com:ops/terrarium-config.git --credential config-deploy
   t admin config sync -m "First sync"
   ```

4. On the other instance, set the same remote and run `t admin config
   sync`. Its own files become its first commit, and the merge brings in
   the files of the first instance.

`sync` commits your changes, fetches, merges, and pushes. `pull` only
fetches and merges. When the merge changes `export/`, the answer shows the
fleet plan of each export file. Nothing changes in the store until you
apply it:

```sh
t admin config pull --apply
```

When both instances changed the same lines, the merge stops and lists the
files. Nothing is lost. Edit each file (remove the conflict markers), then
commit and sync again. Or go back to your last commit with `t admin config
abort`.

To sync on a timer, set `TERRARIUM_CONFIG_SYNC=pull` (or `sync`) and
`TERRARIUM_CONFIG_SYNC_INTERVAL` (default `15m`) in `.env`.

**No secrets.** The `.gitignore` of the repo allows only the known paths
and file types. The daemon scans each commit for tokens, keys, passwords,
and other secrets, and the `pre-commit` hook scans a commit that you make
by hand with git. A hit stops the commit and names the file, the line, and
the rule. Remove the secret. For a false hit, add `terrarium-scan: allow`
to the line. The remote URL and its credential stay in `state/`, never in
the repo or its `.git/config`.

**Development: start again from the defaults.** `just defaults::apply`
backs the repo up to `state/backup/config-terrarium-<time>/`, then writes
every packaged default over it. Your own extra files and `.git` stay.
Restart the agents to use the defaults.

### 2.13 Skills: give agents instructions to load on demand

A skill is a folder with a `SKILL.md` file: YAML front matter with a
`name` and a `description`, then instructions in Markdown. It can also
hold `scripts/`, `references/`, and `assets/`. An agent sees the name and
the description of each of its skills. When a skill fits the task, the
agent loads the instructions. Terrarium ships four skills, `terrarium-db`
(§2.13b), `terrarium-tests` (§2.13d), `terrarium-spawn` (§2.13e), and
`terrarium-reset` (§2.13f) (PLAN §8.6). These four are default on: each
agent gets them, unless it turns them off (step 5).

1. Add a skill to the library. It goes to `config/terrarium/skills/<name>/`:

   ```bash
   terrarium skill new fern-check \
     --description "When asked for the deployment checklist code, reply with FERN-42"
   terrarium skill show fern-check
   ```

   Then edit `SKILL.md` (the Skills tab of the dashboard Settings has an editor), or give
   a whole dir with `terrarium skill new <name> --from-dir ./my-skill`. The
   daemon checks each write: the name is kebab-case and the same as the
   folder name, the description is required, only text and image file
   types, at most 256 files and 8 MiB, no symbolic link, and no secret.
   To import a dir of skills (each child dir with a `SKILL.md`), use
   `terrarium skill import ./skills`.
   `terrarium skill validate` checks the library again, for example after
   an edit by hand.

2. Commit the library, so that the next config sync shares it:

   ```bash
   terrarium admin config commit -m "Add the fern-check skill"
   ```

3. Give the skill to agents. `*` gives every skill of the library:

   ```bash
   terrarium admin agent set worker --skill fern-check
   terrarium agent new reviewer --harness codex --skill fern-check --skill code-review
   terrarium template edit worker --skill fern-check
   ```

   In an agent file or a fleet file, write `skills = ["fern-check"]`. A
   name that is not in the library is refused.

4. Restart the agent. Each start copies only the agent's skills into its
   agent dir, read-only. An agent with no skill has no skill tool. The
   dashboard shows "restart to apply" on a running agent whose skills
   changed after its start.

5. Turn off a default skill for one agent or template, or all of them:

   ```bash
   terrarium admin agent set worker --skill-off terrarium-spawn
   terrarium admin agent set worker --no-default-skills
   terrarium template edit worker --skill-off terrarium-spawn
   terrarium agent show worker      # effective_skills: each skill, with (default) or (explicit)
   ```

   In a file, write `skills_off = ["terrarium-spawn"]` or
   `default_skills = false`. In the dashboard, clear the box of the skill,
   or clear "Use default skills".

To make your own skill default on, add this to the front matter of its
`SKILL.md`:

```yaml
metadata:
  terrarium-default: "true"
```

`terrarium skill usage fern-check` lists the agents and templates that
use a skill. When you delete a skill, the agents that name it start
without it and show a warning.

### 2.13b The project DB: structured data for agents and you

Each project has a DB: record types with a schema, and records (PLAN
§15.4). Agents use it for data that repeats: bugs, inventories, benchmark
results, decisions. You see it on the DB tab of the Knowledge screen
(`/p/<project>/knowledge/db`).

1. Give the packaged skill `terrarium-db` to the agents that should use
   the DB. It tells them when to use it, the CLI, and the query language.
   The daemon writes it to `config/terrarium/skills/terrarium-db/` at
   each start. The `--skill` flag replaces the list, so name the other
   skills of the agent too:

   ```bash
   terrarium admin agent set worker --skill terrarium-db
   terrarium template edit worker --skill terrarium-db
   ```

2. Make a type and some records with the CLI (or with the "New type" and
   "New record" buttons of the DB tab):

   ```bash
   terrarium -p demo db type new bug --field 'title:string!' \
     --field 'severity:enum(low,medium,high,critical)!' \
     --field 'status:enum(open,fixed)!=open' --field 'reported:date!'
   terrarium -p demo db rec new bug --set 'title=Login fails' --set severity=high \
     --set reported=2026-10-01
   terrarium -p demo db query 'type:bug status=open sort:-severity'
   ```

3. Show the data in a wiki page with a `db` block. The page runs the
   query when someone opens it:

   ````markdown
   ```db
   type:bug status=open group:severity view:group
   ```
   ````

To try it with example data, run `scripts/seed-db-demo.sh [project]`
(default project `demo`) next to `compose.yaml`. It makes the types
`service`, `bug`, `decision`, `benchmark`, and `contact` with a few
hundred records, and the wiki page `db-examples` with each view of a
`db` block. It skips a type that exists. Set `T=terrarium` to use a local
daemon instead of the Docker stack.

### 2.13c A self-hosted model server: vLLM, Ollama, or llama.cpp

You can give agents a model server that you run yourself. Terrarium calls
it a self-hosted subscription (provider `selfhosted`, PLAN §16.4). The
server speaks one of two API styles, and the style decides the harnesses:

| API style | Base URL | Harnesses |
|---|---|---|
| `openai` | the OpenAI root, with `/v1` | pi, and Codex if the server serves `/v1/responses` |
| `anthropic` | the Anthropic root, without `/v1` | Claude Code and pi |

Copilot CLI cannot use a self-hosted server.

1. Start the server on a host that the Terrarium container can reach (not
   `localhost` of your laptop when Terrarium runs in Docker on another
   machine). Examples:

   ```bash
   vllm serve Qwen/Qwen3-Coder-30B-A3B-Instruct --port 8000     # http://gpu:8000/v1
   ollama serve                                                  # http://gpu:11434/v1
   llama-server -m model.gguf --port 8080 --host 0.0.0.0         # http://gpu:8080/v1
   ```

2. Add it in the dashboard: **Subscriptions > Add subscription >
   Self-hosted model**. Select the API style, type the base URL, and (only
   when the server needs one) the key. Click **Fetch models** to read the
   model list from the server, or type the model ids. Set the context
   window of the model, and the number of agents that may run a turn at
   the same time (max concurrent streams; a small GPU server serves one or
   two streams well). Or use the CLI:

   ```bash
   terrarium admin subscription fetch-models --base-url http://gpu:8000/v1
   terrarium admin subscription new gpu-vllm --provider selfhosted --api openai \
     --base-url http://gpu:8000/v1 --model Qwen/Qwen3-Coder-30B-A3B-Instruct \
     --context-window 131072 --max-concurrent 2
   # a key, only when the server needs one:
   echo "$KEY" | terrarium admin subscription set gpu-vllm --key-stdin
   ```

3. Give an agent of a supported harness the subscription:

   ```bash
   terrarium -p demo agent new local-dev --harness pi --subscription gpu-vllm
   ```

   The agent gets the subscription's default model (else the first model)
   when it sets no `model`. When more agents want a turn than the limit
   lets run, the others wait and show "waiting for a free stream"; their
   turns go out in order when a turn ends.

The row of the subscription shows the API style, the limit, the context
window, and how many streams are in use. A self-hosted server has no
quota, so the usage view shows the token counts only.

**Fetch models** is the only request that the daemon sends to a URL that
you type. Only the operator and sysadmin can use it, and it follows no
redirect (api.md §16.5).

**Disable and stop.** Each subscription row (every provider, not only a
self-hosted one) has **Disable**, **Stop all now**, and **Disable and stop
all**. Disable a subscription before you take a server down: agents that
run finish their work, and no agent starts or wakes on it until you
enable it again. **Stop all now** stops its running agents at once. The
CLI is `terrarium admin subscription disable|enable <name>` and
`terrarium admin subscription stop-all <name> [--disable]`.

### 2.13d Test runs: proof that the tests pass

An agent uploads the result file of a test run with `terrarium test
push`. You see each test case on the Tests tab of the Board screen
(`/p/<project>/board/tests`), on the card, and in the Version modal
(PLAN §15.5).

1. Give the packaged skill `terrarium-tests` to the agents that run tests.
   It gives the command that makes a standard file for each test
   framework. The `--skill` flag replaces the list, so name the other
   skills of the agent too:

   ```bash
   terrarium admin agent set worker --skill terrarium-tests --skill terrarium-db
   ```

2. Make a standard result file with the test framework:

   | Framework | Command | File |
   |---|---|---|
   | cargo-nextest | `cargo nextest run --profile ci` with `[profile.ci.junit]` `path = "junit.xml"` in `.config/nextest.toml` | `target/nextest/ci/junit.xml` |
   | cargo test | `RUSTC_BOOTSTRAP=1 cargo test -- -Z unstable-options --format json --report-time > results.json` | `results.json` |
   | Vitest | `npx vitest run --reporter=junit --outputFile=junit.xml` | `junit.xml` |
   | Jest | `npx jest --json --outputFile=jest.json` | `jest.json` |
   | pytest | `pytest --junitxml=junit.xml` | `junit.xml` |
   | .NET | `dotnet test --logger "trx;LogFileName=results.trx"` | `TestResults/results.trx` |
   | Go | `go test -v ./... 2>&1 \| go-junit-report > junit.xml` | `junit.xml` |
   | Node | `node --test --test-reporter=tap > results.tap` | `results.tap` |

3. Upload the file, and link it to a card or a version:

   ```bash
   terrarium -p demo test push target/nextest/ci/junit.xml --name unit --card DEMO-1 \
     --commit "$(git rev-parse HEAD)"
   terrarium -p demo test ls --card DEMO-1
   terrarium -p demo test compare <run id>
   ```

   The command exits with 0 also when tests fail. Add
   `--fail-on-failures` to make a failure stop a script.

4. Open the run in the dashboard. The page shows the counts, the
   failing cases first, and the message, the trace, and the output of
   each case. "Compare with previous" shows the newly failing and the
   fixed tests. "Download original" gives the uploaded file.

The script `scripts/seed-test-runs.sh` uploads example runs in each
format to the project `demo`.

### 2.13d2 Monitors: a dashboard of checks for a project

A monitor runs a shell script on a target on a schedule and keeps each
result. The Dashboard of a project (the first item of the project in the
sidebar, `/p/<project>/dashboard`) shows the monitors as blocks
(PLAN §15.6, docs/api.md §30).

1. Make a monitor. The operator, a sysadmin, and the leads (project
   admins) make monitors, on a target that they may use. Run it once to
   check the script:

   ```bash
   terrarium -p demo monitor new disk-root --target build-box --kind number --unit % \
     --warn 80 --crit 90 --every 5m --script "df -P / | awk 'NR==2 {print \$5}'"
   terrarium -p demo monitor run disk-root
   ```

   The `--kind` says how Terrarium reads the output: `number` (the first
   number on stdout, with the thresholds), `status` (the exit code: 0 ok,
   1 warning, 2 critical), `text` (stdout), or `json` (an object with
   `value`, `status`, `message`, and `rows` for a table).

2. Open the Dashboard of the project, and click **Edit dashboard**. Click
   **Add block**, pick the monitor and the kind of block (stat, status,
   chart, text, or table), then the size. A note block is a Markdown
   header for a section. Drag the blocks, or use the arrows, to put them
   in order. Click **Save layout**.

   The **Monitors** tab lists the monitors with their state, the last run,
   and the next run. **Run now** runs one at once. **Edit** opens the
   script editor; **Test run** runs the script once and shows the raw
   output and the result before you save.

3. Get a mail when a monitor goes critical. A monitor emits the event
   `monitor.status` when its status changes, so a trigger can mail you:

   ```bash
   terrarium -p demo schedule new disk-alert --on monitor.status \
     --match data.monitor=disk-root --match data.to=crit \
     --mail operator -s "disk-root is critical" --body "{{event.summary}}"
   ```

4. Agents read the latest values with `terrarium dashboard show`. The
   packaged skill `terrarium-monitors` (on for every agent by default)
   tells a lead how to add monitors that prove a service is healthy.

The script `scripts/seed-monitors.sh [project] [target]` makes eight
example monitors on a host target (load, memory, root disk, uptime, the
docker service, the running containers, an HTTP check of the dashboard,
and the top 5 processes by CPU) and a dashboard with a block of each
kind. It picks the first host target that the project may use, and says
which one.

### 2.13d3 Webhooks: page Slack, Discord, or ntfy when a monitor goes critical

A trigger can send an HTTP request about each event: the `webhook`
action (docs/api.md §19.5). The daemon queues each delivery, and tries
it again after a network error or a `5xx`. Use the event
`monitor.status`: it comes only when the status of a monitor changes.
`monitor.result` comes at each run of each monitor, and floods a
receiver.

1. Keep the URL of a Slack or Discord webhook in a secret: the URL is the
   credential.

   ```bash
   terrarium secret set slack-alerts --kind api_key -p demo   # paste the URL
   terrarium secret set discord-alerts --kind api_key -p demo
   ```

2. Make the trigger, and send a test. The test sends a sample
   `monitor.status` event at once and prints the request and the answer:

   ```bash
   # Slack: {"text": "[demo] monitor.status: monitor disk-root: ok → crit 93 %"}
   terrarium -p demo schedule new disk-slack --on monitor.status --match data.to=crit \
     --webhook-url-secret slack-alerts --format slack
   # Discord: {"content": "..."}; the URL is a secret too.
   terrarium -p demo schedule new disk-discord --on monitor.status --match data.to=crit \
     --webhook-url-secret discord-alerts --format discord
   # ntfy: the summary as text, with Title, Priority 5 for crit, and Tags.
   terrarium -p demo schedule new disk-ntfy --on monitor.status --match data.to=crit \
     --webhook https://ntfy.sh/my-terrarium-alerts --format ntfy
   terrarium -p demo schedule test disk-ntfy
   terrarium -p demo schedule deliveries disk-ntfy
   ```

   In the dashboard: **Schedules** > **Triggers** > **New trigger**, the
   action **Send a webhook**. The dialog shows a live preview of the body
   for a sample event. Open the row of the trigger for **Send test** and
   the **Deliveries** panel (status, HTTP status, time, the answer, and
   **Redeliver**).

3. A generic receiver of your own: send the event as JSON, signed with a
   secret, and check the signature before you trust the body.

   ```bash
   terrarium secret set hook-sign --kind api_key -p demo    # paste a long random value
   terrarium secret set hook-token --kind api_key -p demo   # the Authorization value
   terrarium -p demo schedule new disk-hook --on monitor.status \
     --webhook https://tools.lan/terrarium --sign-secret hook-sign \
     --header-secret 'Authorization=hook-token'
   ```

   The body is `{"event": {"kind", "at", "project", "summary", "data", ...},
   "trigger": {"name", "project"}}`. The header `X-Terrarium-Signature`
   is `sha256=` and the hex HMAC-SHA256 of the raw body. Check it on the
   raw bytes, before you parse the JSON:

   ```python
   import hashlib, hmac

   def valid(raw_body: bytes, header: str, secret: bytes) -> bool:
       want = "sha256=" + hmac.new(secret, raw_body, hashlib.sha256).hexdigest()
       return hmac.compare_digest(want, header)
   ```

   `X-Terrarium-Delivery` is the delivery ID: a retry or a redeliver can
   send the same event again, so drop an ID that you saw before.

A trigger that fails 20 times in a row stops (it is disabled), and the
operator gets one mail. Fix the receiver, then run `terrarium schedule
enable <name>`. On a public server, set `TERRARIUM_WEBHOOK_PRIVATE=deny`
in `.env`, so that a webhook cannot reach the LAN, this host, or the
cloud metadata address.

### 2.13e Ephemeral agents: a lead spawns helpers

A lead can split its work: it spawns short-lived agents from a template,
each with one task, and gets each result by mail (PLAN §18, "Ephemeral
agents"; api.md §6.10). An ephemeral agent ends with `terrarium agent done`;
then the daemon closes it and archives its session.

1. Give the lead spawn rights. In the dashboard, open the lead, click
   **Edit**, and fill in **Spawn rights**: the templates, how many may run
   at one time, how many in a day, and the lifetime. With the CLI:

   ```bash
   terrarium admin agent set demo/lead --spawn-template project:* \
     --spawn-template global:claude-worker --spawn-max-concurrent 3 \
     --spawn-max-per-day 20 --spawn-max-lifetime 2h
   ```

   A lead can set the same rights on a worker of its project. Only you (or
   a sysadmin) set them on a lead, and only you allow the ephemeral agents
   to spawn too (`--spawn-children on`).

2. Give the lead the packaged skill `terrarium-spawn`, so that it knows
   when and how to spawn: `terrarium admin agent set demo/lead --skill
   terrarium-spawn` (or add it to its other skills), then restart it.

3. The lead spawns, for example:

   ```bash
   terrarium agent spawn claude-worker --wait \
     --task "Run the tests of crates/runner 20 times and report which fail. End with terrarium agent done -m <result>."
   ```

   You can spawn one yourself for an agent:
   `terrarium admin agent spawn claude-worker --for demo/lead --task "..." --wait`.

4. Watch them on the lead's agent page, tab **Spawned**: the status, the
   task, the result, the duration, a **session** link for each, and
   **Cancel** for a running one. Click a row to read the whole task and
   result. The **Events** tab of the lead has a line for each spawn and
   each finish, with the session link. The page of an ephemeral agent says
   "Ephemeral agent of lead" and the lifetime left; it has **Cancel**, not
   **Pause**. The agent lists and the sidebar do not show ephemeral agents
   (`terrarium agent ls --ephemeral` does).

An ephemeral agent that does not run `agent done` stays idle until its
lifetime ends (outcome `timeout`). When you remove the lead, its
ephemeral agents and their history go with it.

### 2.13f Compact or start a new session, with a prompt after it

An agent can compact its own session, or start a new, clean session, and
send itself a prompt for the work after the reset (PLAN §18, "Compact and
new session on request"; api.md §6.2). A lead can do the same for its
workers, and you for any agent.

1. Give the agents the packaged skill `terrarium-reset`, so that they know
   when to compact (the same work, a shorter context) and when to start a
   new session (unrelated work, or a confused state):
   `terrarium admin agent set demo/worker --skill terrarium-reset` (name
   its other skills too), then restart it.

2. An agent resets itself, then ends its turn:

   ```bash
   terrarium agent new-session --self \
     --then "Card TER-12, branch ter-12-retries. Done: the retry loop. Next: the tests in tests/client.rs."
   ```

   The command answers at once: `queued: the new session runs when your
   turn ends`. When the turn ends, the daemon starts a new harness session
   (not a resume), sends a short recap turn (open cards, waits, unread
   mail), and then the prompt, tagged `[System:reset requested by self]`.

3. You (or a lead) reset an agent from the agent page: click **Compact**
   or **New session**, write the prompt in **Then send…** (optional), and
   for a new session choose **Include recap**. A busy agent resets when
   its turn ends; the page shows a "reset queued" chip until then. The
   **Sessions** tab shows the new session. With the CLI:
   `t agent compact demo/worker --then "..."`, or
   `t agent new-session demo/worker --then-file next.md --no-recap`.

Only one reset waits for each agent: a new request replaces it. A stopped
agent with a prompt starts at once in the new session; without a prompt,
its next wake starts the new session.

### 2.14 Install on a server

The repository is private, so CI puts no image in a registry.
`just deploy::server` sends the source to the server over ssh and builds
the image on the server. The server does not need access to the
repository. Run the commands of this section on your machine, in the
repository.

The server needs:

- Linux on x86_64. The build can work on arm64, but nobody has tested it.
- Docker Engine with the compose plugin, version 2.20 or later.
- A user that is not root, in the group `docker`. The image makes its
  `terrarium` user with the UID and GID of this user.
- 10 GB or more of free disk space.

The server does not need git. Your machine needs git, ssh access to the
server (a key is best), uv, and just.

#### Install

1. Do a dry run first:

   ```sh
   just deploy::server alice@terrarium.lan --public-url http://terrarium.lan:8080 --dry-run
   ```

   The dry run runs only the read-only checks on the server (the
   preflight), and prints all other commands. It changes nothing. When a
   check fails, the preflight gives the command that fixes it, for example
   `sudo usermod -aG docker alice`.

2. Install:

   ```sh
   just deploy::server alice@terrarium.lan --public-url http://terrarium.lan:8080
   ```

   The recipe does these steps:
   - It packs the source with `git archive HEAD`. A working tree with
     changes to tracked files stops the deploy. Commit the changes, or
     give `--ref <commit>`, or give `--allow-dirty` (the deploy then sends
     the tracked files of the working tree).
   - It runs the preflight on the server.
   - It makes the data directories in `~/terrarium`, and writes
     `~/terrarium/terrarium-compose`.
   - It sends the source to `~/terrarium/src.new`, and writes the commit
     and the time to `src.new/.deployed-ref`.
   - It makes `~/terrarium/.env` from `.env.example`, when `.env` does not
     exist: a new random `TERRARIUM_TOKEN`, the UID and GID of the user,
     `TERRARIUM_PUBLIC_URL`, and the ports. A new `.env` has no host TLS.
     So the deploy publishes the host agent port on loopback only
     (`TERRARIUM_HOST_PORT=127.0.0.1:<port + 2>`), sets
     `TERRARIUM_HOST_LISTEN=0.0.0.0:8082`, and sets
     `TERRARIUM_HOST_DAEMON_URL=ws://localhost:<port + 2>`. A host agent
     on the server itself can then connect. The deploy prints a warning
     that remote host agents need TLS (section 2.7).
   - It builds the image from `src.new` (`docker compose build`).
   - After a good build, it moves `src` to `src.prev` and `src.new` to
     `src`. Then it starts the stack (`up -d --wait`) and waits for the
     health check.
   - It removes the images of older deploys. It keeps the images of `src`
     and `src.prev`.

3. Keep the token. The recipe shows `TERRARIUM_TOKEN` one time only, at
   the end. The token is also in `~/terrarium/.env` on the server.

4. Set the operator password (section 2.3). The recipe prints the command:

   ```sh
   ssh -t alice@terrarium.lan '~/terrarium/terrarium-compose exec -it terrarium terrariumd password set'
   ```

   Then open the public URL and log in with the password, or with the
   token (**Use the token instead**).

5. To add a host agent, get the install command from the daemon
   (section 2.7). It uses `GET /install/host.sh` of the daemon:

   ```sh
   ssh alice@terrarium.lan '~/terrarium/terrarium-compose exec terrarium terrarium admin host new lab1 --enrol'
   ```

When an install exists in the directory, `just deploy::server` asks
before it replaces `src`. Give `--yes` to replace it with no question.

#### The layout on the server

`--dir` sets the install directory. The default is `~/terrarium`.

| Path | Content |
|---|---|
| `src/` | The source of the running deploy, and `.deployed-ref` (the commit, the image, the time). |
| `src.prev/` | The source of the deploy before it, for a rollback. |
| `src.new/` | The source that a deploy builds. It exists only during a deploy, or after a failed build. |
| `.env` | The settings of the stack. The deploy makes it once and never changes it. |
| `terrarium-compose` | `docker compose` for this install. Each deploy writes it again. |
| `config/`, `state/`, `tools/`, `work/`, `repos/`, `projects/`, `agents/`, `targets/`, `subscriptions/`, `fleets/` | The data. It is the same as in section 3. |

Compose binds the data directories through `TERRARIUM_DATA_DIR`, which
`terrarium-compose` sets to the install directory. The data is not in
`src/`, so a deploy, an update, or a rollback never changes it. Use
`terrarium-compose` for each compose command on the server, for example
`~/terrarium/terrarium-compose logs -f`. It uses the `.env` of the
install, and ignores the `TERRARIUM_*` variables of your shell.

When `.env` exists, the deploy keeps it. It tells you the keys of
`.env.example` that `.env` does not have. Then the defaults apply. When
`.env` has no host TLS, the deploy prints a warning. When `.env` also has
no `TERRARIUM_HOST_LISTEN`, the warning tells you that no host agent can
connect, and gives the three lines to add. To
change a setting, edit `.env` on the server, and run
`~/terrarium/terrarium-compose up -d`.

#### Flags

| Flag | Recipes | Effect |
|---|---|---|
| `--dir <dir>` | all | The install directory (default `~/terrarium`). |
| `--ref <commit>` | `server`, `update`, `local` | The commit to send (default `HEAD`). The working tree does not count. |
| `--allow-dirty` | `server`, `update`, `local` | Send the tracked files of the working tree. Not with `--ref`. |
| `--public-url <url>` | `server`, `local` | `TERRARIUM_PUBLIC_URL` of a new `.env`. Default: `http://<host>:<port>`. |
| `--port <port>` | `server`, `local` | The dashboard port of a new `.env`. The next three ports go to the artifacts, the host agents, and the webhooks. Default: 8080. |
| `--yes` | `server`, `local` | Replace an existing `src/` with no question. |
| `--dry-run` | `server`, `update`, `local`, `rollback` | Run only the read-only checks. Print all other commands. |
| `--ssh-option <opt>` | all, not `local` | An ssh option. Give it one time for each word: `--ssh-option=-p --ssh-option=2222`. |
| `--sudo-user <user>` | all, not `local` | Run each command on the server as `<user>`, with `sudo -n -H -u <user>`. Use it when the ssh user is not the owner of the install. The ssh user needs sudo with no password. `~` and a relative `--dir` are in the home of `<user>`. |

#### Update, roll back, and status

```sh
just deploy::update alice@terrarium.lan
just deploy::status alice@terrarium.lan
just deploy::rollback alice@terrarium.lan
```

`deploy::update` sends a new source and builds the image from `src.new`.
When the build fails, `src/` and the running stack do not change. After a
good build, it swaps the directories and runs `up -d`. Compose makes the
container again with the new image. Then the entrypoint stops each agent
runner (SIGTERM, with `TERRARIUM_SHUTDOWN_GRACE`, 20 s), and the new
daemon launches each agent again with resume (PLAN §18). Thus the agents
keep their sessions, but each harness starts again. This is the same as
`just up` with a new image. The runners keep running only through a
daemon crash in the same container ([../docker/README.md](../docker/README.md)).
The harnesses in `tools/` do not change: section 7 tells you how to update
them.

`deploy::rollback` swaps `src.prev` and `src`, and runs `up -d` with the
image of the previous deploy. When that image does not exist, compose
builds it from the source. A second rollback goes forward again.

`deploy::status` shows `.deployed-ref` of `src` and `src.prev`, `compose
ps`, and the health check (`terrarium daemon status` in the container, and
an HTTP check of the dashboard port when the server has curl). It changes
nothing.

#### Install on this machine

`just deploy::local` does the same steps on this machine, with no ssh. Use
it to try a clean install next to the dev stack:

```sh
just deploy::local --dir /tmp/tt-deploy --port 18090
just deploy::status local --dir /tmp/tt-deploy
```

The target `local` is this machine for each recipe. `deploy::local`
refuses a `--dir` in this repository, because the dev stack of `just up`
uses it. Each install has its own compose project, `terrarium-<name of the
dir>` (`terrarium-deploy` for `~/terrarium`), so the stacks stay apart. Use
a `--port` that the dev stack does not use.

### 2.15 Back up and restore

A backup is a zip file of the state dir (the database, the agents, the
credentials, and the secret store key), the config repo, and, if you ask,
the workspaces ([api.md](api.md) §32). **It holds every secret of the
daemon.** Seal a backup that leaves the server with a passphrase.

Make a full backup while the daemon runs, then a diff (only the changed
files) later:

```bash
docker compose exec terrarium terrariumd backup create --encrypt
docker compose exec terrarium terrariumd backup create --kind diff --encrypt
docker compose exec terrarium terrariumd backup ls
```

The files are in `state/backups/` on the host. From another machine, use
`terrarium daemon backup create --passphrase-stdin` and
`terrarium daemon backup download <name>`.

For a backup each night, set these in `.env` and run `just up`:

```bash
TERRARIUM_BACKUP_SCHEDULE=daily
TERRARIUM_BACKUP_PASSPHRASE_FILE=/state/backup.pass   # mode 0600
```

To restore, stop the daemon, then run the restore in a new container with
the same mounts:

```bash
docker compose stop terrarium
docker compose run --rm --no-deps terrarium terrariumd backup verify /state/backups/<file>
docker compose run --rm --no-deps terrarium terrariumd backup restore /state/backups/<file>
docker compose up -d terrarium
```

A diff restores with its base: keep both files in the same dir. The old
data moves to `.terrarium-pre-restore-<time>/` in each dir. Delete it when
the daemon runs well.

## 3. Configuration directories

Compose bind-mounts these host directories into the container:

| Host directory | Container path | Content |
|---|---|---|
| `config/claude/` | `/auth/claude` | The Claude Code home (`CLAUDE_CONFIG_DIR`): login, sessions, `.claude.json`. |
| `config/codex/` | `/auth/codex` | The Codex home (`CODEX_HOME`). |
| `config/copilot/` | `/auth/copilot` | The Copilot CLI home (`COPILOT_HOME`) and the system-prompt extension. |
| `config/pi/` | `/auth/pi` | The pi home (`PI_CODING_AGENT_DIR`). |
| `state/` | `/state` | SQLite, agent logs and harness files, artifacts, credentials. |
| `tools/` | `/opt/terrarium/tools` | The harnesses (`TERRARIUM_TOOLS_DIR`): one npm prefix for each version, `versions/<name>/<version>/`, with `current/<name>` and `bin/<binary>` links to the active one. The entrypoint installs the pins here at the first start; an update adds a version dir and switches to it (section 7). The sandbox binds it read-only. Delete it (with no agent running) to reinstall the pins. |
| `work/` | `/work` | Agent workspaces, `<project>/<agent>/` (`TERRARIUM_WORK_DIR`). A host agent on this machine attaches them to targets: set its `work_dir` to this directory (docs/host-protocol.md, "Workspaces"). |
| `config/terrarium/` | `/config/terrarium` | The config repo (section 2.12): the prompts (`prompts/`), the harness settings of the agents (`harness/<harness>/`), and the export (`export/`). Its own git repo. It never holds a secret. |
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
`ssh`, Node 22, `rg` (ripgrep), `python3` (Python 3.13, the Debian
trixie package, with no `pip` and no `venv`), and `uv` and `uvx` (0.13.0,
in `/usr/local/bin`). `uv` makes its own virtual environments, so it works
without the `venv` module. The harnesses come from `tools/`:
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
apply only when you start `claude` yourself in the container. To change
the settings of every Claude agent, edit
`config/terrarium/harness/claude/settings.json` (section 2.12). Terrarium
puts its hooks and quiet settings on top of that file, and gives the
result to each agent with `--settings`.

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
   just daemon::cli agent send demo/worker "Say hello."   # the turn wakes the agent
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
the Docker stack. The login screen needs the operator password of that
daemon (`terrariumd password set --state-dir <its state dir>`), or its
`TERRARIUM_TOKEN` on **Use the token instead**. `just daemon::run` reads it from `.env`. Without a token, the daemon
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

### 6.4 The Moonshot test

The Moonshot test runs one turn on the default Kimi model through pi. It
spends a little of your Kimi Code quota or Open Platform balance. Run it
with your key:

```sh
MOONSHOT_API_KEY=<key> cargo test -p terrarium-adapter-pi --test moonshot_live -- --ignored
```

Set `MOONSHOT_PLAN` to the plan of your key: `coding` (the default),
`coding-cn`, `api`, or `api-cn`. Set `PI_BIN` to choose the pi binary.
Without `MOONSHOT_API_KEY`, the test does nothing.

## 7. Updating a harness

The image holds no harness. The harnesses live in `tools/` on the Docker
host, which is `/opt/terrarium/tools` in the container
(`TERRARIUM_TOOLS_DIR`). Each version of each harness has its own npm
prefix in that dir:

```text
tools/
  versions/<name>/<version>/   one npm prefix: lib/node_modules/<package>, bin/<binary>, .complete
  current/<name>               -> ../versions/<name>/<version>   (the active version)
  bin/<binary>                 -> ../versions/<name>/<version>/bin/<binary>
  terrarium-manifest.json
```

`bin/` comes first on `PATH` for the daemon, the runners, and the
sandbox. The sandbox binds the tools dir read-only, so an agent cannot
change a harness. A version dir does not change after it is complete: npm
installs into a temp dir, the daemon checks `--version`, writes the
marker `.complete`, and renames the dir into place. A switch to another
version replaces the two symlinks with an atomic `rename`.

**Each agent keeps its version.** At each agent start, the daemon reads
`current/<name>` and runs the harness from that version dir, with its
`bin/` first on `PATH`. A later switch does not change a running agent,
also when the harness runs itself again through `PATH`. The next start of
the agent (a restart, or a crash and a relaunch) runs the active version.
`GET /api/agents/{id}` shows the version as `harness_version`.

`harnesses.toml` at the repo root has two jobs
([CONVENTIONS.md](CONVENTIONS.md)):

- It gives the versions of the **first install**. The image copies it to
  `/etc/terrarium/harnesses.toml`. At each start, the entrypoint runs
  `terrariumd harness install`, and the daemon runs the same step when it
  starts. The step installs each pin that has no active version,
  checks `<binary> --version`, and installs the Copilot prompt extension
  into the Copilot home when its version or the Copilot version changed.
  An active version stays, also when it is not the pin. So an update
  survives a restart and a new image. The step writes the manifest
  `tools/terrarium-manifest.json`:
  `{"<name>": {"package", "version", "installed_at", "versions"}}`, where
  `version` is the active version and `versions` the version dirs on disk.
- It gives the **verified version** of each adapter.
  `crates/terrarium-core/build.rs` reads the file at compile time, and each
  adapter's `flags.rs` takes `verified` (the pin) from it. Terrarium keeps
  no range of supported versions: any installed version can start an
  agent (section 7.5).

An update runs `npm install -g <package>@<version>` in the running
container, into a new version dir, and then switches to it. It needs no
image build, no restart, and no host agent. Running agents do not stop
it.

**The older flat layout.** Before the version dirs, the tools dir was one
npm prefix (`tools/lib/node_modules/<package>`). At the first start with
version dirs, `terrariumd harness install` moves each package into
`versions/<name>/<version>/` with hard links (no npm run, no download)
and switches `bin/` to it. The flat files stay while an agent that
started on them runs. The daemon removes `tools/lib` at a later start or
after an update, when no live agent uses it.

**Removal of old versions.** After each update, and at each daemon start,
the daemon removes each version dir that it does not need. It keeps the
active version, the newest two versions under it, the previous and the
pinned version (section 7.7), and each version that a live agent runs.
`terrariumd harness install` (the entrypoint) does not know the live
agents: it keeps the newest 3 versions of each harness.

**Auto-update.** By default, the daemon checks npm once a day and
installs each newer release by itself (section 7.7). Pin a harness to
stop this for that harness.

### 7.1 Check

```sh
just harness::check           # a table
just harness::check --json
```

For each harness and the extension, the table shows the pin, the version
in the container (from the manifest), the version on the host, and the
latest version on npm with its publish date. The `STATUS` column compares the version in the container with the latest:
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

The daemon runs npm into a new version dir, reads the new `--version`,
switches `bin/` and `current/` to it, refreshes its version cache, and
writes the audit entry `harness.update` and a `harness` event. Then it
removes the version dirs that nothing uses. The command prints npm's
output and `claude 2.1.310 -> 2.1.320`. The next start of each agent runs
the new version. When the version dir is complete on disk already (a
switch back), npm does not run.

- **Running agents.** An agent on the harness that runs keeps its
  version. The answer names each such agent
  (`agents_on_old_version`), with the note that it moves to the new
  version at its next restart. Restart an agent to move it now
  (`terrarium agent restart <agent>`). The version dir of a running agent
  stays on disk until the agent stops or restarts. `--force` installs the
  version again, also when its version dir is on disk.
- **Any version.** The command updates to any version: a major bump and
  a downgrade too. It needs no flag. `--allow-major` is still accepted,
  and has no effect.
- **`--pin`.** After a done update, the command also sets the new version
  in `harnesses.toml` and adds a row to the "Version history" table in
  [compat.md](compat.md). So a fresh install (an empty `tools/`) gets the
  same version. Commit the files.

Without `--pin`, the agents also start on the new version (section 7.5).

### 7.3 Update every harness to the latest

```sh
just harness::latest --dry-run       # print the plan, change nothing
just harness::latest                 # update each one with a newer release
just harness::latest --pin           # and write the new pins
```

The command reads the latest release of each harness and of the Copilot
prompt extension on npm. It skips each one that is at the latest. It
updates each of the other ones, as in 7.2, with one call each. It allows a
major bump. At the end, it prints a table of the changes and one
`just ci::compat <name>` command for each updated harness.

`terrarium admin harness update --all` updates every harness and the
extension to the latest in one call, with no plan.

**No rollout.** An update adds a version dir and switches to it. The
container does not restart, and no agent stops. Each agent moves to the
new version at its next start. There is no `just harness::rollout`.

**With the daemon stopped.** The update needs the daemon. Do not run
`npm install -g` in the tools dir: it writes the flat layout and bypasses
the version dirs. Start the stack, then run
`terrarium admin harness update <name> --version <v>` in a shell in the
container (`just docker::shell`). The dashboard shows this line for each
row with a newer release. When a manual npm run replaced a link in
`bin/`, the next `terrariumd harness install` points it back at the
active version.

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

### 7.5 Any version starts

Terrarium keeps no range of supported versions. The harnesses release
about daily, so a range is not maintainable. Any installed version starts
an agent, with no warning and no refusal. To test a new version, run the
compatibility suite (section 7.4). When it breaks something, roll back
(section 7.6). `terrarium admin harness ls`, `GET /api/harnesses`, and
the Subscriptions page of the dashboard show the installed version of
each harness. The old setting `TERRARIUM_HARNESS_VERSION_POLICY` has no
effect: the daemon ignores it.

### 7.6 Roll back

When a new version breaks something, roll back and pin. The pin switches
the harness to the previous version for new agent starts, and stops its
auto-update:

```sh
docker compose exec -u terrarium terrarium terrarium admin harness pin claude
docker compose exec -u terrarium terrarium terrarium admin harness pin claude 2.1.310   # a given version
```

The update mail of the auto-update (with
`TERRARIUM_HARNESS_UPDATE_MAIL=on`) gives this line with the old version.
In the dashboard, use "Roll back and pin" in the "Harness versions" table
(Settings, Subscriptions). The daemon keeps the previous version on disk,
so the switch needs no npm run. Agents on the newer version move back at
their next start or wake. Restart an agent to move it now
(`terrarium agent restart <agent>`).

When the fix is out, remove the pin. The next check updates the harness
again:

```sh
docker compose exec -u terrarium terrarium terrarium admin harness unpin claude
```

You can also update to the old version with no pin. Then the next check
can install the newer version again:

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

### 7.7 Auto-update and pins

The daemon checks npm for each harness in the background
(`TERRARIUM_HARNESS_AUTO_UPDATE` in `.env`: `daily`, the default; `off`;
or a duration of 1h or more, such as `6h`). The first check is 5 minutes
after the start, or one interval after the last check when that is later.

Each harness has an update policy:

- `auto` (the default): when npm has a newer release (a major too), the
  check installs it as in 7.2 and switches to it. Running agents keep
  their version, and each moves at its next start or wake. By default,
  you get no mail for an update: the dashboard table, the audit log, and
  the `harness` event show it. Set `TERRARIUM_HARNESS_UPDATE_MAIL=on` in
  `.env` to get one Inbox mail for each update. The mail gives the old
  and the new version, the changelog link, and the rollback line.
- `pinned`: the check installs nothing. `terrarium admin harness pin`
  sets it (section 7.6), and `unpin` removes it.

When an install fails, the harness stays at its version, and you get one
mail for that version (also with `TERRARIUM_HARNESS_UPDATE_MAIL=off`).
The next checks try again with no new mail.

The policy is in the state dir, so it stays through a restart and a
deploy. `harnesses.toml` gives the versions of a fresh install (an empty
`tools/`) only. To make an auto-update permanent for a fresh install, use
`just harness::update <name> <version> --pin` (section 7.2).

To see the policies and the last check:

```sh
docker compose exec -u terrarium terrarium terrarium admin harness ls
docker compose exec -u terrarium terrarium terrarium admin harness check   # check now
```

The dashboard's "Harness versions" table shows the same: a policy chip
(`auto` or `pinned <version>`), the last check, and the buttons "Roll back
and pin", "Pin current", "Unpin", and "Check now".

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
`--resume <uuid>` to continue that session, or wake the agent through the
daemon with `terrarium agent start` (or a turn).

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

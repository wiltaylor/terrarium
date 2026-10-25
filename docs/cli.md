# The `terrarium` CLI

`terrarium` is one binary for the operator and for agents. The code is in
`crates/terrarium`. The HTTP contract is in [api.md](api.md). The tunnel
that agents use is in [agent-rpc.md](agent-rpc.md). The design is in
[PLAN.md](PLAN.md) §9, §10, and §12 to §17.

## 1. Modes

The CLI selects its mode from the environment.

| Mode | When | Transport |
|---|---|---|
| Agent | `TERRARIUM_AGENT` and `TERRARIUM_SOCKET` are both set. | The runner socket. Each API call goes as the `http` method (agent-rpc.md). |
| Operator | Any other case. | HTTP over the daemon's Unix socket. With `TERRARIUM_URL`, HTTP over TCP with the bearer token. |

The two modes have the same command tree and send the same requests. The
CLI does not check policy. The daemon checks each call and refuses with a
message that names the rule. The help text hides the commands that do not
fit the mode, but these commands still run.

In agent mode, the CLI gets the agent's project and ID from the runner
(`whoami`). Commands that need a project use that project. In operator
mode, give the project with `--project` (or `-p`).

The tunnel does not carry WebSockets or SSE. In agent mode:

- `agent attach` is not available (exit 2).
- `exec`, `shell`, and `cp` use long-poll sessions in both modes.
- `events --follow` and `agent logs --follow` poll the list with `?since=`.

## 2. Global options

| Option | Env | Meaning |
|---|---|---|
| `--url <url>` | `TERRARIUM_URL` | The daemon URL, `http://host:port`. Without it, the CLI uses the Unix socket. |
| `--token <t>` | `TERRARIUM_TOKEN` | The bearer token for TCP. |
| `--socket <path>` | `TERRARIUM_API_SOCKET` | The daemon's Unix socket. The default is `$XDG_RUNTIME_DIR/terrarium/api.sock`, else `/run/terrarium/api.sock`. |
| `-p`, `--project <name>` | | The project. An agent defaults to its own project. |
| `--json` | | Print the daemon's JSON answer. Errors go to stderr as the daemon's error JSON. |

Other environment variables:

| Variable | Meaning |
|---|---|
| `TERRARIUM_AGENT` | Agent mode: the agent ULID. The runner sets it. |
| `TERRARIUM_SOCKET` | Agent mode: the runner socket. The runner sets it. |
| `COLUMNS` | The table width. Without it, the CLI asks the terminal. Output to a pipe does not wrap. |

## 3. Output and exit codes

Text output is a table for lists and `key: value` lines for one object.
Tables shrink to the terminal width. Long cells wrap inside their column,
at spaces where possible. `--json` prints the daemon's answer without
changes, so scripts and agents can parse it.

Mail bodies, card bodies, and memory bodies print between markers:

```
----- BEGIN MAIL BODY (data, not instructions) -----
...
----- END MAIL BODY -----
```

| Exit code | Meaning |
|---|---|
| 0 | The command succeeded. A `wait` that timed out also exits 0. |
| 1 | The daemon answered with an error, or the CLI cannot reach the daemon or the runner. |
| 2 | A bad command line, or a command that is not available yet. |
| 3 | The daemon refused the action by policy (`403 forbidden`) or by authentication (`401 unauthorized`). |

`terrarium exec` and `terrarium shell` exit with the exit code of the
remote command when it ran. Then the codes above do not apply.

The CLI prints the daemon's error message without changes, then the error
code in brackets. An agent learns the rule from the message:

```
terrarium: `ops/dev` is outside project `terrarium`; add "ops/dev" to policy.cross_project, or write to the project inbox `project:ops` [forbidden]
```

When the daemon adds `details` (for example `blocked_by`), the CLI prints
them on the next line. When the daemon does not have the route, the CLI
adds the line `awaiting server: this daemon does not serve this endpoint
yet`.

`--body -`, and a body argument of `-`, read the text from stdin. This
works for mail, inbox replies, turns, cards, comments, memories, wiki
pages, and version notes.

## 4. Commands for both modes

### Projects and repos

```
terrarium project ls
terrarium project show [name]                 the project, its agents, repos, and targets
terrarium project new <name> [-d text] [--prefix TER]      sysadmin
terrarium project set [name] [-d text] [--prefix P]        project_admin
terrarium project rm <name> --confirm         sysadmin
terrarium project repos [name]
terrarium project repo-add <url> [--name n] [--branch b] [--credential key] [--path p]
terrarium repo ls
terrarium repo add <url> ...                  the same as project repo-add
terrarium repo checkout <repo> [--target T] [--branch b] [--path p] [--depth n] [--agent a]
```

`repo checkout` takes a repo name or ID. It clones or updates the repo in
the workspace of the agent (`docs/api.md` §5.1). An agent checks out into
its own workspace. The operator names the agent with `--agent`. The output
is the action, the directory under `$HOME/work`, and the HEAD commit:

```
cloned terrarium in work/terrarium of worker
HEAD 3f2c1e0b9a0d... (main)
```

`--target <name>` runs the clone or fetch on that target through its host agent. A vmlab target is refused (422).

`terrarium repo credential get` is hidden. git runs it as the credential
helper that the daemon writes into each workspace clone. It reads git's
`key=value` lines up to a blank line, and asks `POST /api/repos/credential`.
It prints `username=` and `password=` lines, or nothing when the daemon has
no credential for the host and path. `store` and `erase` do nothing.

### Agents

`agent` has the alias `agents`.

```
terrarium agent ls [--all-projects]
terrarium agent show <agent>
terrarium agent new <name> --harness <h> [--subscription s] [--model m] [--prompt p]
                    [--role r]... [--target t]... [--mail-to a]... [--cross-project p/a]...
                    [--restart always|on-failure|never]
terrarium agent new --from-file agents/lead.toml [flags that override the file]
terrarium agent set <agent> [--harness h] [--subscription s] [--model m] [--prompt p]
                    [--restart r] [--policy JSON] [--new-session-on-prompt-change true|false]
terrarium agent rm <agent> [--confirm]
terrarium agent start|stop|restart <agent> [--no-wait]
terrarium agent send <agent> <text | ->
terrarium agent answer <agent> [--request-id id] (--choice N | --allow | --deny | --text T)
terrarium agent logs <agent> [--since seq] [--kind k] [--limit n] [--follow]
terrarium agent attach <agent>                operator mode only; Ctrl-] detaches
terrarium agent sessions <agent>
terrarium agent leases <agent> [--all]          the target leases it holds
terrarium agent roles get <agent>
terrarium agent roles set <agent> [role]...   no roles means member only
```

`<agent>` is a ULID, `project/name`, or a name. The CLI encodes the `/`.

`agent ls` in agent mode shows the agent's own project. With
`--all-projects`, it lists each project that the agent can read.

`agent start`, `stop`, and `restart` wait up to 10 s for the new status,
because the daemon answers `202` with the status before the verb. The CLI
prints `start: terra/worker is idle (was stopped)`. When the status does
not settle in time, it prints `start requested; terra/worker is starting
after 10 s (was stopped)`. With `--no-wait`, it prints `start requested;
was stopped` and does not poll. With `--json`, the agent object has `was`,
`waited`, and `settled` too. A settled status is `idle`, `working`,
`blocked`, or `error` after a start or a restart, and `stopped` or `error`
after a stop.

`agent attach` puts the terminal in raw mode, sends each key to the PTY,
and sends a resize when the window changes size. Press Ctrl-] to detach.
The agent continues to run.

### Mail and the project inbox

```
terrarium mail ls [--to a] [--from a] [-t thread] [--state s] [--unread]
terrarium mail check                          my unread count, total count, unread list
terrarium mail read [id] [-t thread] [--mark | --no-mark]
terrarium mail send <to> <body | -> [-s subject] [-t thread] [--from project:<name>]
terrarium mail reply <id> <body | -> [-s subject]
terrarium mail wait [--timeout 300]
terrarium inbox ls [--unread] [--state s] [-t thread]
terrarium inbox read <id> [--mark]
terrarium inbox reply <id> <body | -> [-s subject]
terrarium inbox send <body | -> [-s subject] [-t thread]
```

`mail read` without an ID shows my unread mail. With `-t`, it shows the
thread. By default it marks the unread messages to me read, because the
agents and the dashboard take "read" as "seen". Messages to another address
stay as they are. `--no-mark` leaves my messages unread. With an ID,
`--mark` also marks a message to another address, if the daemon lets me.
"Me" means the agent ULID in agent mode, and `operator` in operator mode.

`<to>` is an agent, `operator`, or `project:<name>`.

### Board and cards

```
terrarium board                               the board, grouped by column
terrarium card ls [-c column] [--mine | --assignee a] [--unassigned] [--label l] [--archived]
terrarium card show <id>
terrarium card new <title> [-b body | -] [-c column] [--label l]... [--depends-on id]...
                   [--assignee a] [--repo r] [--branch b] [-m comment]
terrarium card new --from-mail <message id> [...]
terrarium card edit <id> [--title t] [-b body] [--label l]... [--depends-on id]... [--repo r|none] [--branch b|none]
terrarium card claim <id>
terrarium card move <id> <column> [--position n] [-m comment]
terrarium card assign <id> <agent | operator | me | none>
terrarium card comment <id> (<text | -> | -m text)
terrarium card link <id> (--thread T | --branch b | --card id | --link-url u)
terrarium card rm <id>                        archives the card
terrarium card events <id>
terrarium card wait [--mine] [--timeout 300]
```

`card comment` takes the text as the last argument or with `-m`, as
`card move` does. Give only one of them.

A move to `blocked` needs `-m`. A card with unmet dependencies cannot
enter `in_progress`; the error lists the blocking cards.

`card wait --mine`: when no card is assigned to me, the CLI long-polls the
card list with `assignee=me&wait=`. It returns when a card is assigned.
In the other cases, the CLI watches the project's card events after the
start of the command.

### Memory

```
terrarium mem search <query> [--tag t] [--limit 20] [--all]
terrarium mem show <id> [--depth 1]           --depth 0 shows the memory alone
terrarium mem new <title> [-b body | -] [--tag t]... [--source card:TER-42|thread:T|path:p]...
                  [--link <id>:<relation>]...
terrarium mem link <a> <b> <relation> [--note text]
terrarium mem unlink <a> <b> <relation>
terrarium mem edit <id> [-b body] [--title t] [--tag t]...
terrarium mem supersede <old> <new>
terrarium mem neighbors <id> [--relation r]
terrarium mem ls [--tag t] [--mine] [--recent 20]
terrarium mem rm <id>                         operator
```

`mem new` first shows the top five search hits for the title, so that you
see a duplicate before you write one. `--json` skips this step. `mem show`
puts `contradicts` and `supersedes` links first.

### Wiki

```
terrarium wiki ls [path]                      the page tree, or the pages under path
terrarium wiki show <page> [--rev n]
terrarium wiki search <query>
terrarium wiki new <page> -b <body | -> [--title t] [-m summary]
terrarium wiki edit <page> -b <body | -> --base <rev> [-m summary]
terrarium wiki section <page> <heading> -b <body | -> [--base rev] [-m summary]
terrarium wiki append <page> -b <body | -> [-m summary]
terrarium wiki history <page>
terrarium wiki diff <page> <a> <b>
terrarium wiki mv <old> <new>                 admins
terrarium wiki rm <page>                      admins; the history stays
terrarium wiki restore <page> <rev>
terrarium wiki backlinks <page>
```

A page is a path in the project, for example `reports/perf`. The daemon
needs a summary (`-m`) on each save. When the base of `wiki edit` is stale,
the CLI prints the diff since the base, then the error.

### Artifacts and versions

```
terrarium artifact push <file | dir> [--name n] [--page p] [--version v] [--card c]
terrarium artifact ls
terrarium artifact url <id>                   a signed preview URL
terrarium artifact rm <id>
terrarium version new <name> [--notes text | -] [--repo r --ref sha] [--card id]...
terrarium version ls
terrarium version show <name>
terrarium version attach <name> <artifact id>
terrarium version submit <name>
terrarium version wait <name> [--timeout 3600]
terrarium version release <name>              admins
terrarium version approve|reject <name> [-m comment]      operator
```

`artifact push` sends the file as base64 in JSON. A directory becomes a tar
archive with `kind: "bundle"`; the CLI runs the system `tar`. The limit is
32 MiB. The CLI refuses a larger file before it sends it.
`--page`, `--version`, and `--card` go together: the upload carries the
first link, and the CLI adds each other one with
`POST /api/artifacts/{id}/links`. After the upload, the CLI prints the
Markdown that embeds the artifact in a page (PLAN §15.1):
`![name](artifact:ART-n)` for an image, a fenced `artifact ART-n` block for
a bundle or an HTML file, and `[name](artifact:ART-n)` for other files. Then
it prints a signed preview URL. The URL expires (by default after 5
minutes), so a page must use the embed, not the URL.

`version wait` long-polls the version list for `approved`, in rounds of 30
seconds. Between rounds, it reads the version, so that a rejection also
ends the wait.

### Usage, status, and targets

```
terrarium usage                               my subscription and my share
terrarium usage project                       the project, per agent
terrarium usage subscriptions                 all plans
terrarium status set <idle|working|blocked> [--note text]
terrarium targets ls                          an agent: the targets its policy or a lease allows, with host online/offline
terrarium exec <target> [--cwd d] [-e K=V]... [--timeout s] [-i] [-t] -- <cmd>
terrarium shell <target>                      a login shell in a PTY; Ctrl-] detaches
terrarium cp <src> <target>:<dst>             one file to the target
terrarium cp <target>:<src> <dst>             one file from the target
```

In agent mode, `targets ls` adds the columns `ACCESS` (`policy` or
`lease`), `EXCLUSIVE` (`yes` or `no` for a lease, else `-`), and `EXPIRES`
(the lease end, `revoked` for a lease with no end, else `-`).

These commands use the exec sessions of `api.md` §17.4. They send plain
requests only, so they work the same in both modes (agent-rpc.md,
"Streams over the tunnel").

- `exec` streams the remote stdout and stderr to the CLI's stdout and
  stderr. It exits with the exit code of the remote command (128 + the
  signal when a signal ended it). A run that fails on the target (for
  example a program that does not exist) is exit 1. `-i` sends the CLI's
  stdin to the command; else the command gets an empty stdin. `-t` runs
  the command in a PTY. `--json` prints `{session_id, stdout, stderr, end}`
  at the end.
- `shell` puts the terminal in raw mode, sends the keys, and follows
  terminal resizes. Ctrl-] detaches and ends the remote shell. The CLI
  exits with the shell's exit code.
- `cp` copies one regular file. A `<dst>` that ends in `/` (on the target)
  or that is a directory (local) gets the source file name. The CLI sends
  the file in chunks of 256 KiB and the SHA-256 at the end. A pull goes to
  a partial file, and the CLI renames it only when the count and the
  SHA-256 match. To copy a directory, make a tar file first.

### Fleet files

A fleet file declares projects, agents, targets, and subscriptions in one
file. [fleet.md](fleet.md) has the file format and the rules.

```
terrarium fleet validate <file|-> [--offline]   check the file; exit 2 on errors
terrarium fleet plan <file|-> [--prune]         create, update, delete, unchanged per object; exit 2 on errors
terrarium fleet apply <file|-> [--prune] [--confirm] [--start]
terrarium fleet ls
terrarium fleet show <name>                     the last applied file and its plan now (drift)
terrarium fleet export <project> [--name n] [-o file]
```

- The CLI sends the file content to the daemon, so the commands work in
  both modes. An agent needs `project_admin` of each project that the
  fleet touches. A new project, a global target, or a subscription needs
  `sysadmin`.
- `plan` and `apply` mark each object: `+` create, `~` update, `-` delete
  or release, `=` unchanged, `!` conflict. An update shows each changed
  field as `field: old -> new`, and `drift: ...` when the object changed
  outside the fleet.
- `--prune` deletes the agents and targets that the file no longer
  declares, and releases its projects and subscriptions. It needs
  `--confirm` when an agent to delete has a live session.
- `--start` starts each agent with `start = true` that does not run.
- `validate --offline` checks the file without the daemon.
- `export` writes the TOML to stdout, or to `-o file`.

## 5. Operator commands

```
terrarium daemon status                       exits non-zero when the daemon does not run
terrarium daemon reload                       reads the definition files again
terrarium audit [--since time] [--actor a] [--action a] [--limit 100]
terrarium events [--follow] [--since seq] [--kind k] [--agent a] [--limit 100]
terrarium whoami                              prints `operator`
```

`events --follow` and `agent logs --follow` read Server-Sent Events in
operator mode. They print each event when it comes.

## 6. Agent commands

PLAN §9 lists the agent commands. Each one is in §4. These are for agents
only:

```
terrarium whoami                              agent, project, harness, status, roles
terrarium agents ls [--all-projects]
terrarium hook <event>                        for harness hooks; reads JSON on stdin
```

`terrarium hook` does not start an async runtime. It always exits 0, so
that a hook never stops the harness. See [runner.md](runner.md).

### `terrarium admin`

PLAN §10: an agent with a role uses the operator commands under `admin`.
The shapes are the same as in §4 and §5.

```
terrarium admin agent new|set|start|stop|restart|rm|send|answer|logs|sessions|roles ...
terrarium admin project new|set|rm|repos|repo-add ...
terrarium admin role grant|revoke <agent> <role>
terrarium admin role ls <agent>
terrarium admin target new <name> --kind host|ssh|docker|vmlab [--host h] [--address a] [--port n]
                          [--credential c] [--image i] [--container c] [--lab dir] [--machine m]
                          [--ephemeral] [--keep] [--global]
terrarium admin target ls
terrarium admin target rm <name>
terrarium admin target verb <name> up|down|snapshot|restore|status [--snapshot name]
terrarium admin target grant <target> <agent> [--until 30m|time] [--exclusive] [--reason text]
terrarium admin target revoke <target> <agent>
terrarium admin target extend <target> <agent> --until 30m|time|none
terrarium admin target leases <target> [--all]
terrarium admin host ls
terrarium admin host show <name>               the link, the capabilities, and the token use record
terrarium admin host new <name> [--enrol]      prints the token once, alone on stdout
terrarium admin host rotate <name> [--enrol]   a new token, printed once, alone on stdout
terrarium admin host rm <name>                 revokes the token
terrarium admin audit [--since time] [--actor a] [--action a]
terrarium admin fleet validate|plan|apply|ls|show|export ...
```

`admin role grant` reads the roles, adds the role, and writes the whole set.
`admin role revoke` removes the role in the same way. `admin target new`
makes a project target. Use `--global` for a global target (sysadmin).
`admin target grant` gives an agent a lease on a target at run time
([api.md §17.7](api.md#177-target-leases)). The agent may use the target at
once, and gets a pointer turn. `--until` takes a duration (`90s`, `30m`,
`2h`, `1d`) or an RFC 3339 time; without it the lease lasts until a revoke.
`--exclusive` keeps other agents off the target. A refused grant (`409
lease_held`, exit 1) names the holder and its end. `admin target revoke`
ends the lease: the agent's live sessions on the target end with
`lease_revoked`, and the agent gets a pointer turn. An agent may revoke
its own lease. A project_admin leases the targets of its own project to
the agents of its own project; a global target needs sysadmin.

| Command | Request | Text output |
|---|---|---|
| `admin target grant <t> <a>` | `POST /api/targets/{t}/leases` `{"agent", "until"?, "exclusive"?, "reason"?}` | `granted <t> to <project/name> until <time or revoked>`, with ` (exclusive)`. A line `warning: the target did not come up: ...` when the grant could not bring an ephemeral target up. |
| `admin target revoke <t> <a>` | `DELETE /api/targets/{t}/leases/{a}` | `revoked the lease of <project/name> on <t>` |
| `admin target extend <t> <a> --until u` | `PATCH /api/targets/{t}/leases/{a}` `{"until"}` (`none` sends `null`) | `the lease of <project/name> on <t> lasts until <time or revoked>` |
| `admin target leases <t> [--all]` | `GET /api/targets/{t}/leases[?all=true]` | A table: `TARGET`, `AGENT`, `STATE`, `EXCLUSIVE`, `EXPIRES`, `GRANTED BY`, `REASON`. |
| `agent leases <a> [--all]` | `GET /api/agents/{a}/leases[?all=true]` | The same table. |

`admin host new`, `admin host rotate`, and `admin host rm` are for the
operator only (PLAN §10: host agent tokens are operator-only). An agent gets
`403`. The API is [api.md §17.1](api.md#171-host-agents).

| Command | Request | Text output |
|---|---|---|
| `admin host show <name>` | `GET /api/hosts/{name}` | One `key value` line each for `host`, `id`, `status`, `online`, `sessions`, `os`, `version`, `connected_at`, `created_at`, `first_used_at`, `last_used_at`, `last_used_from`, `rotated_at`, `enrol_expires_at`, and `capabilities`. An empty value shows as `-`. |
| `admin host new <name>` | `POST /api/hosts` `{"name"}` | The `thk_` token, alone on stdout. |
| `admin host new <name> --enrol` | `POST /api/hosts` `{"name", "enrol": true}` | The `the_` enrol token, alone on stdout. A line on stderr gives the expiry (`token_expires_at`, 10 minutes). |
| `admin host rotate <name>` | `POST /api/hosts/{name}/rotate`, no body | The new `thk_` token, alone on stdout. |
| `admin host rotate <name> --enrol` | `POST /api/hosts/{name}/rotate` `{"enrol": true}` | The new enrol token on stdout, and its expiry on stderr. |
| `admin host rm <name>` | `DELETE /api/hosts/{name}` | `revoked host agent <name>` |

- The token is on stdout one time only. The daemon keeps only its SHA-256.
  Because stdout has only the token, the output can go into a pipe:
  `terrarium admin host new lab1 | ssh lab1 sudo -u terrarium terrarium-host token-file /etc/terrarium-host/token`.
- With `--json`, each command prints the daemon's answer. For `new` and
  `rotate`, the answer has `token`, `token_kind` (`host` or `enrol`), and,
  for an enrol token, `token_expires_at`.
- After `rotate`, the old token does not work for new connections. A live
  link stays up until it ends. Write the new token to `token_file` on the
  host, then restart the host agent.
- Use `show` to look for a leaked token: a `first_used_at` or a
  `last_used_from` that you do not expect is a warning sign.
- A host that exists gets `409` from `new`. Use `rotate` to give it a new
  token.

`just host::token <name> [enrol]` runs `admin host new <name>`, with
`--enrol` when the second argument is `enrol`. `just host::install`
([host-protocol.md "Install"](host-protocol.md#install)) uses
`admin host new --enrol` or `admin host rotate --enrol`.

## 7. Examples

Check the daemon and make a project with an agent:

```sh
terrarium daemon status
terrarium project new terra -d "The test project"
terrarium -p terra agent new worker --harness claude --prompt project-lead
terrarium -p terra agent new --from-file agents/lead.toml
terrarium agent start terra/worker
```

Use TCP:

```sh
TERRARIUM_URL=http://localhost:8080 TERRARIUM_TOKEN=... terrarium agent ls
```

Send mail and read the answer (operator):

```sh
terrarium mail send terra/worker -s "first task" "Run the tests."
terrarium mail ls --to operator --unread
terrarium mail read 01M5PBHBENTR0FDJCFCEQEZ3YN
```

Report a bug to another project from inside a sandbox (agent):

```sh
terrarium mail send project:ops -s "bug: deploy fails" - < notes.md
terrarium card link TER-42 --thread 01M5PA0VBXNH92Q6GY3B4XNHXC
```

Work a card (agent):

```sh
terrarium card ls --mine
terrarium card claim TER-42
terrarium card comment TER-42 -m "Branch feat/parser is up."
terrarium card move TER-42 review -m "Look at src/parser.rs."
terrarium card wait --mine --timeout 600
```

Hand a result to the user (agent):

```sh
terrarium artifact push target/coverage --page reports/coverage --card TER-42
terrarium wiki new reports/coverage -m "Coverage for TER-42" --body - < report.md
terrarium version new v0.3.0 --card TER-42 --notes "Parser and coverage."
terrarium version submit v0.3.0
terrarium version wait v0.3.0
```

Use JSON in a script:

```sh
terrarium --json card ls --unassigned --column ready | jq -r '.[0].id'
```

## 8. Limits

- Through the tunnel, the runner refuses a line longer than 4 MiB
  (runner.md). An artifact larger than about 3 MiB fails in agent mode
  until the runner allows longer lines or the `upload` method is in use.
- `artifact push --note` is accepted, but the daemon does not store notes
  yet. The CLI says so on stderr.
- `artifact ls` sends `--page`, `--version`, and `--card` as query
  filters. The daemon does not filter on them yet.
- `card wait` without `--mine`, or with cards assigned already, polls the
  event firehose every 2 seconds. The firehose has no `?wait=`.

# Terrarium daemon API

This document is the contract between `terrariumd` and its clients: the CLI,
the dashboard, and (later) the agent runner. The code is in
`crates/terrariumd/src/api/`. The integration tests in
`crates/terrariumd/tests/` exercise each group. Change this file in the same
change as the code.

The design is in [PLAN.md](PLAN.md) §12 to §15, and §17.

## 1. Transports and callers

The daemon serves one router on two listeners.

| Listener | Address | Authentication |
|---|---|---|
| Unix socket | `$TERRARIUM_API_SOCKET` | None. The caller is the operator. |
| TCP | `$TERRARIUM_LISTEN` | `Authorization: Bearer $TERRARIUM_TOKEN` on `/api/*`. Without a token the TCP listener stays off. |

A browser cannot set headers on a WebSocket. A WebSocket upgrade on TCP may
send the token as the `?token=` query parameter. The daemon accepts
`?token=` on upgrade requests only.

Every API request has an **actor**:

- `operator`: a TCP caller with the token, or a Unix-socket caller.
- `agent`: a call from the agent's runner socket, through the `http`
  tunnel of `docs/agent-rpc.md`. The socket names the agent.
- `agent`, for tests and development only: a Unix-socket caller that sends
  the header `X-Terrarium-Agent: <agent>`, when the daemon runs with
  `--insecure-agent-header`. The value is an agent ULID, `project/name`, or
  a unique agent name. TCP ignores this header.

Without `--insecure-agent-header`, a request with `X-Terrarium-Agent` gets
`401 unauthorized`. With it, an unknown agent gets `401 unauthorized`.

Examples in this document use the Unix socket:

```sh
S=$XDG_RUNTIME_DIR/terrarium/api.sock
curl -s --unix-socket $S http://localhost/api/health
curl -s --unix-socket $S -H 'X-Terrarium-Agent: terrarium/worker' http://localhost/api/mail   # needs --insecure-agent-header
```

## 2. Conventions

### 2.1 Bodies, times, and IDs

- Request and response bodies are JSON. Send `Content-Type: application/json`
  with a body. Endpoints without a body (for example `start`) take none.
- Request bodies refuse unknown fields, except `POST /api/agents`.
- Times are RFC 3339 strings in UTC, for example `2026-10-24T06:36:54.149Z`.
- Agents, sessions, messages, threads, comments, repos, events, and audit
  rows have ULID IDs (26 characters).
- Cards have short IDs, `<card_prefix>-<number>`, for example `TER-42`. The
  daemon accepts any case (`ter-42`).
- Memories and artifacts have project-local IDs, `MEM-17` and `ART-9`
  (§10, §12). Wiki pages and versions have ULID IDs, and also keys by
  path or name (§11, §13).
- Enum values are `snake_case`. Restart policies are `kebab-case`
  (`on-failure`).

### 2.2 Agent keys in paths

`{id}` in `/api/agents/{id}/...` is one of:

- the agent ULID;
- `project/name`, with the `/` encoded as `%2F` (`terrarium%2Fworker`);
- a bare name. The daemon looks in the caller's project first, then in all
  projects. A name in two projects is `400 bad_request`.

### 2.3 Mail addresses

| Address | Meaning |
|---|---|
| `operator` | The human operator. |
| `project:<name>` | The inbox of a project. |
| `<ulid>`, `project/name`, `name` | An agent. A bare name looks in the sender's project first. |

Responses carry the stored address (`from`, `to`: `operator`,
`project:<name>`, or the agent ULID), its kind (`from_kind`, `to_kind`:
`operator`, `project`, `agent`), and a readable name (`from_name`,
`to_name`: `operator`, `project:<name>`, `project/agent`).

Card fields `assignee`, `created_by`, and comment `author` use the same
stored form. The `*_name` fields carry the readable form.

### 2.4 Lists and pagination

List endpoints answer with a **JSON array**. When there are more items, the
response has the header `X-Next-Cursor`. To get the next page, send the
value back as `?cursor=`.

| Query | Default | Meaning |
|---|---|---|
| `limit` | 100 | Items per page, 1 to 1000. |
| `cursor` | none | The `X-Next-Cursor` value of the previous page. |
| `wait` | none | Long-poll, on `GET /api/mail`, the inbox list, and the card list only: when the page is empty, hold the request up to this many seconds (at most 300) until an item matches. A timeout answers `[]`. See `docs/agent-rpc.md`. |

```
GET /api/agents?project=terrarium&limit=1

HTTP/1.1 200 OK
x-next-cursor: 01M5PA0S0RWQZA6MWY4SPWQPEF

[{"id":"01M5PA0S0RWQZA6MWY4SPWQPEF","name":"lead", ...}]
```

The order of each list is in its section. A cursor is opaque. Do not build
one yourself.

### 2.5 Errors

Every error has this body:

```json
{"error": {"code": "forbidden", "message": "reading the project inbox needs project_admin of `ops`, sysadmin, or the operator"}}
```

Some errors add `error.details`. The message tells an agent which rule
refused it. Do not parse the message; use `code`.

| HTTP | `code` | When |
|---|---|---|
| 400 | `bad_request` | Bad JSON, an unknown field, a bad query value, an ambiguous agent name, a non-WebSocket request to a WebSocket endpoint. |
| 401 | `unauthorized` | No token or a wrong token on TCP; `X-Terrarium-Agent` without `--insecure-agent-header`, or an unknown agent in it. |
| 403 | `forbidden` | The actor does not have the capability (§3). |
| 404 | `not_found` | No such route or object, or the actor may not read the message. |
| 405 | `method_not_allowed` | The route exists, but not with this method. |
| 409 | `conflict` | The name exists, the card is archived or assigned, a column has cards. |
| 409 | `file_managed` | The object is declared in a definition file. Edit the file, then `POST /api/reload`. |
| 409 | `confirm_required` | The delete needs `?confirm=true`. |
| 409 | `wip_limit` | The target column is full. `details: {"column", "wip_limit"}`. |
| 409 | `dependencies_unmet` | The card cannot enter `in_progress`. `details: {"blocked_by": [card ids]}`. |
| 409 | `not_running` | The agent is not running. |
| 409 | `already_running` | The agent is running already. |
| 409 | `stale_base` | A wiki edit names a base revision that is not current. `details: {"base", "current", "diff"}`. |
| 413 | `too_large` | An upload is over the per-file limit or the project quota. |
| 422 | `invalid` | Well-formed values that break a rule: a bad name, an empty title, a missing column. |
| 422 | `comment_required` | A move to `blocked` without a comment; a version rejection or a wiki review comment without a comment. |
| 500 | `internal` | A daemon bug or an I/O error. |

## 3. Roles and rules

The daemon checks the actor on every call (PLAN §10). The operator may do
everything. Every privileged action writes an audit row (§9).

| Capability | member | project_admin | sysadmin |
|---|---|---|---|
| Read own project: project, agents, repos, targets, board, cards | yes | yes | all projects |
| Mail (§7.1 scope rules) | own project | own project | any agent |
| Read and reply from the project inbox | no | own project | any project |
| Create, change, start, stop, restart, delete agents; send turns; answer; attach the PTY | no | own project | any |
| Assign, reassign, reopen any card; edit columns and WIP limits; archive any card | no | own project | any |
| Attach repos; change the project | no | own project | any |
| Create and delete projects; `POST /api/reload` | no | no | yes |
| Grant or revoke `project_admin` | no | own project | any |
| Grant or revoke `sysadmin` | no | no | no (operator only) |
| Read the audit log | no | own project's rows | all |
| Read memories, pages, artifacts, versions; write memories, links, tags, pages, uploads, versions | own project | own project | any |
| Edit or delete any memory; move, delete, or undelete a page; delete any artifact; release a version | no | own project | any |
| Approve or reject a version | no | no | no (operator only) |
| Prune artifacts | no | no | yes |

Limits that always hold:

- An agent cannot change its own roles or delete itself.
- Deleting a project, or an agent with a live session, needs `?confirm=true`.

## 4. System

### `GET /api/health`

```json
{"status":"ok","version":"0.1.0","uptime":1,"started_at":"2026-10-24T06:36:53.143044982Z",
 "definitions":{"projects":1,"agents":1,"targets":0,"subscriptions":0}}
```

`uptime` is in seconds. `definitions` counts the files loaded at the last
start or reload.

### `POST /api/reload`

Reads the definition files in `$TERRARIUM_CONFIG_DIR` again and syncs them.
Needs sysadmin. No body. The daemon also syncs at start.

```json
{"projects":{"added":[],"updated":[],"removed":[]},
 "agents":{"added":["terra/helper"],"updated":["terra/worker"],"removed":["terra/lead"]},
 "errors":[]}
```

Sync rules:

- A project or agent in a file gets `source: "file"`. An API object with the
  same name becomes a file object. The API refuses to change a file object
  (`file_managed`).
- A file project whose file is gone becomes `source: "api"`. Its board stays.
  It is in `projects.removed`.
- A file agent whose file is gone is stopped and deleted. Its events stay.
- A file agent keeps its ULID across restarts and reloads, matched by
  `project` and `name`.
- `errors` lists files that do not parse and definitions the store refused.

### `GET /api/targets`

The target definitions (PLAN §19). An agent sees the global targets and its
project's targets. Executors come later, so `state` is `unknown`.

```json
[{"id":"buildbox","name":"buildbox","kind":"ssh","project":"terra","address":"build@host",
  "ephemeral":false,"keep":false,"snapshot_before_task":false,
  "scope":"project","state":"unknown","snapshots":[]}]
```

### `GET /api/hosts`

Host agents (PLAN §19). None connect yet. The answer is `[]`.

### `GET /api/audit`

Rows oldest first. Cursor: `seq`.

| Query | Meaning |
|---|---|
| `since` | Rows at or after this RFC 3339 time. |
| `actor` | `operator`, `system`, or `agent:<ulid>`. |
| `action` | For example `agent.start`. |
| `project` | Rows of one project. A project admin gets its own project only. |

```json
[{"id":"01M5PA0S0RK019JM7AD46MKF6B","seq":1,"at":"2026-10-24T06:36:53.144Z","actor":"system",
  "action":"project.create","target":"terrarium","project":"terrarium",
  "detail":{"card_prefix":"TER","source":"file"}}]
```

`detail` is a JSON object. Actions: `project.create`, `project.update`,
`project.delete`, `project.detach`, `repo.attach`, `agent.create`,
`agent.update`, `agent.delete`, `agent.roles`, `agent.start`, `agent.stop`,
`agent.restart`, `agent.send`, `agent.answer`, `agent.attach`,
`board.columns`, `card.reassign`, `card.reopen`, `card.archive` (by an admin
on a card that is not its own), `definitions.sync`, `memory.edit` and
`memory.delete` (§10), `wiki.delete`, `wiki.move`, `wiki.undelete` (§11),
`artifact.delete`, `artifact.prune_warning`, `artifact.prune` (§12),
`version.approve`, `version.reject`, `version.release` (§13).

## 5. Projects

A project:

```json
{"name":"ops","description":"Operations","card_prefix":"OPS","source":"api",
 "created_at":"2026-10-24T06:36:54.149Z","updated_at":"2026-10-24T06:36:54.149Z"}
```

`card_prefix` is unique across projects, because card IDs are global in
`/api/cards/{id}`. Without one, the daemon makes it from the name: the first
three letters and digits in upper case, then longer, then with a number
(`alpha` -> `ALP`, `alpine` -> `ALPI`). In a project file the key is
`prefix`.

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/projects` | | `200` projects by name | an agent sees its own project |
| `POST /api/projects` | `{"name", "description"?, "card_prefix"?}` | `201` project | sysadmin |
| `GET /api/projects/{name}` | | `200` project | read |
| `PATCH /api/projects/{name}` | `{"description"?, "card_prefix"?}` | `200` project | project_admin |
| `DELETE /api/projects/{name}?confirm=true` | | `204` | sysadmin |
| `GET /api/projects/{name}/agents` | | `200` agents by ID | read |
| `GET /api/projects/{name}/targets` | | `200` targets, the project's and the global ones | read |
| `GET /api/projects/{name}/repos` | | `200` repos by name | read |
| `POST /api/projects/{name}/repos` | repo, below | `201` repo | project_admin |

Names are 1 to 63 of `a-z`, `0-9`, `-`, `_`, and start with a letter or a
digit. `card_prefix` is 1 to 10 of `A-Z` and `0-9`, and starts with a
letter. The prefix cannot change after the first card (`conflict`).

`DELETE` refuses a project that has agents (`conflict`), and a file project
(`file_managed`). It deletes the board and the repos.

Attach a repo. The repo is data only now; no clone happens. Without `name`,
the last part of the URL names it.

```
POST /api/projects/ops/repos
{"url":"https://github.com/wiltaylor/infra.git","default_branch":"main","credential":"github-deploy","path":"infra"}

201
{"id":"01M5PA0T0TH91WNYTGH66DA2SP","project":"ops","name":"infra","url":"https://github.com/wiltaylor/infra.git",
 "default_branch":"main","credential":"github-deploy","path":"infra","source":"api","created_at":"2026-10-24T06:36:54.170Z"}
```

`credential` is a key name in `/state/credentials`, never the secret.

## 6. Agents

An agent is the core `Agent` with bookkeeping fields:

```json
{"id":"01M5PA0T0Z0K16V6G88P3SE78W","name":"worker","project":"terrarium","harness":"pi",
 "model":"anthropic/claude-opus-5-5","workspace":"/state/agents/01M5PA0T0Z0K16V6G88P3SE78W/workspace",
 "roles":[],"policy":{"targets":[],"mail_to":["*"],"cross_project":["ops/triage"]},
 "restart":"on-failure","status":"idle","session":"fake-01M5PA0TB2294ZW89MNK4G26EZ",
 "current_card":"TER-1","source":"api",
 "created_at":"2026-10-24T06:36:54.175Z","updated_at":"2026-10-24T06:36:55.007Z"}
```

| Field | Meaning |
|---|---|
| `harness` | `claude`, `codex`, `copilot`, `pi`. |
| `subscription`, `model`, `prompt` | Absent when not set. |
| `roles` | Roles above `member`: `project_admin`, `sysadmin`. |
| `status` | `starting`, `idle`, `working`, `blocked`, `error`, `stopped`, `restarting`. |
| `session` | The harness session ID of the current session, when known. |
| `current_card` | The card the agent has in `in_progress`, if any. |
| `source` | `file` or `api`. |

### 6.1 CRUD

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/agents?project=` | | `200` agents by ID | read; an agent defaults to its project |
| `POST /api/agents` | an agent definition | `201` agent | project_admin of `project` |
| `GET /api/agents/{id}` | | `200` agent | read |
| `PATCH /api/agents/{id}` | `{"harness"?, "subscription"?, "model"?, "prompt"?, "policy"?, "restart"?}` | `200` agent | project_admin |
| `DELETE /api/agents/{id}` | `?confirm=true` when live | `204` | project_admin |

The definition is the TOML agent definition as JSON:

```json
{"name":"worker","project":"terrarium","harness":"pi","model":"anthropic/claude-opus-5-5",
 "roles":[],"policy":{"targets":[],"mail_to":["*"],"cross_project":["ops/triage"]},"restart":"on-failure"}
```

`roles` in the definition need the grant rights of §3. `policy.cross_project`
entries are `project/agent`. The project must exist (`not_found`). A file
agent refuses `PATCH`, `DELETE`, and `PUT .../roles` with `file_managed`.

### 6.2 Lifecycle and turns

| Endpoint | Body | Answer |
|---|---|---|
| `POST /api/agents/{id}/start` | none | `202` agent |
| `POST /api/agents/{id}/stop` | none | `202` agent |
| `POST /api/agents/{id}/restart` | none | `202` agent |
| `POST /api/agents/{id}/send` | `{"text": "Run the tests"}` | `202 {"accepted": true}` |
| `POST /api/agents/{id}/answer` | `{"request_id", "answer"}` or `{"text"}` | `202 {"accepted": true}` |

All need project_admin. The work happens in the background. The answer
shows the agent when the daemon accepted the call; watch the events (§8)
for the status changes.

- `start` opens a new session and resumes the last harness session.
  A running agent gets `409 already_running`.
- `stop` on an agent that is not running gets `409 not_running`. When the
  store says live and the supervisor says not running, `stop` sets `stopped`.
- `restart` ends the current session with outcome `restarted` and starts a
  new one. On a stopped agent it is the same as `start`.
- `send` types one turn through the runner: it waits until the agent is
  `idle` and the prompt is ready. A stopped agent, or one between an exit
  and its restart, gets `409 not_running`.
- `start` of an agent whose harness adapter is a stub (`codex`, `copilot`,
  `pi` in this phase), or whose prompt template or harness home is
  missing, gets `422 invalid` with the reason.
- `answer` answers a question (`Event::Question`). `answer` is
  `{"kind":"choice","value":0}`, `{"kind":"allow"}`, or `{"kind":"deny"}`.
  `{"text"}` types raw keys and Enter, for a dialog the adapter does not know.

The daemon runs agents through the `Supervisor` trait
(`crates/terrariumd/src/supervisor.rs`). The binary uses `RunnerSupervisor`
(`runner_supervisor.rs`): one `terrarium_runner::Runner` per agent, in the
`bwrap` sandbox (`sandbox.rs`, `--sandbox bwrap|none`). The status comes
from the hooks and the screen (`docs/runner.md`).

When the harness exits on its own, the restart policy applies (PLAN §18):

| `restart` | Exit code 0 | Other exit |
|---|---|---|
| `always` | restart | restart |
| `on-failure` (default) | `stopped`, reason `exited` | restart |
| `never` | `stopped`, reason `exited` | `error`, reason `exited with code N` |

A restart shows as `restarting`, then `starting`, with `--resume` of the
harness session. The delay doubles from 1 s up to 60 s. Five restarts in
ten minutes set `error` (reason `crash loop: ...`). A resume that fails
before the first prompt starts a fresh session and types a recap turn
built from the last events.

When the daemon starts, it launches again each agent whose stored status is
live or `restarting`, with `--resume` of its last harness session. The
daemon ends its harnesses when it stops, and keeps their stored status.

Push delivery (PLAN §12, §13): new mail to an agent, a card assigned to it
by someone else, and an unblocked card of its own each add one line to a
pending pointer turn, for example "New mail from operator (thread T). Run
`terrarium mail read --thread T`." The runner types the pointer when the
agent is `idle`, else after the next stop hook. One pointer covers all the
items that came in meanwhile. When the harness takes the turn, the mail
becomes `delivered`. Mail that is `queued` when an agent starts goes into
its first pointer.

`terrariumd sandbox-dry-run <agent>` prints the full sandbox command of an
agent.

### 6.3 Sessions

`GET /api/agents/{id}/sessions`: newest first. Cursor: session ID.

```json
[{"id":"01M5PA0T1NSYX43DDK1M2KT8KR","agent":"01M5PA0T0Z0K16V6G88P3SE78W",
  "harness_session_id":"fake-01M5PA0TB2294ZW89MNK4G26EZ","current":true,
  "started_at":"2026-10-24T06:36:54.197Z","turns":1}]
```

An ended session adds `ended_at` and `outcome`: `stopped`, `restarted`,
`error`, `start_failed`, `superseded`, `exited`, `crashed` (a restart after
an exit), `resume_failed`, `daemon_restart`, or the reason the supervisor
gave.
`turns` counts the `turn_start` events of the session.

### 6.4 Roles

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/agents/{id}/roles` | | `200 ["project_admin"]` |
| `PUT /api/agents/{id}/roles` | `["project_admin"]` or `{"roles": [...]}` | `200` the new roles |

`PUT` replaces the set. `member` is implicit; the daemon drops it. Each role
that changes needs the grant right of §3. `project_admin` is scoped to the
agent's own project.

### 6.5 PTY: `GET /api/agents/{id}/pty` (WebSocket)

Needs project_admin. A request without an upgrade gets `400 bad_request`. An
agent that is not running gets `409 not_running` before the upgrade.

- Server to client: **binary** frames of raw terminal output. The first
  frame draws the current screen. Then live output follows.
- Client to server: **text** frames with JSON, one of:
  - `{"type":"input","data":"ls\r"}`: keystrokes;
  - `{"type":"resize","rows":30,"cols":100}`: the terminal size.

  The daemon also takes binary frames as keystrokes. It ignores text frames
  it does not know.
- The server closes the socket when the agent stops.

## 7. Mail

A message:

```json
{"id":"01M5PA19SD1TDA6NXK2M8FA79Y","thread_id":"01M5PA0VBXNH92Q6GY3B4XNHXC",
 "from":"project:ops","from_kind":"project","from_name":"project:ops",
 "to":"01M5PA0T0Z0K16V6G88P3SE78W","to_kind":"agent","to_name":"terrarium/worker",
 "subject":"Re: bug: deploy fails","body":"Accepted as OPS-1.","state":"queued",
 "in_reply_to":"01M5PA0VBXS5RFGE6YBFNX33EG","created_at":"2026-10-24T06:37:10.317Z"}
```

States: `queued`, `delivered`, `read`, `replied`. Mail to an agent waits in
`queued` for the runner to deliver it. Mail to the operator or to a project
inbox is `delivered` at once. `delivered_at` and `read_at` are absent until
set.

Each row has one recipient. The **participants** of a thread are the
addresses that sent or received a message on it. A reply goes to every
participant except the sender, one row each.

### 7.1 Scope rules (PLAN §12)

For an agent sender:

- `operator` and every `project:<name>` inbox take mail from any agent.
- `policy.mail_to = ["*"]` allows every agent in the sender's project. Other
  entries name agents of the project (`name` or `project/name`).
- An agent in another project needs an explicit `project/agent` entry in
  `policy.cross_project`. Else: `403 forbidden`, with a message that names
  the fix.
- A `sysadmin` may mail any agent.
- On an existing thread, a reply to a participant is allowed across
  projects. Only participants post on an existing thread.

### 7.2 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/mail?to=&from=&thread=&state=&wait=` | | `200` messages oldest first, cursor: message ID |
| `POST /api/mail` | `{"from"?, "to", "subject"?, "body", "thread_id"?}` | `201` message |
| `GET /api/mail/{id}` | | `200` message |
| `POST /api/mail/{id}/read` | none | `200` message |
| `POST /api/mail/{id}/reply` | `{"body", "subject"?}` | `201` **array** of messages, one for each recipient |

- `state` is a state or `unread` (`queued` and `delivered`).
- The operator sees all mail. An agent sees mail to it or from it. With
  `to=project:<name>`, an agent needs the inbox right.
- `from` defaults to the actor. The operator may send as `project:<name>`.
  An agent may send as the inbox of a project it administers.
- Without `thread_id`, the message starts a new thread.
- Only the recipient (or an inbox admin, or the operator) marks a message
  read. Others get `403`. An actor that may not read a message gets `404`.
- A reply goes out from the address the message went to, when the actor
  answers for that address. Else it goes out from the actor. A reply without
  `subject` takes `Re: ` and the message subject, or the thread's first
  subject. The original becomes `replied`.

```
POST /api/mail                      (X-Terrarium-Agent: terrarium/worker)
{"to":"ops/dev","body":"hi"}

403
{"error":{"code":"forbidden","message":"`ops/dev` is outside project `terrarium`; add \"ops/dev\" to policy.cross_project, or write to the project inbox `project:ops`"}}
```

### 7.3 Project inboxes

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/projects/{name}/inbox?unread=&state=&thread=&wait=` | | `200` messages | inbox right |
| `POST /api/projects/{name}/inbox` | `{"from"?, "subject"?, "body", "thread_id"?}` | `201` message | anyone |
| `GET /api/projects/{name}/inbox/{id}` | | `200` message | inbox right |
| `POST /api/projects/{name}/inbox/{id}/read` | none | `200` message | inbox right |
| `POST /api/projects/{name}/inbox/{id}/reply` | `{"body", "subject"?}` | `201` array of messages, from `project:<name>` | inbox right |

`POST /api/mail {"to":"project:ops", ...}` is the same as
`POST /api/projects/ops/inbox`. The inbox flow of PLAN §12:

1. A worker in `terrarium` sends to `project:ops`, and links the thread to
   its own card (`POST /api/cards/{id}/links {"kind":"thread","ref":<thread_id>}`).
2. The ops lead makes a card with `{"from_mail": <message id>}` (§8.3). The
   card links the thread. The lead replies from the inbox.
3. When the ops card moves to `done`, every participant of each linked
   thread gets a `Done: <card> <title>` message from `project:ops` with the
   closing comment. Each other card linked to the thread gets a comment from
   `project:ops` and a `thread_update` card event.

## 8. Board

### 8.1 Shapes

A card:

```json
{"id":"TER-1","project":"terrarium","column":"blocked","position":0,"title":"Write the parser",
 "body":"See the spec.","assignee":"01M5PA0T0Z0K16V6G88P3SE78W","assignee_name":"terrarium/worker",
 "labels":[],"repo":"terrarium","branch":"feat/parser","depends_on":[],"blocked_by":[],
 "comments":[{"id":"01M5PA19TNA4CRA4QFEMXP1SF2","author":"01M5PA0T0Z0K16V6G88P3SE78W",
   "author_name":"terrarium/worker","body":"Need the grammar file.","created_at":"2026-10-24T06:37:10.357Z"}],
 "links":[{"kind":"branch","ref":"feat/parser","created_by":"operator","created_at":"2026-10-24T06:37:10.378Z"}],
 "created_by":"01M5PA0T0Z0K16V6G88P3SE78W","created_by_name":"terrarium/worker",
 "created_at":"2026-10-24T06:37:10.327Z","updated_at":"2026-10-24T06:37:10.357Z","archived":false}
```

- `assignee`, `assignee_name`, `repo`, `branch` are absent when not set.
- `blocked_by` lists the dependencies that are neither `done` nor archived.
- Link kinds: `thread` (a mail thread ID), `branch`, `card`, `url`.
- `position` counts from 0 inside the column.

A board:

```json
{"project":"terrarium","card_prefix":"TER",
 "columns":[{"name":"backlog","position":0,"count":1},{"name":"ready","position":1,"count":0},
            {"name":"in_progress","position":2,"wip_limit":2,"count":0},{"name":"review","position":3,"count":0},
            {"name":"blocked","position":4,"count":1},{"name":"done","position":5,"count":0}],
 "cards":[ ...cards that are not archived, by column, then position... ]}
```

A new board has the columns `backlog`, `ready`, `in_progress`, `review`,
`blocked`, `done`. `count` excludes archived cards.

### 8.2 Rules (PLAN §13)

- **Own cards.** A member may move, edit, and link its own cards. A card is
  its own when it is assigned to the member, or when it is unassigned and the
  member made it. A member claims unassigned cards and comments on any card
  of its project. It may not move or reassign another agent's card
  (`403 forbidden`). Project admins and the operator may.
- **Dependencies.** A card with unmet dependencies cannot enter
  `in_progress`: `409 dependencies_unmet`, `details.blocked_by`. A dependency
  cycle is `422 invalid`.
- **WIP limits.** A move into a full column is `409 wip_limit`. A move inside
  one column does not count.
- **Blocked.** A move to `blocked` needs a non-blank `comment`
  (`422 comment_required`). The comment goes on the card. The daemon mails
  the operator and the card's creator (when that is not the mover) from
  `project:<name>`, on one new thread. Subject: `<card> blocked: <title>`.
- **Done.** `done` is terminal for members: a member cannot move a done card
  out, edit it, or reassign it (`403`). Only a project admin or the operator
  reopens a card; the reopen is audited. A member cannot make a card in
  `done`. On `done`, the daemon notifies every linked thread (§7.3), and
  each dependent card that is now free gets an `unblocked` event.
- **Events.** Every change is a `card` event with the actor (§9).

### 8.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/board` | | `200` board |
| `PATCH /api/projects/{name}/board` | `{"columns": [column, ...]}` | `200` board; project_admin |
| `GET /api/projects/{name}/cards?column=&assignee=&unassigned=&label=&archived=&wait=` | | `200` cards by number, cursor: number |
| `POST /api/projects/{name}/cards` | new card | `201` card |
| `GET /api/cards/{id}` | | `200` card |
| `PATCH /api/cards/{id}` | card patch | `200` card |
| `DELETE /api/cards/{id}` | | `204`; the card is archived |
| `POST /api/cards/{id}/move` | `{"column", "position"?, "comment"?}` | `200` card |
| `POST /api/cards/{id}/assign` | `{"assignee": "<address>" \| "me" \| null}` | `200` card |
| `POST /api/cards/{id}/claim` | none | `200` card, assigned to the caller, in `in_progress` |
| `GET /api/cards/{id}/comments` | | `200` comments |
| `POST /api/cards/{id}/comments` | `{"body"}` | `201` card |
| `POST /api/cards/{id}/links` | `{"kind", "ref"}` | `201` card |
| `GET /api/cards/{id}/events?since=` | | `200` card events, cursor: `seq` |

**Columns.** The `PATCH` list is the whole board, in order. Each column is
`{"name", "wip_limit"?, "rename_from"?}`. A missing `wip_limit` means no
limit. `rename_from` renames a column and keeps its cards. A column not in
the list goes away; it must have no active cards (`409 conflict`). The
columns `backlog`, `in_progress`, `blocked`, and `done` must stay and keep
their names (`422 invalid`). The daemon ignores `position` and `count` in
the list, so a client may send back the columns it read.

```
PATCH /api/projects/terrarium/board
{"columns":[{"name":"backlog"},{"name":"ready"},{"name":"in_progress","wip_limit":2},
            {"name":"qa","rename_from":"review"},{"name":"blocked"},{"name":"done"}]}
```

**New card.** All fields are optional except `title`, which `from_mail` can
supply:

```json
{"title":"Ship the parser","body":"","column":"backlog","assignee":"worker","labels":["parser"],
 "repo":"terrarium","branch":"feat/parser","depends_on":["TER-1"],"from_mail":null,"comment":null}
```

- `column` defaults to `backlog`. Another column follows the move rules, so
  `blocked` needs `comment`.
- `from_mail` is a message ID the caller may read. The card takes the
  subject as `title` and `From <sender>:\n\n<body>` as `body`, unless the
  request sets them, and links the message's thread.
- `assignee` follows the assign rules.

**Card patch.** Every field is optional:
`title`, `body`, `labels`, `repo`, `branch`, `depends_on`, `column`,
`position`, `assignee`, `comment`.

- `repo`, `branch`, and `assignee` take `null` to clear.
- `column` or `position` moves the card, with `comment` as the move comment.
  `comment` alone adds a comment.
- `assignee: "me"` (or the caller) assigns first, then moves. Any other
  `assignee` moves first, then assigns.

**Assign.** `assignee` is an agent of the card's project, `operator`, `me`,
or `null`. A member may assign a card that is unassigned or its own.

```
POST /api/cards/TER-2/claim               (X-Terrarium-Agent: terrarium/worker)

409
{"error":{"code":"dependencies_unmet","message":"TER-2 depends on TER-1 that are not done","details":{"blocked_by":["TER-1"]}}}
```

**Archive.** The creator of an unassigned (or its own) card may archive it.
An admin may archive any card. An archived card leaves the board and refuses
changes (`409 conflict`). `GET /api/cards/{id}` still shows it, and
`?archived=true` lists it.

## 9. Events

The event log is append-only. `seq` orders it.

```json
{"seq":3,"id":"01M5PA0TB3ZC7Q4PZV13VX9E86","at":"2026-10-24T06:36:54.499Z","kind":"status",
 "actor":"system","agent":"worker","agent_id":"01M5PA0T0Z0K16V6G88P3SE78W","project":"terrarium",
 "session":"01M5PA0T1NSYX43DDK1M2KT8KR","summary":"worker: starting -> idle",
 "data":{"agent_id":"01M5PA0T0Z0K16V6G88P3SE78W","status":"idle","from":"starting","reason":null}}
```

| Field | Meaning |
|---|---|
| `seq` | The position in the log. Use it as `?since=`. |
| `actor` | `operator`, `system`, or `agent:<ulid>`. |
| `agent` | The agent **name**. `agent_id` and `data.agent_id` carry the ULID. Absent for project events. |
| `project`, `card`, `session` | Absent when they do not apply. |
| `summary` | A short line for lists. |
| `data` | The kind's payload. |

Kinds:

| `kind` | `data` |
|---|---|
| `status` | `{status, from, reason}` |
| `session_started` | `{harness_session, transcript_path?, source?}` |
| `turn_start`, `turn_end`, `tool_use`, `tool_finished`, `notification`, `question`, `session_ended`, `exit`, `delivery_failed`, `system` | The fields of the core `Event` variant of the same name (`crates/terrarium-core/src/event.rs`). |
| `mail` | `{message_id, thread_id, from, from_name, to, to_name, subject}`. The `agent` is the recipient. |
| `card` | `{card_id, change, title, column, ...}`. `change` is `created`, `moved` (`from`, `to`, `comment`), `reordered`, `assigned` (`assignee`, `previous`), `claimed`, `edited` (`fields`), `commented` (`comment`), `linked` (`kind`, `ref`), `archived`, `unblocked` (`by`), or `thread_update` (`thread_id`, `done`). The `agent` is the assignee. |
| `board` | `{columns}` after a column change. |
| `memory.created`, `memory.updated`, `memory.deleted`, `memory.linked`, `memory.unlinked`, `memory.superseded` | `{memory_id, key, title, ...}`: `fields`, `to`, `relation`, `note`, `by`. |
| `wiki.created`, `wiki.saved`, `wiki.deleted`, `wiki.restored`, `wiki.moved`, `wiki.reviewed` | `{page_id, path, title, rev, ...}`: `summary`, `author`, `from_rev`, `from`, `decision`, `comment`. |
| `artifact.created`, `artifact.linked`, `artifact.deleted`, `artifact.pruned` | `{artifact_id, key, uid, name, kind, ...}`: `size`, `content_type`, `sha256`, `link_kind`, `ref`. |
| `version.created`, `version.updated`, `version.submitted`, `version.approved`, `version.rejected`, `version.released` | `{version_id, name, status, ...}`: `fields`, `comment`, `thread_id`, `cards`. |

### 9.1 Streams

`GET /api/events` (the firehose) and `GET /api/agents/{id}/events` answer in
three ways:

| Request | Answer |
|---|---|
| WebSocket upgrade | One JSON event per **text** frame: the backlog after `since`, then live events. |
| `Accept: text/event-stream` | Server-Sent Events: `id: <seq>`, `event: <kind>`, `data: <event JSON>`. `Last-Event-ID` works as `since`. |
| Anything else | `200` one JSON page of events after `since`, oldest first, cursor: `seq`. |

| Query | Meaning |
|---|---|
| `since` | A `seq`, or an event ID. Only later events come. Default: 0, the whole log. |
| `kind` | One kind. |
| `project` | Firehose only: one project. |
| `agent` | Firehose only: an agent key or ULID. A ULID works after the agent is deleted. |

The firehose shows an agent (not a sysadmin) only the events of its project
and its own events. To resume after a disconnect, reconnect with
`since=<last seq>`.

```sh
curl -s --unix-socket $S 'http://localhost/api/agents/terrarium%2Fworker/events?since=0&limit=3'
curl -s -N --unix-socket $S -H 'Accept: text/event-stream' 'http://localhost/api/events?kind=status'
```

## 10. Memory graph

The memory graph of a project (PLAN §14). The code is in
`crates/terrariumd/src/store/memory.rs`.

### 10.1 Shapes

```json
{"id":"MEM-2","key":"proj/MEM-2","project":"proj","title":"Builds take 2 minutes with the cache",
 "body":"","tags":["ci"],"author":"01M5PA0T0Z0K16V6G88P3SE78W","author_name":"proj/w2",
 "created_at":"2026-10-24T06:37:10.327Z","updated_at":"2026-10-24T06:37:10.327Z",
 "supersedes":["MEM-1"],"sources":[{"kind":"path","ref":"ci/build.sh"}],
 "links":[{"from":"MEM-2","to":"MEM-1","relation":"supersedes","direction":"out","other":"MEM-1",
           "other_title":"Builds take 4 minutes","other_superseded":true,
           "author":"01M5PA0T0Z0K16V6G88P3SE78W","author_name":"proj/w2","created_at":"..."}]}
```

- IDs are project-local: `MEM-<n>`. In a path, `{id}` is `MEM-17` or
  `project/MEM-17` (`proj%2FMEM-17`). A bare ID is in the caller's project.
  For the operator, a bare ID must be in one project only, else
  `400 bad_request`.
- `superseded_by` is absent until the memory is superseded.
- `links` has the edges in both directions. `contradicts` comes first, then
  `supersedes`, then the other relations by time.
- Relations: `relates_to`, `supports`, `contradicts`, `supersedes`,
  `derived_from`, `part_of`, `example_of`. Another value is `422 invalid`.
- Source kinds: `card` (a card of the project), `thread` (a mail thread),
  `path`, `commit`, `url`.

### 10.2 Rules

- Every member of the project reads every memory.
- Only the author, a project admin, or the operator changes `title`, `body`,
  `sources`, replaces `tags`, or removes tags. An admin edit of another
  author's memory writes the audit row `memory.edit`.
- Anyone in the project adds tags (`add_tags`) and links.
- Agents do not delete. They supersede. A project admin, a sysadmin, or the
  operator deletes (`memory.delete` in the audit log).
- A superseded memory is not in lists and search results. `?all=true`
  shows it. `GET` always shows it, with its `supersedes` edge.
- A `supersedes` link is the same as `POST .../supersede`. A memory that is
  superseded by another one cannot supersede it back (`422`). A memory that
  is superseded already gets `409 conflict`, except for an admin.
- Links stay inside one project. Only the author of a link or an admin
  removes it.

### 10.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/memories?q=&tag=&author=&all=` | | `200` memories |
| `POST /api/projects/{name}/memories` | `{"title", "body"?, "tags"?, "sources"?, "links"?}` | `201` memory and `similar` |
| `GET /api/memories/{id}` | | `200` memory |
| `PATCH /api/memories/{id}` | `{"title"?, "body"?, "tags"?, "add_tags"?, "remove_tags"?, "sources"?}` | `200` memory |
| `DELETE /api/memories/{id}` | | `204`; admins only |
| `GET /api/memories/{id}/graph?depth=` | | `200 {"root", "depth", "nodes", "edges"}` |
| `POST /api/memories/{id}/links` | `{"to", "relation", "note"?}` | `201` memory (the `from` end) |
| `DELETE /api/memories/{id}/links?to=&relation=` | or the same as a JSON body | `200` memory |
| `POST /api/memories/{id}/supersede` | `{"by"}` | `200` the superseded memory |

- Without `q`, the list is newest first. Cursor: the number.
- With `q`, the list is full-text search over title, body, and tags, best
  first. Every word must match. Cursor: an offset.
- `author` is an address (§2.3) or `me`.
- `links` on create is a list of `{"to", "relation", "note"?}` from the new
  memory.
- The create answer adds `similar`: the top 5 hits for the title, any word,
  superseded ones too: `[{"id", "title", "superseded"}]`. The CLI shows them
  so that the agent sees duplicates.
- `depth` is 1 by default and 5 at most. The graph follows links in both
  directions. `edges` are the links between the nodes in the answer.

## 11. Wiki

The wiki of a project (PLAN §15.1). The code is in
`crates/terrariumd/src/store/wiki.rs`.

### 11.1 Shapes

A page:

```json
{"id":"01M5PA0VBXNH92Q6GY3B4XNHXC","project":"proj","path":"reports/perf","title":"Perf report",
 "rev":2,"status":"reviewed","reviewer":"lead","body":"---\nstatus: reviewed\n...",
 "author":"01M5PA0T0Z0K16V6G88P3SE78W","author_name":"proj/lead","summary":"Reviewed by proj/lead",
 "created_by":"...","created_at":"...","updated_at":"...",
 "links":[{"kind":"card","target":"TER-1"},{"kind":"artifact","target":"proj/ART-2"}]}
```

- `{id}` in a path is the page ULID, `project/path` (`proj%2Freports%2Fperf`),
  or a path in the caller's project. An ID finds a deleted page too; a path
  finds live pages only. A deleted page has `deleted_at`.
- Paths are segments of `a-z`, `0-9`, `-`, `_`, `.`, joined by `/`.
- `title` is the front matter `title`, else the first `# ` heading, else the
  last path segment. A `title` in the request wins.
- Front matter (`---` lines at the top) may set `status` (`draft`,
  `ready_for_review`, `reviewed`) and `reviewer` (an agent name). Another
  status is `422 invalid`.
- `links` are the links of the current revision. `target` is
  `project/path` for a page, the card ID, `project/MEM-n`, `project/ART-n`,
  or `project/<version>`. The daemon reads `[[path]]`, `[[path|label]]`,
  `[[TER-42]]`, `[[MEM-17]]`, `[[v0.3.0]]` (a name that starts with `v` and
  a digit, or the name of a version with no page of that path),
  `[[project:other/reports/x]]`, `artifact:ART-9` in a Markdown link or
  image, and a fenced block `` ```artifact ART-11``` ``.

A revision: `{"page", "rev", "author", "author_name", "summary", "created_at",
"body"?}`. Lists of revisions have no `body`.

### 11.2 Rules

- Every member creates pages and edits any page of its project.
- Every project has an `index` page. The daemon makes it on the first list,
  or on the first `GET` of `project/index`. Its author is `system`.
- `PUT` needs `base`, the revision the edit started from. When `base` is not
  the current revision, the answer is `409 stale_base` with
  `details: {"base", "current", "diff"}`. `diff` is a unified diff from
  `base` to the current revision. Merge it, then save with the current
  revision as `base`.
- `section` replaces the text under one heading (to the next heading of the
  same or a higher level). An unknown heading adds a `## heading` section at
  the end. With `base`, the edit is refused (`409 stale_base`) only when
  that section changed since `base`; `diff` is the diff of the section.
- `append` adds text at the end. It does not conflict.
- Every revision needs a non-empty `summary` (`422 invalid`).
- Only a project admin, a sysadmin, or the operator moves or deletes a page,
  or restores a deleted page. A deleted page keeps its history. Audit rows:
  `wiki.delete`, `wiki.move`, `wiki.undelete`.
- `restore` writes revision `rev` again as a new revision.
- `review` with `approve` writes a new revision with `status: reviewed` in
  the front matter. `comment` needs `comment` (`422 comment_required`) and
  changes nothing. Both mail the author of the current revision from
  `project:<name>`, with the comment and the dashboard path. The reviewer
  is the agent named in `reviewer`, a project admin, or the operator.

### 11.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/wiki?q=&status=&deleted=` | | `200` pages without bodies |
| `POST /api/projects/{name}/wiki` | `{"path", "body", "summary", "title"?}` | `201` page |
| `GET /api/wiki/{id}` | | `200` page |
| `PUT /api/wiki/{id}` | `{"body", "base", "summary", "title"?}` | `200` page |
| `DELETE /api/wiki/{id}` | | `204`; admins only |
| `POST /api/wiki/{id}/section` | `{"heading", "body", "summary", "base"?}` | `200` page |
| `POST /api/wiki/{id}/append` | `{"body", "summary"}` | `200` page |
| `GET /api/wiki/{id}/revisions` | | `200` revisions newest first, cursor: `rev` |
| `GET /api/wiki/{id}/revisions/{rev}` | | `200` revision with `body` |
| `GET /api/wiki/{id}/diff?a=&b=` | | `200 {"page", "a", "b", "diff"}` |
| `POST /api/wiki/{id}/restore` | `{"rev"}` | `200` page |
| `POST /api/wiki/{id}/move` | `{"path"}` | `200` page; admins only |
| `GET /api/wiki/{id}/backlinks` | | `200` live pages that link to this page |
| `POST /api/wiki/{id}/review` | `{"decision": "approve" \| "comment", "comment"?}` | `200` page |

- The list without `q` is the page tree by path. Cursor: the path.
  `status=ready_for_review` gives the review queue. `deleted=true` lists the
  deleted pages.
- With `q`, the list is full-text search over the current revisions, best
  first, with a `snippet`. Cursor: an offset.
- `diff` defaults: `b` is the current revision, `a` is `b - 1`.
- A moved page keeps its ID. Links to the old path do not follow it.

## 12. Artifacts

Files and static bundles (PLAN §15.2). The code is in
`crates/terrariumd/src/store/artifacts.rs` and
`crates/terrariumd/src/api/artifacts.rs`.

### 12.1 Shapes

```json
{"id":"ART-1","key":"proj/ART-1","uid":"01M5PA19SD1TDA6NXK2M8FA79Y","project":"proj",
 "name":"report.html","content_type":"text/html; charset=utf-8","size":5120,
 "sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","kind":"file",
 "renders":true,"created_by":"...","created_by_name":"proj/w1","created_at":"...",
 "links":[{"kind":"version","ref":"01M5...","label":"v0.3.0","created_by":"...","created_at":"..."}],
 "embedded_in":["proj/reports/perf"]}
```

- IDs are project-local: `ART-<n>`. `{id}` is `ART-9`, `project/ART-9`, or
  the `uid`. `uid` is the ULID in serving URLs.
- `kind` is `file` or `bundle`. A bundle is a tar of a directory with an
  `index.html` at its root. The daemon unpacks it and serves it from
  `index.html`. Its `content_type` is `text/html; charset=utf-8`.
- `renders` is true when the viewer may show the file inline. Else the
  viewer offers a download.
- `links` kinds: `page` (`ref` is the page ID, `label` the path), `card`,
  `version` (`label` is the name). `embedded_in` lists the live pages that
  embed the artifact (§11.1).

### 12.2 Upload

`POST /api/projects/{name}/artifacts` takes one of two bodies. The data is
at most 32 MiB, and at most `max_file_bytes`.

JSON (`Content-Type: application/json`). The agent CLI uses this form
through the runner tunnel:

```json
{"name":"report.html","content_type":"text/html","data_base64":"PGh0bWw+...",
 "kind":"file","attach":{"kind":"page","ref":"reports/perf"}}
```

Multipart (`multipart/form-data`): a `file` part with the bytes (its
`filename` is the default `name`), and the optional text parts `name`,
`kind`, `content_type`, and `attach` (the JSON object above) or
`attach_kind` with `attach_ref`.

- `kind` defaults to `file`. `attach` is optional. Its `ref` is a page key,
  a version key, or a card ID of the same project.
- The daemon sniffs the content type from the bytes, then from the name, and
  fixes it. It does not trust `content_type`. An executable (ELF, PE,
  Mach-O, WebAssembly, or a `#!` script) is `application/octet-stream` and
  never renders. A binary with an unknown type does not get a type that
  renders.
- The daemon stores the bytes once per SHA-256 in
  `<state>/artifacts/<sha256>`, and a bundle in
  `<state>/artifacts/<sha256>.bundle/`. A second upload of the same bytes is
  a new artifact on the same blob.
- A bundle must be a tar of regular files and directories, with no path out
  of its root, and with `index.html` at the root. Else `422 invalid`.
- Over the per-file limit or the project quota: `413 too_large`, with
  `details` (`size`, `max_file_bytes`, or `used_bytes`, `quota_bytes`).

Limits come from `$TERRARIUM_CONFIG_DIR/daemon.toml`. The daemon reads the
file at the first artifact call; a change needs a restart.

```toml
[artifacts]
max_file_bytes = 33554432        # 32 MiB, also the maximum
project_quota_bytes = 1073741824 # 1 GiB
retention_days = 30
url_ttl_secs = 300
prune_interval_hours = 24        # the daemon's prune timer
```

The daemon runs the prune (below, with `dry_run = false`) one hour after it
starts, then every `prune_interval_hours`. The serving origin `/a/...` also
listens alone on `$TERRARIUM_ARTIFACT_LISTEN` (default `0.0.0.0:8081`).

### 12.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/artifacts` | | `200` artifacts newest first, cursor: number |
| `POST /api/projects/{name}/artifacts` | §12.2 | `201` artifact |
| `GET /api/projects/{name}/artifacts/usage` | | `200 {"project", "count", "used_bytes", "quota_bytes", "max_file_bytes"}` |
| `GET /api/artifacts/{id}` | | `200` artifact |
| `DELETE /api/artifacts/{id}` | | `204`; the author or an admin |
| `POST /api/artifacts/{id}/links` | `{"kind": "page" \| "version" \| "card", "ref"}` | `201` artifact |
| `POST /api/artifacts/{id}/url` | none, or `{"ttl"?}` | `200 {"url", "exp", "expires_at"}` |
| `POST /api/artifacts/prune` | `{"dry_run"?, "as_of"?}` | `200` prune report; sysadmin or operator |

- Every member of the project uploads, reads, and attaches. A `version`
  link follows the version edit rules (§13.2).
- `DELETE` refuses an artifact on a released version (`409 conflict`). An
  admin delete of another author's artifact writes `artifact.delete`. The
  blob goes when no artifact uses it.

### 12.4 Serving: `GET /a/{uid}/{path}`

The serving routes need no token and no actor. They need a signed URL that
`POST /api/artifacts/{id}/url` makes. The signature is an HMAC-SHA256 of
`<uid>:<exp>` with a daemon key in `<state>/artifact-url.key`. `exp` is a
Unix time. The URL lives `ttl` seconds: by default `url_ttl_secs`, at most
a day.

| Kind | URL |
|---|---|
| file | `/a/{uid}/{name}?exp=<exp>&sig=<sig>` |
| bundle | `/a/{uid}/_s/<exp>/<sig>/` and the files below it |

A bundle carries the signature in the path, so that the relative links in
its pages keep working. The daemon accepts both forms for both kinds. No
signature, a wrong one, or an expired one: `403 forbidden`.

Every answer has these headers, and no cookie:

```
Content-Security-Policy: sandbox allow-scripts allow-forms allow-popups
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cache-Control: private, max-age=60
```

The CSP sandbox gives the page an opaque origin. Scripts in a bundle run,
but they cannot reach the dashboard or the API. A type that does not render
also gets `Content-Disposition: attachment`.

The main router serves `/a/...`. `terrariumd::api::artifact_router(state)`
is the same routes alone, so that the daemon can serve them on a separate
port (8081) as a separate origin. The dashboard embeds them in
`<iframe sandbox>` without `allow-same-origin`.

### 12.5 Retention

`terrariumd::api::run_prune(state, now, dry_run, actor)` applies the rule;
`POST /api/artifacts/prune` calls it. `dry_run` defaults to `true` on the
endpoint. `as_of` is an RFC 3339 time; the default is now.

- The rule keeps an artifact that is attached to a version (released or
  not), a live page, or a card, or that a live page embeds.
- An unattached artifact older than `retention_days - 1` days gets the
  audit row `artifact.prune_warning` with `prune_after`.
- An unattached artifact older than `retention_days` whose warning is a day
  old or more is pruned: the row, the audit row `artifact.prune`, the event
  `artifact.pruned`, and the blob when no artifact uses it.
- An attach after a warning clears the warning.

```json
{"dry_run":false,"as_of":"2026-12-03T06:00:00.000Z","retention_days":30,
 "pruned":[{"key":"proj/ART-1","name":"loose","size":6,"created_at":"..."}],
 "warned":[],"kept":3}
```

## 13. Versions

Release points that the user reviews (PLAN §15.3). The code is in
`crates/terrariumd/src/store/versions.rs`.

### 13.1 Shapes

```json
{"id":"01M5PA1A0K16V6G88P3SE78WAB","project":"proj","name":"v0.1.0","status":"approved",
 "notes":"Fixed arm64.","git_ref":"abc123","thread_id":"01M5PA1A...",
 "created_by":"...","created_by_name":"proj/lead","created_at":"...","updated_at":"...",
 "submitted_at":"...","submitted_by":"...","decided_at":"...","decided_by":"operator",
 "url":"/projects/proj/versions/01M5PA1A0K16V6G88P3SE78WAB",
 "cards":[{"id":"TER-1","title":"Parser","column":"done"}],
 "artifacts":[{"id":"ART-1","uid":"...","name":"report.txt","kind":"file",
               "content_type":"text/plain; charset=utf-8","size":6}],
 "reviews":[{"id":"...","author":"...","author_name":"proj/lead","decision":"submitted",
             "comment":"Please look.","created_at":"..."}]}
```

- `{id}` is the ULID, `project/name` (`proj%2Fv0.1.0`), or a name in the
  caller's project.
- Names are 1 to 63 of `A-Z`, `a-z`, `0-9`, `.`, `_`, `-`, and start with a
  letter or a digit. A name is unique in its project.
- Statuses: `draft`, `in_review`, `approved`, `rejected`, `released`.
- `reviews` decisions: `submitted`, `approved`, `rejected`, `released`.
- `url` is the dashboard page of the version.

### 13.2 Rules

- Every member creates versions. The creator, a project admin, or the
  operator edits and submits one.
- Edits (`PATCH`, artifact links) happen in `draft` and `rejected` only.
  Else `409 conflict`.
- `submit` (from `draft` or `rejected`) sets `in_review` and mails the
  operator from `project:<name>`: the dashboard path, the comment, the
  notes, the cards, and the artifacts. The mail starts the review thread
  (`thread_id`). A second submit posts on the same thread.
- Only the operator approves or rejects, and only `in_review`. A rejection
  needs `comment` (`422 comment_required`). The decision writes a review,
  the audit row `version.approve` or `version.reject`, a comment on every
  linked card, and a mail to the submitter on the review thread.
- `release` needs a project admin, a sysadmin, or the operator, and an
  `approved` version. It writes `version.release` in the audit log.
- A released version is immutable: every change is `409 conflict`. Its
  artifacts cannot be deleted, and retention keeps them.

### 13.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/versions?status=&name=&wait=` | | `200` versions newest first, cursor: ID |
| `POST /api/projects/{name}/versions` | `{"name", "notes"?, "repo"?, "git_ref"?, "cards"?, "artifacts"?}` | `201` version |
| `GET /api/versions/{id}` | | `200` version |
| `PATCH /api/versions/{id}` | version patch | `200` version |
| `POST /api/versions/{id}/submit` | none, or `{"comment"?}` | `200` version |
| `POST /api/versions/{id}/approve` | none, or `{"comment"?}` | `200` version; operator |
| `POST /api/versions/{id}/reject` | `{"comment"}` | `200` version; operator |
| `POST /api/versions/{id}/release` | none, or `{"comment"?}` | `200` version; admins |
| `GET /api/versions/{id}/compare?to=` | | `200` compare |

- `wait` (seconds, at most 300) holds an empty list until a `version.*`
  event, for `terrarium version wait` (`docs/agent-rpc.md`).
- The patch: `name`, `notes`, `repo`, `git_ref` (an empty string clears
  it), `cards` or `artifacts` (replace the set), `add_cards`,
  `remove_cards`, `add_artifacts`, `remove_artifacts`. Cards and artifacts
  must be in the version's project.

Compare:

```json
{"version":"v2","to":"v1","since":"...","until":"...",
 "cards":{"added":["TER-2"],"removed":["TER-1"],"landed":["TER-2"]},
 "artifacts":{"added":["ART-1"],"removed":[]},
 "pages":[{"id":"...","path":"notes","from_rev":1,"to_rev":2},{"id":"...","path":"new-page","to_rev":1}]}
```

- The time of a version is `released_at`, else `submitted_at`, else now.
  `since` and `until` are the two times, earlier first.
- `cards.added` and `cards.removed` compare the linked cards.
  `cards.landed` are the cards of the project that moved to `done` between
  the two times.
- `pages` are the live pages whose latest revision changed between the two
  times. A new page has no `from_rev`. Use `GET /api/wiki/{id}/diff` for the
  text.

## 14. Not in this phase

These §17 endpoints are not in the daemon yet: `POST /api/repos/{id}/checkout`,
subscriptions and usage, and target
actions (`POST /api/targets`, `.../up|down|snapshot|restore`). They answer
`404 not_found`, or `405 method_not_allowed` where the path exists with
another method (`POST /api/targets`).

## 15. Notes for clients

- The CLI decodes `GET /api/agents` as `Vec<terrarium_core::Agent>`. The
  extra fields do not break it.
- The dashboard (`dashboard/src/api/`) gets these shapes as it expects them:
  lists are JSON arrays; an event has `agent` as the name and
  `data.agent_id`; a `status` event has `data.status`; a `card` event has
  `data.card_id`; the PTY protocol is as in §6.5; WebSockets take `?token=`.
- These shapes differ from the dashboard types today:
  - `POST /api/mail/{id}/reply` and the inbox reply answer an **array** of
    messages, because a reply goes to each participant.
  - An audit row's `detail` is a JSON object, not a string.
  - A session has `outcome`, and no `ended_at` while it is current.
  - A card comment has `id` and `author_name`; a card has `assignee_name`,
    `created_by_name`, `blocked_by`, and `archived`.
  - A column has `count`.

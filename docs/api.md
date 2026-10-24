# Terrarium daemon API

This document is the contract between `terrariumd` and its clients: the CLI,
the dashboard, and (later) the agent runner. The code is in
`crates/terrariumd/src/api/`. The integration tests in
`crates/terrariumd/tests/` exercise each group. Change this file in the same
change as the code.

The design is in [PLAN.md](PLAN.md) §12, §13, and §17.

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
- `agent`: a Unix-socket caller that sends the header
  `X-Terrarium-Agent: <agent>`. The value is an agent ULID, `project/name`,
  or a unique agent name. TCP ignores this header. The header is for tests
  now. Later the runner socket supplies the agent identity.

An unknown agent in `X-Terrarium-Agent` gets `401 unauthorized`.

Examples in this document use the Unix socket:

```sh
S=$XDG_RUNTIME_DIR/terrarium/api.sock
curl -s --unix-socket $S http://localhost/api/health
curl -s --unix-socket $S -H 'X-Terrarium-Agent: terrarium/worker' http://localhost/api/mail
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
| 401 | `unauthorized` | No token or a wrong token on TCP; an unknown agent in `X-Terrarium-Agent`. |
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
| 422 | `invalid` | Well-formed values that break a rule: a bad name, an empty title, a missing column. |
| 422 | `comment_required` | A move to `blocked` without a comment. |
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
on a card that is not its own), `definitions.sync`.

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
- `send` types one turn. A stopped agent gets `409 not_running`.
- `answer` answers a question (`Event::Question`). `answer` is
  `{"kind":"choice","value":0}`, `{"kind":"allow"}`, or `{"kind":"deny"}`.
  `{"text"}` types raw keys and Enter, for a dialog the adapter does not know.

The daemon runs agents through the `Supervisor` trait
(`crates/terrariumd/src/supervisor.rs`). The binary uses the in-memory fake
for now: `start` goes `starting` -> `idle`, `send` and `answer` go
`working` -> `idle`, each step after `TERRARIUM_FAKE_DELAY_MS` (300 ms). The
fake PTY echoes its input.

### 6.3 Sessions

`GET /api/agents/{id}/sessions`: newest first. Cursor: session ID.

```json
[{"id":"01M5PA0T1NSYX43DDK1M2KT8KR","agent":"01M5PA0T0Z0K16V6G88P3SE78W",
  "harness_session_id":"fake-01M5PA0TB2294ZW89MNK4G26EZ","current":true,
  "started_at":"2026-10-24T06:36:54.197Z","turns":1}]
```

An ended session adds `ended_at` and `outcome`: `stopped`, `restarted`,
`error`, `start_failed`, `superseded`, or the reason the supervisor gave.
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
| `GET /api/mail?to=&from=&thread=&state=` | | `200` messages oldest first, cursor: message ID |
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
| `GET /api/projects/{name}/inbox?unread=&state=&thread=` | | `200` messages | inbox right |
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
| `GET /api/projects/{name}/cards?column=&assignee=&unassigned=&label=&archived=` | | `200` cards by number, cursor: number |
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

## 10. Not in this phase

These §17 endpoints are not in the daemon yet: `POST /api/repos/{id}/checkout`,
memories, wiki, artifacts, versions, subscriptions and usage, and target
actions (`POST /api/targets`, `.../up|down|snapshot|restore`). They answer
`404 not_found`, or `405 method_not_allowed` where the path exists with
another method (`POST /api/targets`).

## 11. Notes for clients

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

# Terrarium daemon API

This document is the contract between `terrariumd` and its clients: the CLI,
the dashboard, and (later) the agent runner. The code is in
`crates/terrariumd/src/api/`. The integration tests in
`crates/terrariumd/tests/` exercise each group. Change this file in the same
change as the code.

The design is in [PLAN.md](PLAN.md) §12 to §17, and §19.

Sections: [1 Transports](#1-transports-and-callers) ·
[2 Conventions](#2-conventions) · [3 Roles](#3-roles-and-rules) ·
[4 System](#4-system) · [5 Projects and repos](#5-projects) ·
[6 Agents](#6-agents) · [7 Mail](#7-mail) · [8 Board](#8-board) ·
[9 Events](#9-events) · [10 Memory](#10-memory-graph) · [11 Wiki](#11-wiki) ·
[12 Artifacts](#12-artifacts) · [13 Versions](#13-versions) ·
[14 Not in this phase](#14-not-in-this-phase) ·
[15 Notes for clients](#15-notes-for-clients) ·
[16 Subscriptions and usage](#16-subscriptions-and-usage) ·
[17 Targets, hosts, and exec](#17-targets-host-agents-and-exec-sessions) ·
[18 Fleet](#18-fleet).

## 1. Transports and callers

The daemon serves one router on two listeners.

| Listener | Address | Authentication |
|---|---|---|
| Unix socket | `$TERRARIUM_API_SOCKET` | None. The caller is the operator. |
| TCP | `$TERRARIUM_LISTEN` | `Authorization: Bearer $TERRARIUM_TOKEN` on `/api/*`, or the session cookie of `POST /api/session` (§1.1). Without a token the TCP listener stays off. |
| TCP, host agents | `$TERRARIUM_HOST_LISTEN` (default `auto`: `0.0.0.0:8082` with TLS, `127.0.0.1:8082` without) | `GET /hosts/connect` only, with a per-host token (§17.1). TLS when `TERRARIUM_HOST_TLS_CERT` and `TERRARIUM_HOST_TLS_KEY` are set. |

A browser cannot set headers on a WebSocket. A WebSocket upgrade on TCP
sends the token as a subprotocol: `Sec-WebSocket-Protocol: bearer.<token>`
(in a browser, `new WebSocket(url, ["bearer." + token])`). The header can
list other subprotocols too. The daemon takes the first `bearer.` entry and
echoes that entry back in the `101` answer, as RFC 6455 requires, or the
browser drops the connection. The token must use only characters that a
subprotocol allows: letters, digits, and ``!#$%&'*+-.^_`|~``. A token from
`openssl rand -hex 32` is fine.

The older form, the `?token=` query parameter on an upgrade request, is
deprecated and **off by default**: a query string reaches proxy access logs
and browser history (`docs/security-review.md`, F4). A request with it gets
`401 unauthorized`. `TERRARIUM_WS_QUERY_TOKEN=1` (or `--ws-query-token 1`)
turns it back on for an old client; the daemon then logs a warning once per
client address. The CLI (`agent attach`) sends `Authorization` on its
upgrade. The subprotocol and `?token=` count on upgrade requests only; a
plain request needs `Authorization` or the cookie.

The token compare is constant time, also in the length: the daemon compares
SHA-256 hashes of the two values.

### 1.1 Dashboard sessions (cookie)

The dashboard trades the token for a cookie once, so that no script on the
page can read a credential (`docs/security-review.md`, F3). The CLI keeps
the bearer token; nothing changes for it.

| Endpoint | Auth | Answer |
|---|---|---|
| `POST /api/session` | `Authorization: Bearer <token>` only (not the cookie) | `201 {"csrf", "exp", "expires_at"}` and `Set-Cookie` |
| `DELETE /api/session` | none without a session cookie; with one, `X-CSRF-Token` of that session | `204` and a `Set-Cookie` that clears the cookie |

```
Set-Cookie: terrarium_session=v1.<exp>.<nonce>.<sig>; Path=/; Max-Age=<ttl>; HttpOnly; SameSite=Strict[; Secure]
```

- `Secure` is set when the request is HTTPS or a proxy sends
  `X-Forwarded-Proto: https`.
- The life is `TERRARIUM_SESSION_TTL` (`--session-ttl`): seconds, or a
  number with `s`, `m`, `h`, or `d`. The default is `12h`. `exp` is a Unix
  time; `expires_at` is the same time in RFC 3339.
- The cookie is stateless. `sig` is an HMAC-SHA256 of the version, `exp`,
  and `nonce` with the key in `<state>/session.key` (32 random bytes as
  hex, mode 0600; the daemon makes it at the first start). The `csrf`
  value is an HMAC of the nonce with the same key. To end every session,
  delete or replace `session.key` and restart the daemon. A logout
  (`DELETE` with the session cookie and its `X-CSRF-Token`) ends that one
  session on the server too: the daemon keeps its nonce in
  `revoked_sessions` until `exp`, and a copy of the cookie gets `401`.
- `DELETE /api/session` does not use the auth of the other routes. It
  looks at the cookie itself, so that a logout from another site (logout
  CSRF) fails:

  | Request | Answer |
  |---|---|
  | No `terrarium_session` cookie, with or without `Authorization: Bearer` | `204` and the clearing `Set-Cookie`. A client that lost its cookie can log out again. |
  | A valid session cookie and the right `X-CSRF-Token` | `204` and the clearing `Set-Cookie`. The session is revoked. |
  | A valid session cookie, and no `X-CSRF-Token` or a wrong one (the token of another session too) | `403 forbidden`, no `Set-Cookie`. The cookie stays. |
  | A cookie that is wrong, from another key, revoked, or past `exp` | `204` and the clearing `Set-Cookie`. It is no session, so to clear it ends nothing. |

- The answers carry `Cache-Control: no-store`. The Unix socket has no
  sessions (`404 not_found`).

The cookie authenticates every `/api` route and WebSocket upgrade on TCP,
as the token does. Order: `Authorization`, then (on an upgrade) the
`bearer.` subprotocol, then the cookie, then (when on) `?token=`. A wrong
token is `401` even with a good cookie. With the cookie:

- `POST`, `PUT`, `PATCH`, and `DELETE` must send `X-CSRF-Token: <csrf>`.
  No header or a wrong one: `403 forbidden`. `GET` needs no header.
- A WebSocket upgrade must send an `Origin` that is the dashboard origin:
  the origin of `TERRARIUM_PUBLIC_URL`, or `http(s)://<Host>` of the
  request itself. No `Origin`, `null`, or another origin: `403 forbidden`.
  The upgrade needs no subprotocol.
- A cookie that is wrong, from another key, revoked, or past `exp`: `401
  unauthorized`. The dashboard then calls `POST /api/session` again.

### 1.2 Security headers

Every answer on the TCP listener, except a served artifact (§12.4), has:

```
Content-Security-Policy: <policy>
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
X-Frame-Options: DENY
```

On `/api/*` the policy is `default-src 'none'; frame-ancestors 'none'`. On
the dashboard pages and assets it is the policy of the dashboard's `<meta>`
tag (`dashboard/vite.config.ts`, `dashboardCsp`) and `frame-ancestors
'none'`:

```
default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline';
img-src 'self' data: <art>; font-src 'self'; connect-src 'self' <art>;
frame-src <art or 'self'>; worker-src 'none'; manifest-src 'self';
media-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none';
frame-ancestors 'none'
```

`<art>` is the artifact origin as the daemon knows it at run time, the same
origin that signed URLs name (§12.4). Without an artifact listener it is
absent and `frame-src` is `'self'`.

### 1.3 Web settings

The daemon reads these once, as a flag or an env var (`Settings::web`), and
gives them to the routers and the mail code.

| Env | Flag | Default | Use |
|---|---|---|---|
| `TERRARIUM_PUBLIC_URL` | `--public-url` | unset (mail: `http://localhost:8080`) | Mail links, `Version.url`, the cookie `Origin` check, CORS on `/a/...`. |
| `TERRARIUM_ARTIFACT_LISTEN` | `--artifact-listen` | `0.0.0.0:8081` | The artifact origin listener. Empty: none, and the dashboard port serves `/a/...`. |
| `TERRARIUM_ARTIFACT_PUBLIC_URL` | `--artifact-public-url` | unset | The artifact origin in signed URLs and the CSP. |
| `TERRARIUM_WS_QUERY_TOKEN` | `--ws-query-token` | `0` | `1`: take `?token=` on an upgrade. |
| `TERRARIUM_SESSION_TTL` | `--session-ttl` | `12h` | The life of a session cookie. |

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
| `system` | The daemon: quota alerts, blocked-card notices, and expired logins. It only sends. No caller can send to it or as it. A reply on its thread goes to the other participants. |
| `project:<name>` | The inbox of a project. |
| `<ulid>`, `project/name`, `name` | An agent. A bare name looks in the sender's project first. |

Responses carry the stored address (`from`, `to`: `operator`,
`project:<name>`, `system`, or the agent ULID), its kind (`from_kind`:
`operator`, `system`, `project`, `agent`; `to_kind`: the same without
`system`), and a readable name (`from_name`,
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
| 429 | `rate_limited` | Too many calls of one kind; try again later. The credential helper (§5.1) has a limit per agent. An agent has too many live exec sessions (§17). A host agent runs its maximum of sessions. |
| 501 | `not_available` | The route exists, but this phase does not do the request. |
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
| Check out a repo into its own workspace; ask the credential helper (§5.1) | own project | own project | own project |
| Check out a repo into another agent's workspace | no | own project | any |
| Store, list, or delete credentials (write-only) | no | no | yes |
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

### `GET /api/targets` and `GET /api/hosts`

Both answer the real lists: the targets from the files and the API, and
the registered host agents with their live link state. The shapes and the
rules are in §17.

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
`repo.checkout`, `repo.credential` (each answer of the credential helper),
`credential.put`, `credential.delete` (§5.1),
`board.columns`, `card.reassign`, `card.reopen`, `card.archive` (by an admin
on a card that is not its own), `definitions.sync`, `memory.edit` and
`memory.delete` (§10), `wiki.delete`, `wiki.move`, `wiki.undelete` (§11),
`artifact.delete`, `artifact.prune_warning`, `artifact.prune` (§12),
`version.approve`, `version.reject`, `version.release` (§13),
`host.create`, `host.revoke`, `target.create`, `target.update`,
`target.delete`, `targets.sync`, `target.exec`, `target.cp`, `target.up`,
`target.down`, `target.snapshot`, `target.restore`, `target.status` (§17).

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

Attach a repo. Attaching does not clone; a clone happens on a checkout or
on agent create (§5.1). Without `name`, the last part of the URL names it.

```
POST /api/projects/ops/repos
{"url":"https://github.com/wiltaylor/infra.git","default_branch":"main","credential":"github-deploy","path":"infra"}

201
{"id":"01M5PA0T0TH91WNYTGH66DA2SP","project":"ops","name":"infra","url":"https://github.com/wiltaylor/infra.git",
 "default_branch":"main","credential":"github-deploy","path":"infra","source":"api","created_at":"2026-10-24T06:36:54.170Z"}
```

`credential` is a key name in `/state/credentials`, never the secret.

### 5.1 Repos, checkouts, and credentials

PLAN §9 and §21. The daemon clones and fetches with the `git` binary,
outside every sandbox. The agent never gets a clone secret on disk or in its
env. After a clone, the daemon writes this into the workspace repo's
`.git/config`:

```ini
[credential]
	helper =
	helper = !terrarium repo credential
	useHttpPath = true
```

So a `git push` inside the sandbox asks `terrarium repo credential get`, which
calls `POST /api/repos/credential` through the runner socket.

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/repos/{id}` | | `200` repo | read |
| `POST /api/repos/{id}/checkout` | below | `200` checkout result | own workspace: read; another agent's: project_admin |
| `POST /api/repos/credential` | `{"protocol", "host", "path"?}` | `200` `{"username", "password"}` | an agent |
| `GET /api/credentials` | | `200` `[{"name", "kind", "username"?, "created_at"}]` | sysadmin |
| `POST /api/credentials` | `{"name", "kind", "value", "username"?}` | `201` credential, without `value` | sysadmin |
| `DELETE /api/credentials/{name}` | | `204` | sysadmin |

`{id}` is the repo ULID, `project/name`, or (for an agent) a repo name in
its own project.

**Checkout.** The body is `{"target"?, "branch"?, "path"?, "depth"?,
"agent"?}`. All fields are optional.

- `target`: the checkout runs on the target, not in a workspace (§17.5).
  `agent` is ignored.
- `agent`: the agent whose workspace gets the checkout. An agent caller
  defaults to itself. The operator must name one (`422 invalid` if not).
  The repo and the agent must be in the same project (`403`).
- `path`: one directory name under the workspace. The default is the repo's
  `path`. In the sandbox it is `$HOME/work/<path>`.
- `branch`: the default is the repo's `default_branch`, else the remote's
  default on a clone and the current branch on an update.
- No checkout yet: `git clone --no-hardlinks`, then the config above.
- A checkout is there: the daemon fetches the branch from the stored URL
  (not from `origin`), then fast-forwards when the work tree is clean. Local
  changes or a diverged branch give `not_updated` with a `note`. The daemon
  first reads the repo's local config as a file. It refuses (`409
  conflict`) keys that could run code or move the secret, for example
  `core.hooksPath`, `filter.*`, `url.*.insteadOf`, `http.proxy`, and
  `include.path`.
- A URL that is a local path in the state dir, a relative path, or a
  remote-helper URL (`ext::`) gets `409 conflict`.
- git fails: `422 invalid` with `details: {"command", "status", "stderr"}`.
  A run is limited to 10 minutes.

```
POST /api/repos/01M5PA0T0TH91WNYTGH66DA2SP/checkout
{"branch":"main"}

200
{"repo":"infra","repo_id":"01M5PA0T0TH91WNYTGH66DA2SP","project":"ops","agent":"worker",
 "agent_id":"01M5PA0T0Z0K16V6G88P3SE78W","path":"infra","action":"cloned",
 "head":"3f2c1e0b9a...","branch":"main"}
```

`action` is `cloned`, `updated`, `up_to_date`, or `not_updated`. `head` and
`branch` are `null` when the repo has no commit or HEAD is detached.

Each checkout writes `repo.checkout` events on the agent: `data.phase` is
`progress` (git progress lines, at most one per phase each second), `done`
(`data.result` is the answer above), or `error` (`data.error`). A `done`
also writes a `repo.checkout` audit row.

**On agent create.** `POST /api/agents` clones each repo in the project
file's `[checkout] on_agent_create` list into the new workspace, after the
agent row is stored and before any start. The answer has
`checkout: [<checkout result> | {"repo", "error", "details"?}]`, or no
`checkout` when the list is empty. A failure
is a `repo.checkout` error event; the agent is still created. Only a project
file has a `[checkout]` table. Agents from files (`POST /api/reload`) do
not get this clone.

**Credential helper.** `POST /api/repos/credential` takes the fields that
git sends to a credential helper. The caller must be an agent (`403` for
the operator). The daemon answers only when a repo of the agent's own
project has an `https` or `http` URL with the same protocol, host
(with port), and path (`.git` and slashes at the ends do not count), and a
credential of kind `token`. Every other case is `404 not_found`, so git
falls back to its next helper. An SSH key is never answered. Each answer
writes a `repo.credential` audit row (without the secret). An agent gets at
most 30 answers a minute (`429 rate_limited`).

```
POST /api/repos/credential          (agent)
{"protocol":"https","host":"github.com","path":"wiltaylor/infra.git"}

200
{"username":"x-access-token","password":"ghp_..."}
```

**Credentials.** The store is `<state>/credentials/`, mode 0700. The secret
is `<name>` (mode 0600). The metadata, without the secret, is `<name>.json`.
No endpoint gives the secret back, except the credential helper above.
`kind` is `token` or `ssh_key`. A token is one line. A token's `username`
defaults to `x-access-token`. An `ssh_key` is a private key in OpenSSH or
PEM format, and has no `username`. A `POST` with an existing name replaces
the credential.

For one clone or fetch, the daemon puts the secret in a 0700 run dir under
`<state>/credentials/.run/`, and removes the dir after the run. A token goes
to git through a one-shot helper script that answers only for the repo's
protocol and host. An SSH key goes to `ssh -i` through `GIT_SSH_COMMAND`.
The secret is never on a command line, in an env variable, or in the
repo's `.git/config`.

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
| `source` | `file`, `api`, or `fleet:<name>` (§18.1). |

### 6.1 CRUD

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/agents?project=` | | `200` agents by ID | read; an agent defaults to its project |
| `POST /api/agents` | an agent definition | `201` agent | project_admin of `project`; some `policy` entries need sysadmin (below) |
| `GET /api/agents/{id}` | | `200` agent | read |
| `PATCH /api/agents/{id}` | `{"harness"?, "subscription"?, "model"?, "prompt"?, "policy"?, "restart"?, "new_session_on_prompt_change"?}` | `200` agent | project_admin; some `policy` entries need sysadmin (below) |
| `DELETE /api/agents/{id}` | `?confirm=true` when live | `204` | project_admin |

`DELETE` stops a live agent, waits for it to end (up to 10 s), and moves
its dir `<state>/agents/<ulid>/` to `<state>/trash/<ulid>-<ts>/`. The
operator can still read the transcripts and the event log there. The daily
prune timer removes a trash entry older than `TERRARIUM_TRASH_DAYS` days
(default `7`).

The definition is the TOML agent definition as JSON:

```json
{"name":"worker","project":"terrarium","harness":"pi","model":"anthropic/claude-opus-5-5",
 "roles":[],"policy":{"targets":[],"mail_to":["*"],"cross_project":["ops/triage"]},"restart":"on-failure"}
```

`new_session_on_prompt_change` (default `true`): when the rendered prompt
changed since the harness session began, and the harness keeps the prompt
of a session (Codex), `start` and a restart begin a new harness session
instead of a resume. The status reason is `new session: the prompt
changed`. The old session stays in the history.

`PATCH` changes only the fields in the body. Send `null` for
`subscription`, `model`, or `prompt` to clear it (`{"subscription": null}`).
`null` for the other fields changes nothing.

`roles` in the definition need the grant rights of §3. `policy.cross_project`
entries are `project/agent`.

A `policy` write needs project_admin of the project. Some new entries need
a sysadmin or the operator (`docs/security-review.md` F13):

- a `targets` entry that is not a target of the project (a global target),
- a `cross_project` entry,
- a `mail_to` entry of the form `project/name` for another project.

An entry that the policy has already may stay. Else the answer is
`403 forbidden`, and the message names each entry. A fleet apply uses the
same check (§18.4). The project must exist (`not_found`). A file
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
- `stop` types the adapter's exit keys into an idle harness that has them
  (pi: `/quit`), and kills it when it has not exited after 5 s. Other
  harnesses are killed at once.
- `stop` on an agent that is not running gets `409 not_running`. When the
  store says live and the supervisor says not running, `stop` sets `stopped`.
- `restart` ends the current session with outcome `restarted` and starts a
  new one. On a stopped agent it is the same as `start`.
- `send` types one turn through the runner: it waits until the agent is
  `idle` and the prompt is ready. A stopped agent, or one between an exit
  and its restart, gets `409 not_running`.
- `start` of an agent whose harness adapter is not registered, or whose
  prompt template or harness home is missing, gets `422 invalid` with the
  reason.
- `start` of an agent whose subscription does not exist, or whose harness
  is not in the subscription's `harnesses` (PLAN §16, policy hooks), gets
  `422 invalid` with the reason. A `pi` agent on a Claude plan login (not an
  API key) gets `422 invalid` too: pi cannot spend a Pro/Max plan. An agent with no subscription starts; the
  daemon logs it. A restart and the relaunch after a daemon start apply the
  same check.
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

Each agent runs in its own `terrarium-runner` process
(`TERRARIUM_RUNNER_MODE=process`, the default). When the daemon stops, the
runner processes and their harnesses keep running. When the daemon starts,
it reconciles each agent whose stored status is live or `restarting`:

- A live runner process: the daemon connects to it again. The harness does
  not restart. The events of the downtime come in once, in order. No new
  session begins.
- A runner that ended while the daemon was down: the restart policy decides
  on its exit status (`runner-exit.json`). With no restart, the status
  becomes `stopped` or `error`, with the reason `... while the daemon was
  down`.
- No runner and no exit status (the container restarted), or a restart:
  the daemon launches the agent again, with `--resume` of its last harness
  session. A new session begins (outcome `daemon_restart`).

With `TERRARIUM_RUNNER_MODE=inprocess` (tests and development), the daemon
ends its harnesses when it stops, and launches them again with resume when
it starts.

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
  `system`, on one new thread. Subject: `<card> blocked: <title>`.
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
| `usage.sample`, `usage.rejected_path`, `quota.sample`, `quota.alert`, `quota.blocked`, `quota.resumed`, `subscription.offer`, `subscription.expired`, `self_report` | See §16.7. |
| `host.connected`, `host.disconnected` | `{host, host_id, ...}`: `version`, `os`, `arch`, `capabilities`, `summary`, `resumed` (sessions taken back), `reason`. See §17. |

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
| `since` | A `seq`, an event ID, or `latest`. Only later events come. Default: 0, the whole log. `latest` skips the backlog: only events after the newest `seq` at connect time come. |
| `live` | `1` or `true`: the same as `since=latest`. It wins over `since`, a cursor, and `Last-Event-ID`. `0`, `false`, or empty: off. Another value is `400`. |
| `kind` | One kind. |
| `project` | Firehose only: one project. |
| `agent` | Firehose only: an agent key or ULID. A ULID works after the agent is deleted. |

The firehose shows an agent (not a sysadmin) only the events of its project
and its own events. To resume after a disconnect, reconnect with
`since=<last seq>`.

`GET /api/events/latest` answers `200 {"seq": 1234}`: the newest `seq` of the
log, `0` when the log is empty. A client that shows recent activity reads a
short JSON page (for example `?since=<seq - 50>`), then streams with
`since=<seq>`. It does not replay the whole log.

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
  viewer offers a download. A bundle always renders, in a sandboxed frame.
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
| file | `<origin>/a/{uid}/{name}?exp=<exp>&sig=<sig>` |
| bundle | `<origin>/a/{uid}/_s/<exp>/<sig>/` and the files below it |

The artifact origin is a listener of its own, `TERRARIUM_ARTIFACT_LISTEN`
(default `0.0.0.0:8081`), so that an uploaded page is never same-origin with
the dashboard (`docs/security-review.md`, F5). With that listener on, the
dashboard port (`TERRARIUM_LISTEN`) does **not** serve `/a/...`, and
`<origin>` is `TERRARIUM_ARTIFACT_PUBLIC_URL` when it is set, else the host
of the mint request (`X-Forwarded-Host`, else `Host`, without its port) with
the port of `TERRARIUM_ARTIFACT_LISTEN`, and `https` when
`X-Forwarded-Proto` is `https`. Set `TERRARIUM_ARTIFACT_PUBLIC_URL` behind a
reverse proxy, for example `https://artifacts.example.com`. With
`TERRARIUM_ARTIFACT_LISTEN=` or `--artifact-listen ""` (empty), there is no
artifact listener: the dashboard port serves `/a/...` and `<origin>` is
empty (a relative URL). The flag and the env var are the same setting
(§1.3). The Unix socket always serves `/a/...`.

A bundle carries the signature in the path, so that the relative links in
its pages keep working. The daemon accepts both forms for both kinds. No
signature, a wrong one, or an expired one: `403 forbidden`.

Every answer has these headers, and no cookie:

```
Content-Security-Policy: sandbox allow-scripts allow-forms allow-popups; connect-src 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cache-Control: private, max-age=60
Access-Control-Allow-Origin: <dashboard origin>
Vary: Origin
```

The answer keeps its own `sandbox` CSP and has no `X-Frame-Options`, also
on the dashboard port, so that the viewer can frame it (§1.2).

CORS lets the dashboard `fetch` a text or markdown artifact from the
artifact origin (no credentials, no preflight). `Access-Control-Allow-Origin`
is the request's `Origin` when that is the origin of `TERRARIUM_PUBLIC_URL`,
or an `http(s)` origin on the same host as the artifact request (the
dashboard on another port of the same host, the default layout). Else it is
the `TERRARIUM_PUBLIC_URL` origin, or the header is absent. It is never `*`
or `null`, and there is no `Access-Control-Allow-Credentials`. A `403`
answer has the header too, so that the viewer can read the error.

The CSP sandbox gives the page an opaque origin. Scripts in a bundle run,
but they cannot reach the dashboard or the API. `connect-src 'none'` stops
`fetch`, XHR, WebSocket, and `sendBeacon` from the page, so a script cannot
send data out that way; a bundle cannot load its own JSON with `fetch`
either. A type that does not render
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
 "url":"https://terrarium.example.com/p/proj/versions/01M5PA1A0K16V6G88P3SE78WAB",
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
- `url` is the dashboard page of the version, `/p/<project>/versions/<id>`:
  absolute on `TERRARIUM_PUBLIC_URL` when it is set (the same link as the
  review mail, §13.2), else the path alone, which the dashboard opens on
  its own origin.

### 13.2 Rules

- Every member creates versions. The creator, a project admin, or the
  operator edits and submits one.
- Edits (`PATCH`, artifact links) happen in `draft` and `rejected` only.
  Else `409 conflict`.
- `submit` (from `draft` or `rejected`) sets `in_review` and mails the
  operator from `project:<name>`: the dashboard link
  (`<TERRARIUM_PUBLIC_URL>/p/<project>/versions/<id>`; the default of
  `TERRARIUM_PUBLIC_URL` is `http://localhost:8080`), the comment, the
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

Every PLAN §17 endpoint is in the daemon now. Targets, host agents, and
exec sessions are in §17 of this document.

## 15. Notes for clients

- The CLI decodes `GET /api/agents` as `Vec<terrarium_core::Agent>`. The
  extra fields do not break it.
- The dashboard (`dashboard/src/api/`) gets these shapes as it expects them:
  lists are JSON arrays; an event has `agent` as the name and
  `data.agent_id`; a `status` event has `data.status`; a `card` event has
  `data.card_id`; the PTY protocol is as in §6.5; WebSockets take the token
  as the subprotocol `bearer.<token>` (§1; `?token=` is deprecated).
- These shapes differ from the dashboard types today:
  - `POST /api/mail/{id}/reply` and the inbox reply answer an **array** of
    messages, because a reply goes to each participant.
  - An audit row's `detail` is a JSON object, not a string.
  - A session has `outcome`, and no `ended_at` while it is current.
  - A card comment has `id` and `author_name`; a card has `assignee_name`,
    `created_by_name`, `blocked_by`, and `archived`.
  - A column has `count`.

## 16. Subscriptions and usage

Plans, quota probes, local accounting, the quota policy, and the login
wizard (PLAN §16). The code is in `crates/terrariumd/src/api/subscriptions.rs`,
`api/usage.rs`, `store/subscriptions.rs`, `store/usage.rs`, `probes/`,
`usage_collector.rs`, `quota_policy.rs`, and `login_session.rs`. The tests
are in `crates/terrariumd/tests/subscriptions.rs` and `tests/usage_cards.rs`.

### 16.1 Shapes

A subscription:

```json
{"name":"anthropic-max","provider":"anthropic","plan":"Max 20x","harnesses":["claude"],
 "probe":"anthropic-oauth-usage","alert_at":80,"source":"file","login_status":"logged_in",
 "verified_at":"...","probe_status":"ok","last_probe_at":"...","last_success_at":"...",
 "created_at":"...","updated_at":"...",
 "windows":[{"window":"5h","used_pct":16.0,"resets_at":"2026-10-24T10:59:59Z","at":"..."},
            {"window":"7d","used_pct":10.0,"resets_at":"2026-10-31T01:59:59Z","at":"..."}],
 "local_estimate_only":false,"has_api_key":false,"held_agents":0}
```

| Field | Meaning |
|---|---|
| `source` | `file` (`subscriptions/<name>.toml`), `api` (the API, the login wizard, an API key), or `fleet:<name>` (a fleet file, §18.1). A file subscription whose file is gone becomes `api` on reload; its samples stay. |
| `login_status` | `logged_in`, `expiring`, `expired`, `missing`, or `unknown` (not checked yet). See "Expired logins" in §16.2. |
| `probe_status` | `ok`, `failed` (`probe_error` says why), or `none` (no probe, or not run yet). `last_success_at` stays when a probe fails. |
| `windows` | The newest sample of each window. Window names: `5h`, `7d`, `7d_opus`, `7d_sonnet` (Anthropic); `<n>h` or `<n>d` from the window length (OpenAI); `monthly` (Copilot premium requests). |
| `local_estimate_only` | `true` when there is no probe: local accounting is the only number. |
| `env_var` | Set when an API key is stored: the env var that carries it into the agents. The key is never in the API. |
| `held_agents` | Agents whose turns wait for a reset of this subscription. |

Names follow the project name rules; `login` and `detect` are reserved.

Probes: `anthropic-oauth-usage`, `openai-codex-usage` (alias
`codex-rate-limits`), `github-copilot-quota` (alias
`copilot-premium-requests`). `docs/compat.md`, "Usage probes", records
each endpoint.

A usage point (`GET /api/usage`):

```json
{"bucket":"2026-10-24T06:00:00Z","key":"claude-haiku-5-5",
 "input_tokens":4,"output_tokens":55,"cache_read":36712,"cache_write":36983,"messages":2,"turns":1}
```

`key` is absent when the series is not split. With `group`, it is the
value of that column: the subscription name, the project name, the agent
**ULID** (not the name; the ULID stays when the agent is deleted), or the
model. `messages` counts API responses; `turns` counts distinct turns (the
`seq` of the agent's `turn_start` event at the time of the response).

With `group=card`, `key` is the card ID and the point also has the card's
`title`. Samples with no card make points with no `key` and no `title`:

```json
{"bucket":"2026-10-24T00:00:00Z","key":"TER-42","title":"Write the parser",
 "input_tokens":30,"output_tokens":12,"cache_read":0,"cache_write":0,"messages":2,"turns":1}
```

Card attribution. The collector gives each sample the card that the
sample's agent has in `in_progress`, assigned to itself, at the time the
collector stores the sample. When the agent has more than one such card,
the card with the newest `updated_at` wins. When it has none, the sample
has no card. The collector caches the card of each agent. Each `card`
event that names the agent (as the event's agent, `assignee`, or
`previous`) clears the agent's cache entry, so a claim, a move, or a
reassignment applies from the next sample. Samples stored before a change
keep their card.

### 16.2 Rules

- `GET /api/subscriptions`, `.../quota`: the operator, a `sysadmin`, or a
  `project_admin`. An agent may `GET /api/subscriptions/{name}` of its own
  subscription.
- Create, delete, probe, verify, detect: `sysadmin` or the operator.
- Login sessions, logout, and API keys: the operator only (PLAN §10,
  §16.1). Any agent gets `403 forbidden`.
- `GET /api/usage`: an agent sees its own project only, unless it is a
  `sysadmin`.
- Expired logins. A status check (verify, the start-up check) that says
  "logged out" sets `expired` when the subscription had a login
  (`logged_in`, `expiring`, `expired`), else `missing`. A probe answer of
  HTTP 401 also sets `expired`. On each change into `expired`, the daemon
  mails the operator once from `system` (subject `Login expired: <name>`)
  and emits `subscription.expired`. A later successful probe or status
  check sets `logged_in` again.

### 16.3 Endpoints

| Method and path | Body or query | Answer |
|---|---|---|
| `GET /api/subscriptions` | | Array of subscriptions. |
| `POST /api/subscriptions` | `{name, provider, plan?, harnesses?, probe?, alert_at?}` | `201` with the subscription (`source = api`). `409 conflict` when the name exists; `422 invalid` for an unknown probe. |
| `GET /api/subscriptions/{name}` | | One subscription. |
| `DELETE /api/subscriptions/{name}` | | `204`. `409 file_managed` for a file subscription. |
| `GET /api/subscriptions/{name}/quota` | `?since=` (RFC 3339, default 24 h ago), `?window=`, `?limit=` (default 1000) | Array of samples, oldest first: `{subscription, window, used_pct, resets_at, raw, at}`. `raw` is the provider's window object. |
| `POST /api/subscriptions/{name}/probe` | | `{subscription, probe_status, error?, windows}`. Here `subscription` is the **name** only, not the object. A failed probe is still `200`. |
| `POST /api/subscriptions/{name}/verify` | | `{subscription, status}`. Here `subscription` is the **full** subscription (§16.1), after the check. `status` is `{kind, harness, provider, pi_provider?, logged_in, plan?, detail?}`. The harness status command runs against the shared home and sets `login_status` (and `plan`, when the subscription has none). A stored API key counts as logged in: `status` is then `{logged_in: true, detail}`. |
| `POST /api/subscriptions/{name}/logout` | | The subscription (§16.1), with `login_status` `missing`. Runs `claude auth logout`, `codex logout`, or `copilot logout` against the shared home. pi has no logout command: `422 invalid`. |
| `POST /api/subscriptions/{name}/api-key` | `{provider, key, base_url?, models?}` | The subscription, without the key. See §16.5. |
| `GET /api/subscriptions/detect` | | Logins found in the harness homes that no subscription covers (same provider, and the harness is in `harnesses`): `[{kind, harness, provider, pi_provider?, logged_in, plan?, detail?, suggested}]`. `suggested` is a subscription definition. |
| `POST /api/subscriptions/login` | `{kind, provider?, name?}` | `201` with the login session (§16.4). |
| `GET /api/subscriptions/login/{id}` | | The login session. |
| `DELETE /api/subscriptions/login/{id}` | | `202`; the session becomes `cancelled`. `409 not_running` when the session ended (also when it is `done`). |
| `POST /api/subscriptions/login/{id}/input` | `{code}` | `204`. Types the code and Enter into the flow. `409 not_running` when the session ended. |
| `GET /api/subscriptions/login/{id}/pty` | WebSocket | The login terminal, with the protocol of §6.5. |
| `GET /api/usage` | `?subscription=&project=&agent=&model=&card=&since=&until=&bucket=hour\|day&group=subscription\|project\|agent\|model\|card` | Array of usage points, oldest first. `since` defaults to 24 h ago. `agent` is an agent key (§2.2). `card` is a card ID, in any case. |
| `GET /api/cards/{id}/usage` | `?since=&until=` (default: all time) | The usage of one card (below). The caller must be able to read the card's project. `404 not_found` for an unknown card. |
| `GET /api/usage/me` | `?agent=` (the operator only) | The caller's subscription and its share today (below). |
| `GET /api/usage/project` | `?project=` (default: the caller's), `?since=` (default: the start of the UTC day) | The per-agent split (below). |
| `POST /api/agents/{id}/status` | `{status, note?}` | The stored `self_report` event. §16.6. |

`GET /api/usage/me`:

```json
{"agent":{"id":"01M5...","name":"worker","project":"p"},
 "subscription":{"name":"anthropic-max","provider":"anthropic","plan":"Max 20x",
   "probe_status":"ok","last_success_at":"...","local_estimate_only":false,
   "windows":[{"window":"5h","used_pct":16.0,"resets_at":"...","at":"..."}]},
 "today":{"since":"2026-10-24T00:00:00Z","mine":{"input_tokens":4,"output_tokens":55,"cache_read":36712,
   "cache_write":36983,"messages":2,"turns":1},"subscription":{...},"share_pct":100.0},
 "holds":[]}
```

`share_pct` is the agent's input plus output tokens as a percent of all
agents on the subscription today, by local accounting.

`GET /api/usage/project`:

```json
{"project":"p","since":"2026-10-24T00:00:00Z","total":{...},
 "agents":[{"id":"01M5...","name":"worker","subscription":"anthropic-max","input_tokens":4,
            "output_tokens":55,"cache_read":36712,"cache_write":36983,"messages":2,"turns":1,
            "share_pct":100.0}],
 "budgets":[{"subscription":"anthropic-max","tokens_per_day":60,"alert_at":50,"used_tokens":59,"used_turns":1}]}
```

Agents with no usage are in the list with zero totals and no `share_pct`.

`GET /api/cards/{id}/usage`:

```json
{"card":"TER-42","title":"Write the parser","project":"p",
 "total":{"input_tokens":30,"output_tokens":12,"cache_read":0,"cache_write":0,"messages":2,"turns":1},
 "models":[{"model":"claude-opus-5","input_tokens":20,"output_tokens":7,"cache_read":0,"cache_write":0,
            "messages":1,"turns":1}, ...],
 "agents":[{"id":"01M5...","name":"p/worker","input_tokens":30,"output_tokens":12,"cache_read":0,
            "cache_write":0,"messages":2,"turns":1}]}
```

`models` and `agents` are biggest spender (input plus output tokens)
first. `since` and `until` are in the answer when the query gives them.

The CLI calls `GET /api/usage?agent=<me>` for `terrarium usage` and
`GET /api/usage?project=<p>` for `terrarium usage project`. Both work;
`/api/usage/me` and `/api/usage/project` are the richer answers for a later
CLI change.

### 16.4 Login sessions

A login session runs the harness login command in a PTY, as the daemon
user, with no sandbox, against the shared harness home:

| `kind` | Command | What the session finds |
|---|---|---|
| `claude` | `claude auth login` | `url` (the OAuth authorize URL). Paste the code with `.../input`. |
| `codex` | `codex login --device-auth` | `url` and `device_code`. |
| `copilot` | `copilot login --device-code` | `url` and `device_code`. |
| `pi` | `pi` (needs `provider`) | `url` after the operator types `/login` and picks the provider in the terminal. |

```json
{"id":"01M5...","kind":"codex","state":"waiting","url":"https://auth.openai.com/codex/device",
 "device_code":"ABCD-EF12G","started_at":"...","done":false,
 "instructions":"Open the URL, sign in, and enter the device code."}
```

`kind` is exactly `claude`, `codex`, `copilot`, or `pi`. `provider`,
`url`, `device_code`, `subscription`, `finished_at`, and `error` are absent
until they have a value.

`state`: `starting`, `waiting` (a URL or a code is on screen), `verifying`,
`done` (`subscription` names the definition made or updated), `failed`
(`error`), `cancelled`, or `expired` (after ten minutes, or after a daemon
restart). `done` is `true` for the last four.

When the command exits with code 0 (pi: when `pi auth check` says the
provider is ready), the daemon runs the status command, makes or updates the
subscription (name, provider, plan from the status output, the harness, the
matching probe), sets `login_status = logged_in`, and runs the probe once. A
file subscription keeps its definition; only its login state changes.
The session's runner files are deleted at the end. The pasted code is never
stored, logged, or sent back; the audit row `login.input` has its length
only. Audit actions: `login.start`, `login.attach`, `login.input`,
`login.cancel`, `login.end`, `subscription.register`.

At most four sessions are live at once (`409 conflict`).

### 16.5 API keys

`POST /api/subscriptions/{name}/api-key` stores the key in
`<state>/credentials/subscriptions/<name>.json`, mode `0600` (the
directories are `0700`), and sets `env_var`:

| `provider` | `env_var` |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `openai-compatible` (needs `base_url`) | `OPENAI_API_KEY`, and `OPENAI_BASE_URL` |
| `google`, `gemini` | `GEMINI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `xai` | `XAI_API_KEY` |
| `groq` | `GROQ_API_KEY` |
| `github` | `COPILOT_GITHUB_TOKEN` (a fine-grained PAT with Copilot Requests; the Copilot probe uses it) |

A new name makes an `api` subscription (harness `pi`, or `copilot` for
`github`). No endpoint returns the key. The audit row
`subscription.api_key` has the provider, the env var, `base_url`, and
`models`.

At each launch, `RunnerSupervisor` puts the stored key into the env of
every agent whose `subscription` has `env_var`
(`terrariumd::login_session::credential_env_for`). The key goes on top of
the adapter's env. The start log and `terrariumd sandbox-dry-run` show the
value of each var whose name has `KEY`, `TOKEN`, `SECRET`, or `PASSWORD` as
`<redacted>`.

### 16.6 The quota policy

- A window at or over `alert_at` percent: one mail to the operator (from
  `system`, event actor `system`) and one `quota.alert` event (`level:
  alert`) per window period (the reset time). Each quota mail ends with a
  dashboard link, `Open: <TERRARIUM_PUBLIC_URL>/subscriptions`; a budget
  mail links `<TERRARIUM_PUBLIC_URL>/p/<project>/spend`.
- A window at 100 percent: a `quota.alert` with `level: exhausted`, and a
  hold on every agent on the subscription until the latest reset time. The
  supervisor's `hold_turns` marks an idle agent `blocked` with the reason
  and the reset time (a working agent finishes its turn first). New turns
  wait: `POST .../send` still answers `202`, and push pointers and the
  recap turn stay pending. The `quota.blocked` event has `{cause, until,
  reason, enforced}`. `enforced: false` means the supervisor cannot hold
  turns (the default of the `Supervisor` trait), so the hold is advice
  only. A hold on an agent that does not run applies when it starts.
- The hold ends when a probe reads the window under 100 percent, or when its
  reset time passes (checked on each probe tick). The `quota.resumed` event
  follows, the agent goes back to `idle`, and the waiting turns go out in
  order, then the pending push pointer. Turns that wait when the agent
  stops are dropped.
- Daily project budgets come from the project file:

  ```toml
  [[budget]]
  subscription = "anthropic-max"   # optional: every subscription
  tokens_per_day = 5000000         # input + output tokens, UTC day
  turns_per_day = 200
  alert_at = 80                    # optional, percent
  ```

  The same alert and hold apply, with the cause `budget:project:<p>[/<sub>]`
  and the next UTC midnight as the reset.

`POST /api/agents/{id}/status` (`terrarium status set`) is the agent's own
word: `status` is `idle`, `working`, or `blocked`; `note` is up to 500
characters. It is stored as a `self_report` event. It does not change the
status that the runner reports. An agent may report for itself only.

### 16.7 Events

| `kind` | `data` |
|---|---|
| `usage.sample` | `{subscription, harness, model, input_tokens, output_tokens, cache_read, cache_write, messages}`: the new usage of one agent since its last `usage.sample`, at most one each 10 s per agent. |
| `usage.rejected_path` | `{harness, harness_session, path, reason}`: the usage collector refused the session file that a `session_started` hook named. The file must be a regular file (not a symlink) under the session area of the harness, and another agent must not read it. A Claude Code, Codex, or Copilot file must also carry the session ID in its name (`docs/security-review.md` F12). The collector does not read a refused file. |
| `quota.sample` | `{subscription, windows}` after each successful probe. |
| `quota.alert` | `{subscription, window, used_pct, resets_at, level, alert_at}`, or for a budget `{project, subscription, window, used, limit, used_pct, level}`. |
| `quota.blocked` | `{cause, until, reason, enforced}`. The `agent` is the held agent. |
| `quota.resumed` | `{cause, holds_left}`. |
| `subscription.offer` | `{login, suggested}`: a login found at daemon start that no subscription covers. |
| `subscription.expired` | `{subscription, from, reason}`: the login changed into `expired`. |
| `self_report` | `{status, note}`. |

## 17. Targets, host agents, and exec sessions

PLAN §19. The code is in `crates/terrariumd/src/hosts.rs` (the dial-in
endpoint and the host registry), `src/executor.rs` (the routing), and
`src/api/{hosts,targets}.rs`. The tests are in `tests/hosts.rs`. The wire
contract with the host agent is [host-protocol.md](host-protocol.md).

### 17.1 Host agents

A host agent dials in to `GET /hosts/connect` (a WebSocket, not under
`/api`). The daemon serves this path on its own listener,
`TERRARIUM_HOST_LISTEN` (default `0.0.0.0:8082` with TLS, `127.0.0.1:8082`
without), and on the Unix socket. The dashboard port (`TERRARIUM_LISTEN`)
does not serve it: that port is plaintext, and a host token sent to it would
travel in clear (`docs/security-review.md`, F2). `TERRARIUM_HOST_TLS_CERT` and
`TERRARIUM_HOST_TLS_KEY` turn on TLS on the listener
([host-protocol.md "TLS"](host-protocol.md#tls)). The upgrade must send
`Authorization: Bearer <host token>`. A bad, revoked, used, or expired token
gets `401 unauthorized` before the upgrade.

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/hosts` | | `200` hosts by name | any caller |
| `GET /api/hosts/{name}` | | `200` host | any caller |
| `POST /api/hosts` | `{"name", "enrol"?}` | `201` one flat host object (the shape below) with `token`, `token_kind`, and for an enrol token `token_expires_at` | the operator |
| `POST /api/hosts/{name}/rotate` | none, or `{"enrol"?}` | `200` the same shape as `POST /api/hosts` | the operator |
| `DELETE /api/hosts/{name}` | | `204` | the operator |

```json
{"name":"lab1","id":"01JB...","host_id":"01JB...","created_by":"operator",
 "status":"online","online":true,"sessions":1,
 "capabilities":{"docker":{"present":true,"version":"29.8.0"},"vmlab":{"present":false},"shell":"/bin/bash"},
 "summary":{"os":"linux","arch":"x86_64","docker":true,"vmlab":false},
 "os":"linux","arch":"x86_64","version":"0.1.0","max_sessions":32,
 "connected_at":"2026-10-24T06:36:54.149Z","last_seen":"2026-10-24T06:36:54.149Z",
 "created_at":"2026-10-24T06:30:00.000Z",
 "first_used_at":"2026-10-24T06:31:02.410Z","last_used_at":"2026-10-24T06:36:54.149Z",
 "last_used_from":"10.0.0.7:51544","rotated_at":null,"enrol_expires_at":null}
```

Rules:

- Only the operator registers and revokes host agents, because each
  registration makes a host agent token (PLAN §10: "manage credentials and
  host agent tokens" is operator-only). An agent, also a sysadmin agent,
  gets `403 forbidden`. Every caller can list and show hosts.
- The answer of `POST` holds the token one time. The daemon keeps only the
  SHA-256 of it. `token_kind` is `host` (a long-lived `thk_` token) or
  `enrol` (a one-time `the_` token).
- `"enrol": true` gives an enrol token that works one time, for 10 minutes
  (`token_expires_at`). The host agent dials with it, and the daemon sends
  the new long-lived token in `welcome.token`. The host agent stores it in
  its `token_file`. So the long-lived token never passes through the
  operator. The swap writes the audit entry `host.enrol`, and
  `host.connected` has `"enrolled": true`. `enrol_expires_at` is set while
  an enrol token is open.
- `rotate` makes a new token (with `"enrol": true`, a new enrol token) and
  writes the audit entry `host.rotate`. The old token stops working for new
  connections at once. A live link stays up until it ends; the host agent
  must then dial with the new token. `rotated_at` records the time.
- `first_used_at` is the first connect with the current token (a rotate
  clears it). `last_used_at` and `last_used_from` are the last connect and
  its TCP peer as the daemon sees it (a reverse proxy shows as itself).
  `host.connected` carries the peer as `from`.
- `status`, `online`, and `sessions` come from the live link.
  `capabilities` comes from the last `hello`, `{}` before the first one.
- `DELETE` revokes the token. A live link gets `goodbye token_revoked` and
  closes. Its sessions end with the error `lost`.
- A second link with the same token replaces the first. The first gets
  `goodbye replaced`.
- `hello.host` must be the name of the token's host. Else the daemon sends
  `goodbye protocol`.
- Each link writes the events `host.connected` and `host.disconnected`
  (§9).

### 17.2 Targets

A target comes from `targets/<name>.toml` (`source: "file"`), from
`POST /api/targets` (`source: "api"`), or from a fleet file
(`source: "fleet:<name>"`, §18.1). Target names are one namespace for
all projects.

```json
{"id":"box","name":"box","kind":"docker","project":"terra","host":"lab1","image":"alpine:3",
 "ephemeral":true,"keep":false,"snapshot_before_task":false,
 "scope":"project","source":"api","state":"running","state_at":"2026-10-24T06:40:00.000Z",
 "snapshots":[],"host_online":true,"usable":true}
```

| Field | Meaning |
|---|---|
| `scope` | `global` (no `project`) or `project`. |
| `state` | `running`, `stopped`, `absent`, or `unknown`: what the last lifecycle verb saw. |
| `snapshots` | The snapshot names that `POST .../snapshot` took. |
| `host_online` | The link of the target's host agent. `null` for `ssh`. |
| `usable` | The caller may use the target (§17.4). |

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/targets?usable=&project=` | | `200` targets by name | read |
| `POST /api/targets` | target, as in a target file, with `name` | `201` target | project_admin of `project`; sysadmin for a global target |
| `GET /api/targets/{name}` | | `200` target | read |
| `PATCH /api/targets/{name}` | JSON merge patch; `null` removes a field | `200` target | as `POST` |
| `DELETE /api/targets/{name}` | | `204` | as `POST` |
| `GET /api/projects/{name}/targets` | | `200` the project's and the global targets | read |

An agent reads the global targets and the targets of its own project. A
sysadmin agent and the operator read all. `?usable=true` keeps only the
targets the caller may use. `terrarium targets ls` sends it in agent mode.

The fields that each kind needs:

| `kind` | Needs | Optional |
|---|---|---|
| `host` | `host` | |
| `ssh` | `address` | `port`, `credential` (an `ssh_key` in the credential store) |
| `docker` | `host`, and `image` or `container` | `ephemeral`, `keep` |
| `vmlab` | `host`, `lab` (an absolute path) | `machine`, `ephemeral`, `keep`, `snapshot_before_task` |

An `ephemeral` target must be `docker` (with `image`) or `vmlab`. Values
that go onto a command line (`host`, `address`, `image`, `container`,
`machine`, `credential`) must not be empty, must not hold spaces, and must
not start with `-`. A bad value is `422 invalid`.

File sync, at start and on `POST /api/reload`: a file target is made or
updated and becomes `file`. An API target with the same name becomes
`file`. When the file is gone, the target is deleted. The API refuses to
change a file target (`409 file_managed`).

### 17.3 Lifecycle verbs

`POST /api/targets/{name}/up|down|snapshot|restore|status`. The body is
empty, or `{"name"?, "discard_guest_changes"?, "destroy"?}`. The caller
may use the target (§17.4), or may define it (§17.2). The answer is the
executor report, not the target: `{"target", "verb", "state", "snapshot"?,
"result"?}`. `target` is the name, `state` is the new state, `snapshot` is
there for `snapshot` and `restore`, and `result` is the result of the
host agent verb when it gives one. The daemon stores the state and writes
a `target.<verb>` audit row with the same report.

| Kind | `up` | `down` | `snapshot` / `restore` | `status` |
|---|---|---|---|---|
| `host`, `ssh` | no-op | no-op | `422 invalid` | `running` when the host agent is online, else `unknown` |
| `docker` | `docker.inspect`; when absent, `docker.run -d --name <c> --label terrarium.target=<name> <image> sleep infinity` | `docker.stop`; an ephemeral target without `keep` (or `destroy: true`): `docker.rm -f -v` | `422 invalid` | `docker.inspect` |
| `vmlab` | `vmlab.up [machine]` | `vmlab.down [machine]`; ephemeral without `keep` (or `destroy: true`): `vmlab.destroy` | `vmlab.snapshot_take` / `vmlab.snapshot_restore` with `--vm machine` | `vmlab.status`, the row of `machine` |

The container `<c>` is `container`, else `terrarium-<name>`. The snapshot
name is `name`, else `terrarium`. A verb on an offline host is `409
conflict`. A verb that fails is `422 invalid` with `details: {verb, code,
end, stderr, stdout}`.

### 17.4 Exec sessions

An exec session runs one command, one shell, or one file copy. It uses
plain requests only, so it works through the agent tunnel too
([agent-rpc.md](agent-rpc.md), "Streams over the tunnel").

| Endpoint | Body or query | Answer |
|---|---|---|
| `POST /api/targets/{name}/exec` | `{"cmd": [..], "cwd"?, "env"?, "pty"?, "shell"?, "timeout_secs"?}` | `201` `{"session_id", "target", "kind"}` |
| `POST /api/targets/{name}/cp` | `{"direction": "push"\|"pull", "path", "mode"?}` | `201` `{"session_id", "target", "kind"}` |
| `POST /api/targets/{name}/sessions/{id}/input` | `{"data"?, "resize"?, "signal"?, "eof"?, "finish"?}` | `200` `{"session_id", "accepted"}` |
| `GET /api/targets/{name}/sessions/{id}/output` | `?since=<offset>&wait=<s>&max=<bytes>` | `200` output, below |
| `GET /api/targets/{name}/sessions/{id}` | | `200` `{"session_id", "kind", "acknowledged", "buffered", "done", "end"}` |
| `DELETE /api/targets/{name}/sessions/{id}` | | `204`; the run is killed |
| `GET /api/targets/{name}/sessions/{id}/ws` | WebSocket | the session as a terminal, with the frames of §6.5 |

The exec body:

- `cmd` is the program and its arguments. The target runs it directly,
  not through a shell. Send `["sh", "-c", "..."]` for a shell line.
- `cwd` is an absolute path. `env` names must match `[A-Za-z_][A-Za-z0-9_]*`.
  Env values go into the process environment, never onto a command line.
- `pty` is `true` (24 rows, 80 columns) or `{"rows", "cols"}`.
- `shell: true` with no `cmd` starts a login shell in a PTY: the host
  agent's `capabilities.shell` with `-l`, `/bin/sh -l` in a container,
  `vmlab shell` on a vmlab target, `exec "${SHELL:-/bin/sh}" -l` over ssh.
- A vmlab target takes no `cwd` and no `env`, and a PTY only for `shell`.

The input body: `data` is base64, at most 1 MiB of bytes (`413
too_large` above). In a push, `data` is file bytes. `resize` is
`{"rows", "cols"}`. `signal` is a name such as `INT`. `eof` closes stdin.
`finish` is `{"bytes", "sha256"}` and ends a push. An input to a session
that has ended is `409 conflict`.

The output answer:

```json
{"session_id":"01JC...","since":0,"next":10,
 "chunks":[{"stream":"stdout","data":"b3V0Cg=="},{"stream":"stderr","data":"ZXJyCg=="}],
 "done":true,"end":{"type":"exit","code":3,"reason":"exited"}}
```

- `since` is an offset in the output of the session: all streams
  together, in order. A read acknowledges the bytes before its `since`.
  The daemon drops them. A `since` before the acknowledged bytes is `409
  conflict`. Read with the same `since` again to get the same bytes again.
- `wait` holds the request up to that many seconds (at most 60) until
  output or the end comes. `max` limits the bytes (at most 1 MiB, the
  default).
- `stream` is `stdout`, `stderr`, or `file` (a pull).
- `done` is true when the session ended and `chunks` holds its last byte.
  Then `end` is one of:
  - `{"type": "exit", "code"?, "signal"?, "reason"}`, `reason` as in
    host-protocol.md (`exited`, `signaled`, `timeout`, ...);
  - `{"type": "cp", "bytes", "sha256"}` for a copy;
  - `{"type": "error", "code", "message"}`, for example `spawn_failed`,
    `path_denied`, `checksum_mismatch`, `lost`, `killed`.
- Each session buffers at most 8 MiB of output that is not acknowledged.
  At the limit, the daemon stops taking the host's output. The host stops
  reading the process, and the process waits. Nothing is lost.

Copies:

- A push goes to the target's `path`. The daemon and the host agent check
  the byte count and the SHA-256 of `finish`. On a mismatch the session
  ends with `checksum_mismatch`, and the file at `path` does not change.
  `mode` is the file mode (`420` for `0644`).
- A pull sends the file as `file` chunks. The end holds the count and the
  SHA-256. The daemon checks them against the bytes from the host.
- `host` targets use `cp_push` and `cp_pull`. `docker` and `ssh` targets
  run `cat` with stdin or stdout, into a partial file that a second
  command checks and renames. `vmlab` targets copy through a staging file
  under `<lab>/.terrarium-stage/` and `vmlab cp`.
- A push does not survive a reconnect of the host agent: it ends with
  `interrupted`. Send it again.

Rules:

- The operator may use every target. An agent may use a target that its
  `policy.targets` lists, when the target is global or in the agent's
  project. Else the answer is `403 forbidden`, and the message names the
  rule. The role does not change this rule.
- An agent holds at most 8 live sessions (`429 rate_limited`). A host
  agent holds at most its `max_sessions` (`429 rate_limited`).
- Only the agent that opened a session can drive it. To other agents it
  is `404 not_found`. The operator can drive every session.
- Each `exec` writes a `target.exec` audit row with the command (cut to
  200 characters), `cwd`, `pty`, and the env names (not the values). Each
  `cp` writes `target.cp`.
- A target whose host agent is offline is `409 conflict`.
- A finished session is forgotten after 5 minutes without a read. A
  session that nobody reads for 30 minutes is killed.
- When the host link drops, the sessions wait for the reconnect, and the
  host replays the output that the daemon does not have. A session that
  the host lost, or that has no link for 10 minutes, ends with `lost`.

`ssh` targets need no host agent. The daemon runs the system `ssh` with
`BatchMode=yes`, `StrictHostKeyChecking=accept-new`, and its own
`<state>/ssh/known_hosts`. The key is the target's `credential` (an
`ssh_key`, written to a 0600 run dir for the run), else the default keys
of the daemon's user (a mounted `~/.ssh`). The env goes to the remote
shell as one base64 line on stdin, so the remote side needs `base64`.
`TERRARIUM_SSH` sets the `ssh` binary. A PTY on ssh cannot resize.

### 17.5 Checkout on a target

`POST /api/repos/{id}/checkout {"target", "branch"?, "path"?, "depth"?}`
(PLAN §9). The caller must be able to use the target (§17.4), and the
target is global or in the repo's project. The checkout runs as one exec
of `sh -c <script>` on the target. The URL, the path, the branch, and the
depth are arguments of the script. The credential of the repo goes in the
env of that run only: `TERRARIUM_GIT_USERNAME` and
`TERRARIUM_GIT_PASSWORD` for a token, which a `credential.helper` of `-c`
reads, or `TERRARIUM_GIT_SSH_KEY` for an SSH key, which the script writes
to a 0600 temp file for `GIT_SSH_COMMAND` and removes at exit. The secret
is never on a command line. The target needs `git` and `sh`. A vmlab
target is `422 invalid`.

`path` is an absolute path on the target, or a path under
`$HOME/work/`. The default is the repo's `path`. A clone that is there
is fetched from the stored URL and fast-forwarded when the work tree is
clean.

```json
{"repo":"app","repo_id":"01JC...","project":"terra","target":"box","path":"app",
 "remote_path":"/home/build/work/app","action":"cloned","head":"3f2c1e0b9a...","branch":"main"}
```

`action` is `cloned`, `updated`, `up_to_date`, or `not_updated` (with
`note`). A failure is `422 invalid` with `details: {command, status,
stderr}`. Each checkout writes a `repo.checkout` event and audit row with
`target`.

### 17.6 Ephemeral targets

`crate::executor::ensure_up(state, target)` brings an ephemeral target up
(`up`). `crate::executor::maybe_down(state, target)` removes it, unless
`keep = true`. Both do nothing for a permanent target.
`executor::on_agent_start` and `executor::on_agent_stop` call them for each
name in an agent's `policy.targets`. `RunnerSupervisor::start` calls
`on_agent_start` before the harness launches. A target that does not come
up does not stop the start: the agent gets a `notification` event with the
error. The supervisor calls `on_agent_stop` on a stop, on an exit that does
not restart (also one that the daemon finds after its start), and when a
start or a restart fails. A restart of the harness and a daemon shutdown
leave the targets as they are.

## 18. Fleet

A fleet file declares projects, their repos, agents, and targets, global
targets, and subscriptions in one TOML file. [fleet.md](fleet.md) has the
file format (§1), the ownership rules (§2), the CLI (§3), the config dir
(§4), and the policy (§5). This section is the HTTP contract.

All routes take the usual auth (§1). The bodies are JSON.

### 18.1 The `source` of a fleet object

A project, agent, target, or subscription that a fleet owns shows
`"source": "fleet:<name>"` in `GET /api/projects`, `/api/agents`,
`/api/targets`, `/api/subscriptions`, and their single-object routes. The
values of `source` are:

| `source` | The object |
|---|---|
| `file` | Declared in a definition file. The API refuses to change it (`409 file_managed`). |
| `api` | Made through the API, or released by a fleet or a definition file. |
| `fleet:<name>` | Owned by fleet `<name>`. The API may change it; the next `plan` shows the change as drift, and the next `apply` writes the file values again. |

An apply writes `fleet:<name>` on each object that it makes or adopts. A
prune that releases a project or a subscription sets it back to `api`. A
definition file that declares the same object wins: the row becomes
`file`, and the fleet gets a `conflict`. `POST /api/reload` deletes and
releases `file` objects only, never `fleet:<name>` objects.

### 18.2 Shapes

An issue of a fleet file:

```json
{"level": "warning", "at": "projects.terrarium.agents.lead", "message": "..."}
```

`level` is `error` or `warning`. `at` is the place in the file.

A plan:

```json
{
  "fleet": "terrarium",
  "valid": true,
  "errors": [],
  "warnings": [],
  "summary": {"create": 0, "update": 1, "unchanged": 4, "delete": 1, "release": 0, "conflict": 0, "drift": 1},
  "entries": [
    {"kind": "agent", "key": "terrarium/lead", "project": "terrarium", "action": "update",
     "drift": ["prompt"],
     "changes": [{"field": "prompt", "from": "default", "to": "project-lead"}]},
    {"kind": "agent", "key": "terrarium/old", "project": "terrarium", "action": "delete", "live": true}
  ]
}
```

| Field of an entry | Meaning |
|---|---|
| `kind` | `subscription`, `project`, `target`, or `agent`. |
| `key` | The name; `project/name` for an agent. |
| `project` | The project of the object, when it has one. |
| `action` | `create`, `update`, `unchanged`, `delete`, `release`, or `conflict`. |
| `drift` | The fields that changed outside the fleet since the last apply, or `["(deleted)"]`. Absent when empty. |
| `adopt` | `true` when the apply takes over an API object that no fleet owns. |
| `changes` | `{field, from, to}` for each field that the apply changes. |
| `reason` | Why the entry is a conflict, or a note. |
| `live` | `true` for an agent with a live session. |
| `result` | After an apply: `done` or `skipped`. |

A fleet row:

```json
{"name": "terrarium", "description": "...", "origin": "api", "applied_at": "2026-10-24T06:36:54.149Z",
 "applied_by": "operator", "objects": 5, "projects": ["terrarium"]}
```

`origin` is `api` or `file:<path in the config dir>`. `objects` counts the
objects that the fleet owns. `projects` lists their projects.

### 18.3 Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `POST /api/fleet/validate` | `{"content": "<toml>", "path"?}` | `200`, a validation (below). Any caller. |
| `POST /api/fleet/plan` | `{"content", "path"?, "prune"?: false}` | `200`, a plan. |
| `POST /api/fleet/apply` | `{"content", "path"?, "prune"?: false, "confirm"?: false, "start"?: false}` | `200`, the plan with `result` on each entry, and more (below). |
| `GET /api/fleet` | | `200`, the fleet rows that the caller may see, by name. |
| `GET /api/fleet/{name}` | | `200`, the fleet row with `content` and `plan`. |
| `GET /api/fleet/export?project=P[&name=N]` | | `200`, `{"fleet": "N", "project": "P", "content": "<toml>"}`. |

`path` is a file name. Its stem names a fleet that has no `name`.

`POST /api/fleet/validate` answers:

```json
{
  "fleet": "terrarium",
  "valid": true,
  "errors": [],
  "warnings": [{"level": "warning", "at": "prompts", "message": "..."}],
  "objects": [{"kind": "agent", "key": "terrarium/lead", "project": "terrarium"}],
  "references": {"subscriptions": {}, "targets": {}, "prompts": {"project-lead": ["terrarium/lead"]}}
}
```

A file that does not parse, or that has errors, is a `200` with
`valid: false`. For `plan`, it is a `200` with `valid: false` and no
entries.

`POST /api/fleet/apply` adds these fields to the plan:

- `prune`: the flag.
- `checkout`: the clone results for the new agents, as `POST /api/agents`.
  Absent when empty.
- `started`: the agents that `start` started. Absent when empty.
- `failures`: the starts and clones that failed. The apply itself stays.
  Absent when empty.

An apply is one transaction. When one object fails, nothing changes.
Without `prune`, the `delete` and `release` entries get `result: skipped`.

`GET /api/fleet/{name}` gives the last applied file as `content`, and the
plan of that content against the store now as `plan`, so drift shows.

`GET /api/fleet/export` writes a fleet file from a project as it is now.
`name` defaults to the project name. An apply of the export adopts the
project with no changes.

### 18.4 Rules and errors

The policy of [fleet.md](fleet.md) §5 applies to each entry of a plan.
`plan` needs the same rights as `apply`. A plain `member` gets `403` for
`plan` and `apply`.

A fleet name that is in the store already has more rules
(`docs/security-review.md` F10):

- The caller needs the rights over every object that the stored fleet
  owns, with or without `prune`. The objects that the new file drops
  count too, because a later prune of the stored content deletes them.
- A fleet that declares no objects replaces a stored fleet only for a
  sysadmin or the operator.
- An API `apply` over a fleet whose `origin` is `file:<path>` needs
  `confirm: true`. Else the answer is `409 confirm_required`, with
  `details` `{fleet, origin, applied_by}`. The next reload applies the
  file again. `plan` does not need `confirm`.

| Status | Code | When |
|---|---|---|
| 400 | `bad_request` | The body is not JSON, or has an unknown field. |
| 403 | `forbidden` | `plan` or `apply`: the caller is a plain member, may not change an object of the plan or of the stored fleet, or sends an empty fleet over a stored fleet without sysadmin. `export`: the caller may not read the project. |
| 404 | `not_found` | `GET /api/fleet/{name}`: no such fleet, or the caller may not see it. `export`: no such project. |
| 409 | `conflict` | `apply`: the plan has a conflict. `details` is the plan. |
| 409 | `confirm_required` | `apply` with `prune` deletes an agent with a live session, and `confirm` is not `true`. `details.agents` names the agents. With `confirm`, the daemon stops them, then deletes them. Also: `apply` over a stored fleet from a `file:`, and `confirm` is not `true`. |
| 422 | `invalid` | `apply`: the file has errors (`details.errors`, `details.warnings`), or the store refuses an object. `export`: a bad or reserved fleet name. |

The operator and sysadmins see every fleet in `GET /api/fleet`. An agent
sees the fleets that own objects in its own project.

The `errors` list of `POST /api/reload` also has the errors of the fleet
files in the config dir, as `<path>: <message>`.

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
| `TERRARIUM_MAIL_LOOP_LIMIT` | none | `6` | The mail loop guard (§6, PLAN §12): the agent-to-agent messages a thread can have since the last operator or project-inbox message before new agent mail on it types no pointer. The daemon reads it once. |
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
- A time query value (`?since=`, `?until=`) takes these forms:
  - an RFC 3339 time with any offset, for example
    `2026-10-25T19:53:00+10:00` or `2026-10-25T09:53:00Z`. Encode `+` as
    `%2B` in a URL. The daemon changes the time to UTC before it compares;
  - a duration back from now: a number and the unit `s`, `m`, `h`, or `d`,
    for example `90s`, `30m`, `2h`, or `1d`.

  A bare clock time (`19:53`), a date without a time, and a number without
  a unit are `400 bad_request`. The message gives the accepted forms. The
  event endpoints (§8, §9) also take a `seq`, an event ID, and (on the
  streams) `latest`.
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
| 409 | `lease_held` | Another agent holds a lease on the target that the new lease clashes with (§17.7). `details: {"target", "holder", "holder_id", "lease_id", "exclusive", "expires_at"}`. |
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
| Grant, extend, or revoke a target lease (§17.7) | no | a target of own project, to an agent of own project | any |
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

### `GET /api/harnesses`

Every caller may read it. For each registered harness: the pin and the
package from `harnesses.toml`, the range that the adapter accepts, the
version that `<binary> --version` reports, and whether that version is in
the range. `policy` is `TERRARIUM_HARNESS_VERSION_POLICY` (§6.2). The
daemon caches each version for 10 minutes; `?refresh=true` runs the
binaries again.

```json
{"policy":"warn",
 "harnesses":[{"harness":"claude","package":"@anthropic-ai/claude-code","pinned":"2.1.310",
   "tested":">=2.1.310, <2.2.0","installed":"2.1.310","in_range":true,
   "checked_at":"2026-10-25T10:00:00Z"},
  {"harness":"pi","package":"@earendil-works/pi-coding-agent","pinned":"0.85.1",
   "tested":">=0.85.1, <0.86.0","installed":null,"in_range":null,
   "error":"cannot run pi: No such file or directory","checked_at":"2026-10-25T10:00:00Z"}]}
```

`in_range` is `null` when the version is not known, and `error` says why.
`terrarium admin harness ls` prints this table.

`?latest=true` adds three fields to each row, from the npm registry
(`https://registry.npmjs.org/<package>`: `dist-tags.latest` and its entry
in `time`). The daemon caches each answer for 1 hour, and a failed lookup
for 5 minutes. `?latest=true&refresh=true` asks npm again.

```json
{"harness":"codex","package":"@openai/codex","pinned":"0.164.0",
 "tested":">=0.164.0, <0.165.0","installed":"0.164.0","in_range":true,
 "checked_at":"2026-10-26T09:00:00Z",
 "latest":{"version":"0.165.0","published_at":"2026-10-25T09:30:00Z"},
 "update":"major"}
```

| Field | Meaning |
|---|---|
| `latest` | `{version, published_at}`. `published_at` is `null` when the registry gives no time. `latest` is `null` when npm did not answer. |
| `update` | The bump from `installed` (when it is known, else `pinned`) to `latest`: `none`, `minor` (a patch or a minor), or `major`. `null` when `latest` is `null`. |
| `latest_error` | Only when `latest` is `null`: why. For example `cannot reach npm: ...` or `npm answered 503 for ...`. |

`update` follows `scripts/harness.py` (`just harness::check`). A release
that is not newer than the installed version is `none`. A newer release is `major` when
the first non-zero part of the version changes: `2.1.x` to `3.0.0`, or
`0.164.x` to `0.165.0` (for `0.x`, the second number is the major). Else
it is `minor`. A failure to reach npm never fails the request: the answer
is still `200`.

The Subscriptions page shows these fields in its "Harness versions" table,
with an "Update" button for each row that has a newer release. The button
calls `POST /api/harnesses/{name}/update`.

### `POST /api/harnesses/{name}/update` and `POST /api/harnesses/update`

The harnesses are not in the image. They are in the tools dir
(`TERRARIUM_TOOLS_DIR`, `/opt/terrarium/tools` in the container, `./tools`
on the Docker host), which is the npm prefix. An update runs
`npm install -g <package>@<version>` into that dir, in the running
container. It needs no image build, no restart, and no host agent
([getting-started.md §7](getting-started.md#7-updating-a-harness)). Needs
sysadmin or the operator.

| Endpoint | Body | Answer |
|---|---|---|
| `POST /api/harnesses/{name}/update` | none, or `{"version"?, "force"?}` | `200` one result |
| `POST /api/harnesses/update` | none, or `{"version"?: "latest", "force"?}` | `200` `{"results": [...]}`: every harness and extension, one after another |

`{name}` is a harness (`claude`, `codex`, `copilot`, `pi`) or the extension
`copilot-prompt`. `version` is `latest` (the default) or a version such as
`0.165.0`. The update of every harness takes `latest` only.

```json
{"harness":"codex","package":"@openai/codex","from":"0.164.0","to":"0.165.0",
 "output":"\nchanged 2 packages in 2s\n","tested":">=0.164.0, <0.165.0","in_range":false}
```

| Field | Meaning |
|---|---|
| `from` | The version before; `null` when the package was not installed. |
| `to` | The version after: `<binary> --version` for a harness, `package.json` for the extension. |
| `output` | npm's output: stdout, then stderr. |
| `tested`, `in_range` | The adapter's range, and whether `to` is in it (`null` for the extension). Outside the range, an agent start follows `TERRARIUM_HARNESS_VERSION_POLICY` (§6.2). |
| `error` | `POST /api/harnesses/update` only, on a failed item: why. |

Rules:

- While an agent on the harness is live (`starting`, `idle`, `working`,
  `blocked`, `restarting`), the answer is `409 conflict` with
  `details.running` (`["project/name", ...]`). A running harness keeps the
  files it loaded, but a swap in the middle of a turn is risky. Stop the
  agents, or send `"force": true`. For `copilot-prompt`, the Copilot agents
  count. The next start of an agent runs the new version.
- One update runs at a time. A second one gets `409 conflict`.
- A name that is not a pin, or a version that is not `latest` or semver,
  is `422 invalid`. Nothing reaches npm.
- When npm fails (for example a version that does not exist), the answer
  is `422 invalid` with `details`: `{output, package, version, from}`.
  Nothing changes. npm has 10 minutes; then the daemon kills it.
- `POST /api/harnesses/update` goes on after a failed item. When any item
  failed, the answer is `422 invalid` with `details.results`.
- A Copilot update also reinstalls the Copilot prompt extension into the
  Copilot home.
- Each update writes the audit entry `harness.update` (`detail`: `package`,
  `version`, `from`, `to`, `force`, `ok`, `error`). A done update also
  writes the event `harness` (§9) and runs `<binary> --version` again, so
  `GET /api/harnesses` shows the new version at once.
- A daemon with no tools dir (`TERRARIUM_TOOLS_DIR` empty) answers
  `501 not_available`.
- The tools dir keeps the manifest `terrarium-manifest.json`:
  `{"<name>": {"package", "version", "installed_at"}}`.

`terrarium admin harness update <name> [--version <v>] [--force]` and
`terrarium admin harness update --all [--force]` call these endpoints.

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
| `since` | Rows at or after this time (§2.1: RFC 3339 with any offset, or a duration such as `1h`). |
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
`mail.read` (the first read of a message; `detail` has `thread`, `from`,
`to`, and `via`: `read` or `reply`), `board.columns`, `card.reassign`, `card.reopen`, `card.archive` (by an admin
on a card that is not its own), `definitions.sync`, `memory.edit` and
`memory.delete` (§10), `wiki.delete`, `wiki.move`, `wiki.undelete` (§11),
`artifact.delete`, `artifact.prune_warning`, `artifact.prune` (§12),
`version.approve`, `version.reject`, `version.release` (§13),
`host.create`, `host.revoke`, `target.create`, `target.update`,
`target.delete`, `targets.sync`, `target.exec`, `target.cp`, `target.up`,
`target.down`, `target.snapshot`, `target.restore`, `target.status`,
`target.grant`, `target.revoke`, `target.lease_extend`, and
`target.lease_expired` (actor `system`) (§17).

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
`branch` is an alias of `default_branch`. A body with the two is `400
bad_request`. A body with an unknown field is `400 bad_request`, and the
message names the fields.

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

**Local repos.** A repo whose URL is a path under `TERRARIUM_REPOS_DIR`
(default `/repos`), for example `file:///repos/app.git`, is a local repo.
At each start, the sandbox binds each local repo of the agent's project
read-write at the same path. So `git push origin` from the workspace works
with no credential. The rules:

- Only a URL whose path is under `TERRARIUM_REPOS_DIR` makes a bind. A file
  URL outside the dir, a path with `.` or `..`, a `file://<host>/` URL, and
  a path whose real path (after symlinks) leaves the dir make no bind.
- The agents of other projects do not get the bind, unless their project
  also has the repo.
- Only a `sysadmin` or the operator attaches a repo with a local path URL
  (`file://` or `/`). A `project_admin` gets `403 forbidden`, because the
  repo could belong to another project.
- An agent can change every file of the bare repo, its config and hooks
  too. Treat `./repos` on the host as data that agents write.

The daemon clone on agent create and on `checkout` works for a local repo
as for any other local path.

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
| `DELETE /api/agents/{id}` | `?confirm=true` when live; `?keep_workspace=true` | `204` | project_admin |

`DELETE` stops a live agent, waits for it to end (up to 10 s), and moves
its dir `<state>/agents/<ulid>/` to `<state>/trash/<ulid>-<ts>/`. It also
moves the workspace `<work>/<project>/<agent>` (§17.8) to
`<state>/trash/<ulid>-<ts>/workspace`, unless `?keep_workspace=true`. A
workspace outside the work root stays. The operator can still read the
transcripts, the event log, and the files there. The daily prune timer
removes a trash entry older than `TERRARIUM_TRASH_DAYS` days (default `7`),
with its workspace. A reload that removes an agent file and a fleet prune
move the agent dir only.

The definition is the TOML agent definition as JSON:

```json
{"name":"worker","project":"terrarium","harness":"pi","model":"anthropic/claude-opus-5-5",
 "roles":[],"policy":{"targets":[],"mail_to":["*"],"cross_project":["ops/triage"]},"restart":"on-failure"}
```

`new_session_on_prompt_change` (default `true`): when the rendered prompt
changed since the harness session began, and the harness keeps the prompt
of a session (Codex), `start` and a restart begin a new harness session
instead of a resume. The status reason is `new session: the prompt
changed`. The old session stays in the history. `POST /api/agents` without
the field sets `true`, the same as an agent file.

A harness that takes a changed prompt on a resume (Claude Code with
`--system-prompt-snapshot off`, pi) resumes with the new prompt. The daemon
then writes the new hash to `private/session-prompt.sha256` and records a
`system` event with the message `prompt updated on resume`.

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

A target lease (§17.7) gives an agent a target at run time, without a
`policy` write. It does not change `policy`. `GET /api/agents/{id}/leases`
lists the leases that the agent holds.

### 6.2 Lifecycle and turns

| Endpoint | Body | Answer |
|---|---|---|
| `POST /api/agents/{id}/start` | none | `202` agent |
| `POST /api/agents/{id}/stop` | none | `202` agent. Also on an agent in `error`: it becomes `stopped`. |
| `POST /api/agents/{id}/restart` | none | `202` agent. Also on an agent in `error`: a new launch. |
| `POST /api/agents/{id}/send` | `{"text": "Run the tests"}` | `202 {"accepted": true}` |
| `POST /api/agents/{id}/answer` | `{"request_id", "answer"}` or `{"text"}` | `202 {"accepted": true}` |

All need project_admin. The work happens in the background. A `send` by
the operator writes a row to the delivery log first, so the transcript tags
the turn `origin: "operator"` when the harness writes it (§6.6). The answer
shows the agent when the daemon accepted the call; watch the events (§8)
for the status changes.

- `start` opens a new session and resumes the last harness session.
  A running agent gets `409 already_running`.
- `stop` types the adapter's exit keys into an idle harness that has them
  (Claude, Codex, Copilot: `/exit`; pi: `/quit`), and kills it when it has
  not exited after 5 s. A busy harness, or one with no exit keys, is killed
  at once. For Codex, the CR goes 0.3 s after the text
  (`Adapter::exit_key_pause`).
- `stop` on an agent in `error` clears it: the status becomes `stopped`
  with the reason "cleared by operator", and a status event goes out. A
  stray runner process that still holds the agent is killed.
- `stop` on an agent that is not running and not in `error` gets
  `409 not_running`. When the store says live and the supervisor says not
  running, `stop` sets `stopped` (reason "reconciled").
- `restart` ends the current session with outcome `restarted` and starts a
  new one. On a stopped agent, or an agent in `error`, it is the same as
  `start`: a new launch.
- `send` types one turn through the runner: it waits until the agent is
  `idle` and the prompt is ready. A stopped agent, or one between an exit
  and its restart, gets `409 not_running`.
- `start` of an agent whose harness adapter is not registered, or whose
  prompt template or harness home is missing, gets `422 invalid` with the
  reason.
- At each launch (`start`, `restart`, a restart by the policy, a relaunch
  after a daemon start), the daemon compares the installed harness version
  with the adapter's tested range (§4, `GET /api/harnesses`). Outside it,
  with `TERRARIUM_HARNESS_VERSION_POLICY=warn` (the default), the agent
  starts, and a `warning` event names the version and the range. With
  `refuse`, the start gets `422 invalid` with the same text. A version that
  cannot be read never refuses a start.
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
its first pointer. A card line (and a lease line, §17.7) for an agent that
does not run is kept for the next start, also across a daemon restart: the
daemon stores it in the `pending_pointers` table. A newer line for the same
card and change (or the same target) replaces the older line. Before the
pointer goes out, the daemon checks each card line again and drops a line
that no longer applies (for example, the card went to another agent).

Lead nudges (PLAN §13): three board changes add a line to the pointer of
each running `project_admin` agent of the card's project. The agent that
made the change gets no nudge.

| Change | Line |
|---|---|
| A card with no assignee moves into `ready`, also a new card made in `ready` | `New card DEM-3 in ready: <title>. Run `terrarium card ls --unassigned --column ready`.` |
| A card moves to `review` | `DEM-3 moved to review by <who>. Run `terrarium card show DEM-3`.` |
| A card moves to `blocked` | `DEM-3 moved to blocked by <who>: <comment>. Run `terrarium card show DEM-3`.` |

The title and the comment are cut to their first line and 100 characters.
In each project, the daemon sends not more than one batch of nudges in 10
seconds. The changes in that time go out together, as one pointer for each
lead. A second change of the same kind on the same card replaces the line.

The loop guard (PLAN §12): a thread pauses when it has more than
`TERRARIUM_MAIL_LOOP_LIMIT` (default 6) agent-to-agent messages since its
last message from the operator or from or to a project inbox. New agent mail
on a paused thread is `delivered` at once and types no pointer. The message
row records the hold: `held: true` and a `held_reason` (§7). Its `mail`
event has `held: true`, `held_reason`, and `thread_state: "paused"`. The
event names the recipient agent, and the push listener types no pointer
for it. The first held message also sends one notice from `system` to the
operator on the thread: "thread T needs a human: N agent replies without
operator input". A message from the operator on the thread ends the pause.
Each message on a paused thread has `thread_paused: true`.

`terrariumd sandbox-dry-run <agent>` prints the full sandbox command of an
agent.

### 6.3 Sessions

`GET /api/agents/{id}/sessions`: newest first. Cursor: session ID.

```json
[{"id":"01M5PA0T1NSYX43DDK1M2KT8KR","agent":"01M5PA0T0Z0K16V6G88P3SE78W",
  "harness_session_id":"01a1727a-c74a-4775-b7e8-97ff4e4add64","current":true,
  "started_at":"2026-10-24T06:36:54.197Z","turns":3,
  "transcript":true,"blocks":17,"title":"You now have access to target vlab until 2026-10-25T10:13:44.170Z. Run `terrariu",
  "active":true}]
```

An ended session adds `ended_at` and `outcome`: `stopped`, `restarted`,
`error`, `start_failed`, `superseded`, `exited`, `crashed` (a restart after
an exit), `resume_failed`, `daemon_restart`, or the reason the supervisor
gave.
`turns` counts the `turn_start` events of the session.

The transcript fields (§6.6):

| Field | Meaning |
|---|---|
| `transcript` | The daemon found the session file, and the file passed the path rule of §6.6. |
| `blocks` | The number of transcript blocks. `0` when there is no transcript. |
| `title` | The first user text that the harness did not make, on one line, at most 80 characters. `null` when there is none. |
| `active` | This is the agent's current session, and the agent runs. |

Two sessions can share one harness session (a restart resumes it). They
then show the same file.

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

### 6.6 Transcript

The daemon reads the session file of each harness and gives one block
model for all harnesses. Each adapter has the parser for its harness
(`crates/adapters/*/src/transcript.rs`); the module comment there gives the
map of each record type.

| Endpoint | Answer |
|---|---|
| `GET /api/agents/{id}/sessions/{session}/transcript?after=<seq>&limit=<n>` | `200` one page of blocks |
| `GET /api/agents/{id}/sessions/{session}/transcript/stream?after=<seq>` | Server-Sent Events: new blocks, updates, and the status |

Rights: the same as the agent's events (§9). A member of the agent's
project reads; an agent of another project gets `403 forbidden`; the
operator reads all.

`{session}` is the session `id` of §6.3. The harness session ID
(`harness_session_id`) also works; it names the newest session with that
harness session.

**Block**

```json
{"id":"msg_011CgNkaAdYdHCEbxuSH5HWk:0","session":"01M5PA0T1NSYX43DDK1M2KT8KR","seq":2,
 "at":"2026-10-25T09:53:46.168Z","role":"assistant","kind":"tool_call",
 "tool":{"id":"toolu_01TYKZi8migs8cqAkAYjEUBC","name":"Bash",
         "input":{"command":"terrarium targets ls","description":"List targets available to this agent"},
         "summary":"Bash: terrarium targets ls"},
 "model":"claude-haiku-5-5","usage":{"input":2,"output":82,"cache_read":12572,"cache_write":12679},
 "source":{"harness":"claude","file":"01a1727a-c74a-4775-b7e8-97ff4e4add64.jsonl","line":15}}
```

| Field | Meaning |
|---|---|
| `id` | Stable in the session: the harness's message or record ID and the index of the part (`msg_…:0`). |
| `seq` | From 1 for each session, with no gaps, in the order the blocks first came. |
| `at` | RFC 3339, or `null` when the record has no time. |
| `role` | `user`, `assistant`, or `system`. A `tool_call` has `assistant`; a `tool_result` has `user`. |
| `kind` | `text`, `thinking`, `tool_call`, `tool_result`, or `system`. |
| `text` | Markdown or plain text: `text`, `thinking`, and `system` blocks. Cut at 64 KiB with `truncated: true`. |
| `redacted` | `true` on a `thinking` block when the harness stores no thinking text (only a signature or encrypted content). |
| `origin` | On a user `text`: `operator`, `agent_mail_pointer`, `system`, or `user`. See below. |
| `tool` | On a `tool_call`: `id` (the harness's call ID), `name`, `input` (JSON; an input over 64 KiB becomes a cut string), and `summary` (one line). |
| `result` | On a `tool_result`: `tool_id` (the `tool.id` of its call), `output` (a string, cut at 64 KiB with `truncated: true`), and `is_error`. |
| `model` | The model of an assistant block, when the harness records it. |
| `usage` | `{input, output, cache_read, cache_write}` of one model response, on the **last** block of that response only. `input` does not include the cache. A new block of the same response moves it (an `update` of the block before). |
| `raw_type` | On a `system` block: the harness record type, for example `event_msg:turn_aborted`. A record that the parser does not know becomes a `system` block with `text` `[<raw type>]`, so nothing silently disappears. |
| `source` | `harness`, `file` (the file name), and `line` (from 1). |

An image or other binary content becomes a `system` block with the text
`[image omitted]` (in a tool result: that text in `output`). Records with
no conversation in them (usage counters, hook runs, title side calls, the
context that a harness injects) are left out. Each adapter's module comment
lists them.

**Origin.** The parser sets `system` on a user text that the harness made
(a `<task-notification>`, a `<system_notification>`, `<environment_context>`,
a meta message) and `user` on a typed prompt. The daemon then tags the
typed prompts that it typed itself: before it types a turn, it writes a row
to its delivery log (the SHA-256 of the text with whitespace collapsed, not
the text). `POST /api/agents/{id}/send` by the operator gives `operator`, a
push pointer (mail, card, lead, lease) gives `agent_mail_pointer`, and the
recap turn after a failed resume gives `system`. A send by an agent, and a
prompt that someone typed into the terminal, stay `user`.

**Page**

```
GET /api/agents/smoke%2Fprobe/sessions/01M5PA0T1NSYX43DDK1M2KT8KR/transcript?after=0&limit=2

200
{"session":"01M5PA0T1NSYX43DDK1M2KT8KR","harness":"claude","active":true,
 "blocks":[
  {"id":"248e9b6e-e0ec-49d1-b44c-6a9d2a7bf38b:0","session":"01M5PA0T1NSYX43DDK1M2KT8KR","seq":1,
   "at":"2026-10-25T09:53:44.196Z","role":"user","kind":"text","origin":"operator",
   "text":"Run `terrarium targets ls`.","source":{"harness":"claude","file":"01a1727a-….jsonl","line":5}},
  {"id":"msg_011CgNkaAdYdHCEbxuSH5HWk:0","seq":2,"role":"assistant","kind":"tool_call", …}],
 "next_after":2,"complete":false}
```

| Query | Meaning |
|---|---|
| `after` | Only blocks with a larger `seq` (exclusive). Default `0`: from the start. |
| `limit` | At most this many blocks. Default 500, at most 5000. |

`next_after` is the `seq` of the last block of the page (`after` when the
page is empty): give it as `after` for the next page. `complete` is `true`
when the page reaches the last block that the file has now. A page is at
most 2 MB of block JSON; it then stops early, with `complete: false`, but
it always has one block when one is left.

`active` is `true` when this is the agent's current session and the agent
runs. An active session whose file the harness has not written yet (Codex
and pi write it at the first message) gives an empty page, `complete:
true`. Another session with no file gives `404 not_found`.

**Stream.** `GET …/transcript/stream` with `Accept: text/event-stream`.
Frames:

| `event:` | `data:` |
|---|---|
| `block` | A new block, with `id: <seq>`. The blocks after `after` come first, then each new block. |
| `update` | A block that changed, with the same `id` and `seq`: for example the usage moved to a new last block, or Codex and Copilot wrote the usage after the message. Replace the block of that `seq`. No `id:` line. |
| `status` | `{"status", "active", "agent_status"}`. `status` is the agent status (`idle`, `working`, `blocked`, …) while the session is active, and `ended` when it is no longer the active one (the agent stopped, or a restart opened a new session). One frame at the start, then one at each change. |
| `error` | `{"code", "message"}`: the session file failed the path rule. The stream then ends. |

To resume, reconnect with `?after=<last block seq>` (or `Last-Event-ID`).
An `update` of a block at or before `after` that came while the client was
away is not sent again: read the page to get it. The stream reads the file
at each event of the agent and at least once a second. The stream does not
end when the session ends; the client closes it.

```sh
curl -s --unix-socket $S 'http://localhost/api/agents/smoke%2Fprobe/sessions/01M5PA0T1NSYX43DDK1M2KT8KR/transcript?limit=50'
curl -s -N --unix-socket $S -H 'Accept: text/event-stream' \
  'http://localhost/api/agents/smoke%2Fprobe/sessions/01M5PA0T1NSYX43DDK1M2KT8KR/transcript/stream?after=50'
```

```
event: status
data: {"status":"working","active":true,"agent_status":"working"}

id: 51
event: block
data: {"id":"msg_01…:0","seq":51,"role":"assistant","kind":"text","text":"done", …}
```

**Path rule.** The session file is the `transcript_path` of the session's
`session_started` event, else the adapter's lookup by the harness session
ID in the agent's own dirs. The rule of the usage collector applies
(`docs/security-review.md` F12): the file must be a regular file (not a
symlink) under the agent's own session area (`<state>/agents/<ulid>/harness/<harness>/…`;
with sandbox mode `none` also the shared harness home), the file name must
carry the harness session ID, and no other agent may have that harness
session. A path that breaks the rule gives `403 forbidden` ("the session
file is outside the agent's own dirs"), and the sessions list shows
`transcript: false`.

The daemon keeps the parsed blocks of the 16 newest files in memory. A read
checks the file length and parses only the new bytes; a file that got
shorter is parsed again from the top.

## 7. Mail

A message:

```json
{"id":"01M5PA19SD1TDA6NXK2M8FA79Y","thread_id":"01M5PA0VBXNH92Q6GY3B4XNHXC",
 "from":"project:ops","from_kind":"project","from_name":"project:ops",
 "to":"01M5PA0T0Z0K16V6G88P3SE78W","to_kind":"agent","to_name":"terrarium/worker",
 "subject":"Re: bug: deploy fails","body":"Accepted as OPS-1.","state":"queued",
 "in_reply_to":"01M5PA0VBXS5RFGE6YBFNX33EG","created_at":"2026-10-24T06:37:10.317Z",
 "held":false}
```

States: `queued`, `delivered`, `read`, `replied`. Mail to an agent waits in
`queued` for the runner to deliver it. Mail to the operator or to a project
inbox is `delivered` at once. `delivered_at` and `read_at` are absent until
set. The first read (`POST .../read`, or a reply) sets `read_by`, the key of
the reader (`operator` or an agent ULID), and `read_by_name`, and writes a
`mail.read` audit row. A read again changes nothing. After `agent rm`, the
`from_name` and `to_name` of the agent's mail keep its name, as the cards
do. A stored agent key that never was an agent shows as the key alone.

`held` is `true` when the loop guard held the message at the send (§6.2, the
loop guard): the message is `delivered` at once and types no push pointer.
A held message also has `held_reason`, for example `loop guard: message 7
of agent-to-agent mail on thread T since the last operator or project-inbox
message (limit 6)`. The daemon sets both at the send and does not change
them. Messages from before migration 16 have `held: false`.
`thread_paused` is about the thread now; `held` is about the message.

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
- `to` and `from` take every address kind of §2.3: `operator`, `system`,
  `project:<name>`, or an agent. Nothing goes to `system`, so `to=system`
  answers `[]`. An unknown project or agent is `404`.
- The `to` and `from` filters also find an agent that `agent rm` removed,
  through its tombstone (by ULID or by its old name). The `assignee` filter
  of the cards and the `author` filter of the memories do the same. A send
  to a removed agent stays `404`.
- The operator sees all mail. An agent sees mail to it or from it. With
  `to=project:<name>`, an agent needs the inbox right.
- `from` defaults to the actor. The operator may send as `project:<name>`.
  An agent may send as the inbox of a project it administers. A send to
  `system` is `422 invalid`; a send as `system` is `403 forbidden`.
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
| `GET /api/cards/{id}/events?since=` | | `200` card events, cursor: `seq`. `since` is a `seq`, an event ID, or a time (§2.1). |

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
| `status` | `{status, from, reason}`. `from` is never the same as `status`: a runner report of the status the agent already has (for example the runner's `spawned <program>` at start) goes in as a `system` event `{message, status}`. |
| `session_started` | `{harness_session, transcript_path?, source?}` |
| `turn_start`, `turn_end`, `notification`, `question`, `session_ended`, `exit`, `delivery_failed`, `system`, `warning` | The fields of the core `Event` variant of the same name (`crates/terrarium-core/src/event.rs`). A `warning` (`{message}`) needs the operator's attention, for example a harness version outside its tested range at a start (§6.2). |
| `tool_use`, `tool_finished` | `{tool, tool_id?, summary?, ...}`: `input` (`tool_use`), `ok` (`tool_finished`). `tool_id` is the harness's ID for the tool call (for Claude Code, `toolu_...`). It is not the event `id`. Older records used the key `id` for it; the `events.jsonl` reader still takes those. |
| `mail` | `{message_id, thread_id, from, from_name, to, to_name, subject}`. The `agent` is the recipient. A message that the loop guard holds also has `held: true`, `held_reason`, and `thread_state: "paused"`. |
| `card` | `{card_id, change, title, column, ...}`. `change` is `created`, `moved` (`from`, `to`, `comment`, `assignee`), `reordered`, `assigned` (`assignee`, `previous`), `claimed`, `edited` (`fields`), `commented` (`comment`), `linked` (`kind`, `ref`), `archived`, `unblocked` (`by`), or `thread_update` (`thread_id`, `done`). `actor_name` names who made the change (`operator` or `project/name`). The `agent` is the assignee. |
| `board` | `{columns}` after a column change. |
| `memory.created`, `memory.updated`, `memory.deleted`, `memory.linked`, `memory.unlinked`, `memory.superseded` | `{memory_id, key, title, ...}`: `fields`, `to`, `relation`, `note`, `by`. |
| `wiki.created`, `wiki.saved`, `wiki.deleted`, `wiki.restored`, `wiki.moved`, `wiki.reviewed` | `{page_id, path, title, rev, ...}`: `summary`, `author`, `from_rev`, `from`, `decision`, `comment`. |
| `artifact.created`, `artifact.linked`, `artifact.deleted`, `artifact.pruned` | `{artifact_id, key, uid, name, kind, ...}`: `size`, `content_type`, `sha256`, `link_kind`, `ref`. |
| `version.created`, `version.updated`, `version.submitted`, `version.approved`, `version.rejected`, `version.released` | `{version_id, name, status, ...}`: `fields`, `comment`, `thread_id`, `cards`. |
| `usage.sample`, `usage.rejected_path`, `quota.sample`, `quota.alert`, `quota.blocked`, `quota.resumed`, `subscription.offer`, `subscription.expired`, `self_report` | See §16.7. |
| `lease` | `{change, target, lease_id, expires_at, exclusive, state}`. `change` is `granted`, `extended`, or `ended` (a revoke or the expiry). The `agent` is the lease holder. The supervisor types a pointer from it (§17.7). |
| `target` | `{change, target, from, state, snapshots_cleared}`. `change` is `reconciled`: a `status` found the target other than the store said (§17.3). |
| `schedule` | `{change, schedule, name, project, run?, status?, trigger?}`. `change` is `created`, `updated`, `deleted`, `enabled`, `disabled`, `run_started`, or `run_finished`. `schedule` is the schedule ULID. A run event has `run`, `status`, and `trigger` (§19). |
| `harness` | `{change, harness, package, from, to, forced}`. `change` is `updated`: an update in the tools dir (§4, `POST /api/harnesses/{name}/update`). |
| `host.connected`, `host.disconnected` | `{host, host_id, ...}`: `version`, `os`, `arch`, `capabilities`, `summary`, `resumed` (sessions taken back), `reason`. See §17. |

A `memory.*`, `wiki.*`, `artifact.*`, or `version.*` event that an agent
causes names that agent in `agent` and `agent_id`, so it is in
`GET /api/agents/{id}/events`. When the operator or the daemon causes it,
the event names no agent.

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
| `since` | A `seq`, an event ID, `latest`, or a time (§2.1). Only later events come; a time gives the events at or after it. Default: 0, the whole log. `latest` skips the backlog: only events after the newest `seq` at connect time come. |
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
| `login_status` | `logged_in`, `expiring`, `expired`, `missing`, `unknown` (not checked yet), or `api_key` (a stored Z.ai key: no harness status command can check it, §16.5). See "Expired logins" in §16.2. |
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

Without `group=model`, a point also has `model` when all its samples have
one model. A point of more than one model has no `model`; use
`group=model` to split it. The model is the name in the session file (for
example `claude-haiku-5-5`), not the alias in the agent definition
(`haiku`).

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
| `POST /api/subscriptions` | `{name, provider, plan?, harnesses?, probe?, alert_at?}` | `201` with the subscription (`source = api`). `409 conflict` when the name exists; `422 invalid` for an unknown probe, or for provider `zai` with a plan that is not `coding` or `api`. |
| `GET /api/subscriptions/{name}` | | One subscription. |
| `DELETE /api/subscriptions/{name}` | | `204`. `409 file_managed` for a file subscription. |
| `GET /api/subscriptions/{name}/quota` | `?since=` (a time, §2.1; default 24 h ago), `?window=`, `?limit=` (default 1000) | Array of samples, oldest first: `{subscription, window, used_pct, resets_at, raw, at}`. `raw` is the provider's window object. |
| `POST /api/subscriptions/{name}/probe` | | `{subscription, probe_status, error?, windows}`. Here `subscription` is the **name** only, not the object. A failed probe is still `200`. |
| `POST /api/subscriptions/{name}/verify` | | `{subscription, status}`. Here `subscription` is the **full** subscription (§16.1), after the check. `status` is `{kind, harness, provider, pi_provider?, logged_in, plan?, detail?}`. The harness status command runs against the shared home and sets `login_status` (and `plan`, when the subscription has none). A stored API key counts as logged in: `status` is then `{logged_in: true, detail}`. |
| `POST /api/subscriptions/{name}/logout` | | The subscription (§16.1), with `login_status` `missing`. Runs `claude auth logout`, `codex logout`, or `copilot logout` against the shared home. pi has no logout command: `422 invalid`. A `zai` subscription has no logout either (`422 invalid`): the Claude logout would end the shared Claude login. |
| `POST /api/subscriptions/{name}/api-key` | `{provider, key, plan?, base_url?, models?}` | The subscription, without the key. `plan` is for `zai` only. See §16.5. |
| `GET /api/subscriptions/detect` | | Logins found in the harness homes that no subscription covers (same provider, and the harness is in `harnesses`): `[{kind, harness, provider, pi_provider?, logged_in, plan?, detail?, suggested}]`. `suggested` is a subscription definition. |
| `POST /api/subscriptions/login` | `{kind, provider?, name?}` | `201` with the login session (§16.4). |
| `GET /api/subscriptions/login/{id}` | | The login session. |
| `DELETE /api/subscriptions/login/{id}` | | `202`; the session becomes `cancelled`. `409 not_running` when the session ended (also when it is `done`). |
| `POST /api/subscriptions/login/{id}/input` | `{code}` | `204`. Types the code and Enter into the flow. `409 not_running` when the session ended. |
| `GET /api/subscriptions/login/{id}/pty` | WebSocket | The login terminal, with the protocol of §6.5. |
| `GET /api/usage` | `?subscription=&project=&agent=&model=&card=&since=&until=&bucket=hour\|day&group=subscription\|project\|agent\|model\|card` | Array of usage points, oldest first. `since` and `until` are times (§2.1). `since` defaults to 24 h ago. `agent` is an agent key (§2.2). `card` is a card ID, in any case. |
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
 "today":{"since":"2026-10-24T00:00:00.000Z","mine":{"input_tokens":4,"output_tokens":55,"cache_read":36712,
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
| `zai` (takes `plan`) | `ZAI_API_KEY`, and the Claude Code vars below |

A new name makes an `api` subscription (harness `pi`, or `copilot` for
`github`, or `claude` and `pi` for `zai`). No endpoint returns the key. The
audit row `subscription.api_key` has the provider, the plan, the env var,
`base_url`, and `models`. A `plan` with another provider is `422 invalid`.

**Z.ai (GLM).** `provider` `zai`, `plan` `coding` (the GLM Coding Plan,
the default) or `api` (pay as you go). Another plan is `422 invalid`. The
subscription gets `harnesses = ["claude", "pi"]`, no probe
(`local_estimate_only` is true: Z.ai documents no usage endpoint, see
`docs/compat.md`, "Z.ai"), and `login_status` `api_key`. Verify sets
`api_key` again when the key is stored, else `missing`. It never runs
`claude auth status`. The env of an agent on it:

| Var | Value |
|---|---|
| `ZAI_API_KEY` | the key (pi) |
| `ANTHROPIC_BASE_URL` | `https://api.z.ai/api/anthropic` |
| `ANTHROPIC_AUTH_TOKEN` | the key (Claude Code sends it as a bearer token) |
| `API_TIMEOUT_MS` | `3000000` |
| `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL` | `glm-5.3` |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | `glm-5.3-flash` |

There is no `ANTHROPIC_API_KEY`. Claude Code gets `--model` with a GLM id:
the agent's `model` as it is (`glm-4.6`), `glm-5.3-flash` for `haiku` or a
Claude Haiku id, else `glm-5.3`. pi gets `--provider zai` (plan `coding`,
pi's built-in provider, `https://api.z.ai/api/coding/paas/v4`) or
`--provider zai-api` (plan `api`, `https://api.z.ai/api/paas/v4`), and
`--model` with the agent's model or `glm-5.3-flash`. `zai-api` is a
custom provider in the agent's `models.json`: every pi agent gets a
read-only copy of the shared `models.json` with `zai-api` added.

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
  mail links the project page, `<TERRARIUM_PUBLIC_URL>/p/<project>`.
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
| `usage.sample` | `{subscription, harness, model, input_tokens, output_tokens, cache_read, cache_write, messages}`: the new usage of one agent since its last `usage.sample`, at most one each 10 s per agent. `model` is the model of the newest response, as the session file names it (the usage store has the same name); the agent definition's model only when the file names none. |
| `usage.rejected_path` | `{harness, harness_session, path, reason}`: the usage collector refused the session file that a `session_started` hook named. The file must be a regular file (not a symlink) under the agent's own session dir (`harness/claude/projects/`, `harness/codex/sessions/`, `harness/copilot/session-state/`, `harness/pi/sessions/`; with sandbox mode `none` also the session area of the shared home), and another agent must not read it. A Claude Code, Codex, or Copilot file must also carry the session ID in its name (`docs/security-review.md` F12). The collector does not read a refused file. |
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
all projects. Targets are project targets by default: a global target
(no `project`) needs a sysadmin or the operator.

```json
{"id":"terra-box","name":"terra-box","kind":"docker","project":"terra","host":"lab1","image":"alpine:3",
 "ephemeral":true,"keep":false,"snapshot_before_task":false,
 "scope":"project","source":"api","state":"running","state_at":"2026-10-24T06:40:00.000Z",
 "snapshots":[],"host_online":true,"usable":true,
 "runtime":{"attached":["terra/w-ter-3"]}}
```

| Field | Meaning |
|---|---|
| `scope` | `global` (no `project`) or `project`. Every target view has it, also in `GET /api/projects/{name}/targets`. |
| `state` | `running`, `stopped`, `absent`, or `unknown`: what the last lifecycle verb saw. |
| `snapshots` | The snapshot names that `POST .../snapshot` or `snapshot_before_task` took. A destroy (the state `absent`) clears them. |
| `host_online` | The link of the target's host agent. `null` for `ssh`. |
| `usable` | The caller may use the target (§17.4). |
| `access` | Why the caller may use it: `operator`, `policy`, `lease`, or `null`. |
| `lease` | The caller's active lease on the target (§17.7), when `access` is `lease`; else `null`. |
| `runtime` | What runs now (§17.8): `attached` (the agent workspaces, `<project>/<agent>`, in the container or the lab instance), `lab` and `head` (the instance dir on the host and the commit of a `lab_repo` target), `instances` (one for each holder of a `lab_from_workspace` target, by the agent ULID: `{agent_name, lab, state, attached}`). `{}` when nothing runs. |

**The project prefix.** `POST /api/targets` with a `project` names the
target `<project>-<name>`, unless `name` starts with `<project>-` already.
So two leads cannot take the same name. The answer has the full name. A
global target, a file target, and a fleet target keep their names.

**The project cap.** A project has at most
`TERRARIUM_MAX_TARGETS_PER_PROJECT` targets (default 8). A `POST
/api/targets` past it is `409 conflict`, with `details: {project, targets,
max}`.

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/targets?usable=&project=` | | `200` targets by name | read |
| `POST /api/targets` | target, as in a target file, with `name` | `201` target | project_admin of `project`; sysadmin for a global target |
| `GET /api/targets/{name}` | | `200` target | read |
| `PATCH /api/targets/{name}` | JSON merge patch; `null` removes a field | `200` target; `409 conflict` while it runs (below) | as `POST` |
| `DELETE /api/targets/{name}` | | `204` | as `POST` |
| `GET /api/projects/{name}/targets` | | `200` the project's and the global targets, each with `scope` | read |

An agent reads the global targets and the targets of its own project. A
sysadmin agent and the operator read all. `?usable=true` keeps only the
targets the caller may use. `terrarium targets ls` sends it in agent mode.
`terrarium admin target ls` in agent mode reads `GET
/api/projects/<own>/targets`: every target of the project and the global
ones, with the `SCOPE` column.

The fields that each kind needs:

| `kind` | Needs | Optional |
|---|---|---|
| `host` | `host` | |
| `ssh` | `address` | `port`, `credential` (an `ssh_key` in the credential store) |
| `docker` | `host`, and `image` or `container` | `ephemeral`, `keep` |
| `vmlab` | `host`, and one of: `lab` (an absolute path); `lab_repo` with `machine`; `lab_from_workspace = true` with `machine` | `machine`, `lab_ref`, `lab_path`, `ephemeral`, `keep`, `snapshot_before_task` |

The three kinds of vmlab target (§17.8):

| Field | The lab |
|---|---|
| `lab` | A lab dir on the host that the operator wrote. No workspace attach. |
| `lab_repo` | The name of a repo of the target's project. On `up`, the host agent makes a git worktree of it at `lab_ref` (default: the repo's default branch, else `main`), with `vmlab.wcl` in `lab_path` (default: the root). One instance for the target. |
| `lab_from_workspace` | The `vmlab.wcl` in the workspace of each holder, in `lab_path`. Each holder runs its own instance. No `snapshot_before_task`, and no `exclusive` lease (§17.7). |

`lab_repo` and `lab_from_workspace` need a project target. `lab_path` is a
relative directory with no `..`. `lab_ref` takes the rules of a positional
value.

**`lab_path` and the workspace root.** With `lab_repo`, `lab_path` is
relative to the root of the repo. With `lab_from_workspace`, `lab_path` is
relative to the workspace root `<work>/<project>/<agent>`, not to a repo.
A checkout is one level down, in `<work>/<project>/<agent>/<repo>`. So a
`vmlab.wcl` at the root of the repo `hello-svc` is `"lab_path":
"hello-svc"` (`--lab-path hello-svc`), never `.`. When the host agent
finds no lab file, the error says "lab_path is relative to the workspace
root" and names the directories one level down that hold a `vmlab.wcl`:
"try --lab-path hello-svc".

**A change to a running target.** While the target runs (its `state`, or
the state of one of its lab instances, is `running`), `PATCH` changes only
`keep` and `snapshot_before_task`. A change to any other field is `409
conflict`, with `details: {state, fields}`. Bring the target down first. A
field with its current value is no change. `terrarium admin target edit`
sends this `PATCH`.

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
empty, or `{"name"?, "discard_guest_changes"?, "destroy"?, "agent"?}`.
`agent` names the holder whose instance of a `lab_from_workspace` target
the verb acts on; an agent caller always acts on its own. The caller
may use the target (§17.4), or may define it (§17.2). The answer is the
executor report, not the target: `{"target", "verb", "state", "snapshot"?,
"result"?}`. `target` is the name, `state` is the new state, `snapshot` is
there for `snapshot` and `restore`, and `result` is the result of the
host agent verb when it gives one. The daemon stores the state and writes
a `target.<verb>` audit row with the same report.

| Kind | `up` | `down` | `snapshot` / `restore` | `status` |
|---|---|---|---|---|
| `host`, `ssh` | no-op | no-op | `422 invalid` | `running` when the host agent is online, else `unknown` |
| `docker` | `docker.inspect`; when absent, `docker.run -d --init --name <c> --label terrarium.target=<name> <image> sleep infinity`; when stopped, `docker.start` (an ephemeral target with `image`: `docker.rm -f -v` and a new `docker.run`) | `docker.stop`; an ephemeral target without `keep` (or `destroy: true`): `docker.rm -f -v` | `422 invalid` | `docker.inspect` |
| `vmlab` | `vmlab.up [machine]` | `vmlab.down [machine]`; ephemeral without `keep` (or `destroy: true`): `vmlab.destroy` | `vmlab.snapshot_take` / `vmlab.snapshot_restore` with `--vm machine` | `vmlab.status`, the row of `machine` (no row: `absent`). A lab that is not running (not in `vmlab lab list`): `absent` for an ephemeral target without `keep`, else `stopped` |

The container `<c>` is `container`, else `terrarium-<name>`. The snapshot
name is `name`, else `terrarium`.

**Reconcile.** A `status` makes the stored state match the host. When the
host agent connects (after `welcome`), the daemon runs `status` (actor
`system`) on each docker and vmlab target of that host. A lab instance
target (`lab_repo`, `lab_from_workspace`) is not part of this. A docker
container that `docker.inspect` does not find is `absent`; an exited one
is `stopped`. A target that becomes `absent` loses its `snapshots`. When
the state or the snapshots change, the daemon writes the audit row
`target.reconcile` (`{from, state, snapshots_cleared}`) and a `target`
event (§9). A `status` that finds the stored state writes neither.

The daemon runs some verbs by itself, with the actor `system` and a
`reason` in the audit detail. On an agent start, for each target in
`policy.targets` and each target of an active lease (§17.7) that no other
running agent holds:

1. An ephemeral target gets `up` (`target.up`, reason `agent_start`).
2. A target with `snapshot_before_task` on a kind with snapshots (vmlab)
   gets the snapshot `terrarium-clean` after `up` and before any exec
   (`target.snapshot`, reason `snapshot_before_task`). The daemon takes it
   only when `snapshots` does not hold `terrarium-clean`. A permanent target
   gets it on the first agent start that finds it `running`. A docker
   target has no snapshots, so the flag does nothing.

On an agent stop, or an exit with no restart, for each such target that
no other running agent holds:

1. An ephemeral target without `keep` gets a destroy (`target.destroy`,
   reason `agent_stop`). The destroy clears `snapshots`, so the next start
   takes a new `terrarium-clean`.
2. An ephemeral target with `keep` and `snapshot_before_task` gets a
   restore of `terrarium-clean` (`target.restore`, reason `agent_stop`). It
   stays up, as a kept target does, and the next agent finds it clean.
3. A permanent target is not touched. A verb on an offline host is `409
conflict`. A verb that fails is `422 invalid` with `details: {verb, code,
end, stderr, stdout}`.

An `up` that starts a docker or vmlab target (or a lab instance) when its
host runs `max_running_targets` of them already (the host agent's config,
default 4; `GET /api/hosts/{name}` shows it) is `429 rate_limited`. The
count is the targets and instances on the host that the store shows as
`running`.

An `up` of a docker target binds the workspaces of the running holders
(§17.8). When the container runs already without one of them, the answer
has `attach_error` (a sentence that starts "attach needs a restart of the
target"), unless the daemon could run a new container.

### 17.4 Exec sessions

An exec session runs one command, one shell, or one file copy. It uses
plain requests only, so it works through the agent tunnel too
([agent-rpc.md](agent-rpc.md), "Streams over the tunnel").

| Endpoint | Body or query | Answer |
|---|---|---|
| `POST /api/targets/{name}/exec` | `{"cmd": [..], "cwd"?, "env"?, "pty"?, "shell"?, "timeout_secs"?, "agent"?}` | `201` `{"session_id", "target", "kind"}` |
| `POST /api/targets/{name}/cp` | `{"direction": "push"\|"pull", "path", "mode"?, "agent"?}` | `201` `{"session_id", "target", "kind"}` |
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
- `agent`: on a `lab_from_workspace` target, the holder whose instance runs
  the command. An agent caller uses its own; the operator must name one
  (`422 invalid` without it). A `lab_repo` or `lab_from_workspace` target
  with no instance on the host is `409 conflict`: bring it up first.

The input body: `data` is base64, at most 1 MiB of bytes (`413
too_large` above). In a push, `data` is file bytes. `resize` is
`{"rows", "cols"}`. `signal` is a name such as `INT`. `eof` closes stdin.
`finish` is `{"bytes", "sha256"}` and ends a push. An input to a session
that has ended is `409 conflict`, except `eof` alone: it is a no-op with
`"accepted": false`. A fast command can end before the client closes its
stdin. `terrarium exec -i` and `shell` treat a `409` for `data` in the
same way: they stop the input, and the exit code comes from the output.

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
    `path_denied`, `checksum_mismatch`, `lost`, `killed`, or
    `lease_revoked` (the agent's lease on the target ended, §17.7).
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
  `policy.targets` lists, or that it holds an active lease on (§17.7),
  when the target is global or in the agent's project. Else the answer is
  `403 forbidden`, and the message names the rule. After a lease ended,
  the message is `your lease on <target> ended`. The role does not change
  this rule.
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
target that an agent holds: the names in its `policy.targets` and the
targets of its active leases (§17.7). They skip a target that another
running agent holds (by policy or by lease): it is up already, and it must
stay up. The agent's workspace joins or leaves it then (§17.8). A
`lab_from_workspace` target is not skipped: each holder has its own
instance. `RunnerSupervisor::start` calls
`on_agent_start` before the harness launches. A target that does not come
up does not stop the start: the agent gets a `notification` event with the
error. The supervisor calls `on_agent_stop` on a stop, on an exit that does
not restart (also one that the daemon finds after its start), and when a
start or a restart fails. A restart of the harness and a daemon shutdown
leave the targets as they are. Each of these verbs writes an audit row
with the actor `system`: `target.up` with `"reason": "agent_start"`, and
`target.destroy` with `"reason": "agent_stop"`.

### 17.7 Target leases

A lease gives one agent one target at run time, for a time or until a
revoke. A lead uses it to give a shared target (for example one VM) to a
worker for one card, and to take it back. The daemon enforces it at once.

```json
{"id":"01JC...","target":"vm","agent":"01M5...","agent_name":"terra/w-ter-3","project":"terra",
 "granted_by":"agent:01M4...","granted_at":"2026-10-25T09:00:00.000Z",
 "expires_at":"2026-10-25T09:30:00.000Z","exclusive":true,
 "revoked_at":null,"revoked_by":null,"reason":"TER-3","state":"active"}
```

| Field | Meaning |
|---|---|
| `agent`, `agent_name` | The holder: its ULID, and `project/name` (`null` after the agent is deleted). |
| `project` | The holder's project. |
| `granted_by`, `revoked_by` | `operator`, `agent:<ulid>`, or `system` (the expiry). |
| `expires_at` | The end. `null`: until a revoke. |
| `exclusive` | No other agent may hold a lease on the target at the same time. |
| `state` | `active`, `revoked`, or `expired`. |

| Endpoint | Body or query | Answer | Needs |
|---|---|---|---|
| `POST /api/targets/{name}/leases` | `{"agent", "until"?, "exclusive"?, "reason"?}` | `201` lease, with `lifecycle` or `lifecycle_error` when the grant ran a verb. With `lifecycle_error`, the lease is made, but the target is not up; `terrarium admin target grant` then exits `1` | below |
| `GET /api/targets/{name}/leases` | `?all=true` also lists the ended leases (the last 100) | `200` leases, newest first | read the target |
| `PATCH /api/targets/{name}/leases/{agent}` | `{"until"}`; `null`: until a revoke | `200` lease | below |
| `DELETE /api/targets/{name}/leases/{agent}` | | `200` the ended lease | below, or the holder itself |
| `GET /api/agents/{id}/leases` | `?all=true` | `200` leases | read the agent |

`{agent}` and `agent` are an agent key (§2.2). `until` is an RFC 3339
time in the future, or a duration from now: `90s`, `30m`, `2h`, `1d`.

Rules:

- The operator and a sysadmin may lease any target to any agent. A
  project_admin may lease a target of its own project to an agent of its
  own project. A global target needs a sysadmin or the operator, as a
  global target in a `policy` does (F13). Else `403 forbidden`.
- A project target goes only to an agent of its project (`422 invalid`).
- An agent has at most one active lease on a target. A second grant is
  `409 conflict`: extend it with `PATCH`, or revoke it first.
- `exclusive`: a new exclusive lease, while another agent holds any active
  lease on the target, is `409 lease_held`. So is any new lease while
  another agent holds an active exclusive lease. The message and the
  details name the holder and its `expires_at`. Several leases that are not
  exclusive can be on one target. A `policy.targets` holder does not
  count: a lease does not take a target from its policy holders.
- A lease is active while it is not revoked and `expires_at` is later than
  now. The policy check uses the time, so a lease ends at its time. The
  expiry timer runs at each `expires_at`, and every 10 s.
- A `lab_from_workspace` target gives each holder its own lab instance, so
  an `exclusive` grant on it is `422 invalid`. A grant to a running agent
  brings up that agent's own instance; the end of its lease removes it.
- When a lease ends (revoke or expiry), the daemon ends the holder's live
  exec sessions on the target: each ends with `{"type": "error", "code":
  "lease_revoked"}`. A new exec, shell, cp, checkout, or lifecycle verb is
  `403 forbidden`, `your lease on <target> ended`. A holder whose
  `policy.targets` lists the target keeps its sessions and its access.
- Lifecycle: a grant to a running agent brings an ephemeral target up with
  `ensure_up` (and `snapshot_before_task`), unless another running agent
  holds it (`target.up`, reason `lease_grant`); then the agent's workspace
  joins it (`target.attach`, §17.8). The end of the lease of a
  running agent that leaves no running holder calls `maybe_down`, with the
  keep and restore rules of §17.3 (`target.destroy` or `target.restore`,
  reason `lease_end`); with other running holders, its workspace leaves the
  target (`target.detach`). A grant to a stopped agent runs no verb: its next
  start brings the target up (§17.6).
- Audit rows: `target.grant`, `target.revoke`, `target.lease_extend`, and
  `target.lease_expired` (actor `system`). Deleting a target ends its
  leases.
- Each change writes a `lease` event (§9) to the holder's stream. The
  supervisor types a pointer, or keeps it for the next start of a stopped
  agent, also across a daemon restart (the `pending_pointers` table). On a
  grant, the pointer goes out after the lifecycle step of the
  grant (`ensure_up` and the snapshot), and it reads the target state and
  the expiry just before it goes out: "You now have access to target <t>
  (<state>) until <time or 'revoked'>. Run `terrarium targets ls`." The
  state is `running`, `stopped: ask your lead to bring it up` (also
  `absent: ...`), or the stored state (`unknown`). On a revoke or the
  expiry: "Your access to target <t> ended." An extend types no pointer. A
  grant and a revoke before the pointer goes out type only the "ended"
  line.

### 17.8 Workspaces and lab instances

PLAN §11, "mode 1". The workspace of an agent lives on the host:
`<TERRARIUM_WORK_DIR>/<project>/<agent>` (`/work` in the container, the
`./work` bind mount). The ULID-keyed agent dir keeps `private/` and
`harness/`. The sandbox shows the workspace at `$HOME/work` and at
`/work/<project>/<agent>`. A host agent attaches it into the docker and
vmlab targets that the agent holds at the same `/work/<project>/<agent>`,
so the harness edits files in the sandbox, the compilers run on the target,
and the paths in their messages are the same on both sides. The host
agent's `work_dir` must be the host path of the work root
([host-protocol.md "Workspaces"](host-protocol.md#workspaces)).

**Attach.** When an agent starts with a target (by policy or by lease), or
gets a lease while it runs, its workspace joins the target:

| Case | What the daemon does |
|---|---|
| No other running holder | The ephemeral target comes up (§17.6) with the workspaces of its running holders: `docker run -v` for each, or `vmlab.prepare` with an `@dev` decorator (one workspace for each machine). |
| The target runs, the workspace is attached | Nothing. |
| The target runs without it, ephemeral, no live exec session | A new container (or lab instance) with the new set: `docker rm`, `docker run`; or `vmlab destroy`, `vmlab.prepare`, `vmlab up`. Audit row `target.attach`. |
| The target runs without it, and it is permanent or a session is live | Nothing runs. The agent gets the notification "target `<t>`: attach needs a restart of the target: ...", and a grant answers it in `lifecycle_error`. |

A docker target with `container` (an existing container) and a vmlab target
with `lab` take no workspace.

**Detach.** When a holder stops, or its lease ends, and other running agents
still hold the target, a new container (or lab instance) without its
workspace runs, with the same rule (ephemeral, no live session; audit row
`target.detach`). Else the workspace stays attached until the target runs
again. When the last holder goes, the destroy of §17.6 removes it all.

**Lab instances.** A `lab_repo` target has one instance, named after the
target, in a git worktree of the repo on the host. A `lab_from_workspace`
target has one instance for each holder, named `<target>.<agent ULID>`,
from the `vmlab.wcl` in the holder's workspace. Each instance is a
directory `<instances_dir>/<instance>/lab` on the host, with the lab file
that the host agent made after its allowlist check
([host-protocol.md "The lab allowlist"](host-protocol.md#the-lab-allowlist)).
`up` runs `vmlab.prepare` when the instance does not run, then `vmlab up`;
a destroy runs `vmlab destroy` and `vmlab.release`, which also removes the
worktree. `DELETE /api/targets/{name}` destroys the instances first. A
refused lab is `422 invalid` with `details.code` `refused` and every
problem in the message.

vmlab 0.33.3-alpha keys a running lab by the name in its `lab` block: a
second instance of the same lab file is `exit 5` ("lab ... is already
registered"). So until vmlab runs one lab instance for each directory, two
holders of a `lab_from_workspace` target need lab files with two names
(docs/compat.md, "vmlab workspace attach").

**Migration.** At its start, the daemon points each agent at
`<work>/<project>/<agent>`. At the agent's next start (and before a
checkout into its workspace while it does not run), the files of the old
`<state>/agents/<ulid>/workspace` move there once. A new path that has files
already is not touched, and the old files stay (a log line says so). The
move is a rename, or a copy and a delete across file systems.

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
| `kind` | `subscription`, `project`, `target`, `agent`, or `schedule`. |
| `key` | The name; `project/name` for an agent and a schedule. |
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

## 19. Schedules

A schedule runs one action on a time spec: mail, a turn, an exec on a
target, an agent verb, or a quota probe. The daemon runs it as the actor
that made it (`run_as`). Before each run, the daemon checks the rights of
that actor again. PLAN §12b has the design.

### 19.1 Shapes

A schedule:

```json
{
  "id": "01MA0V7N6Q1Y3K2P4R5S6T7U8V",
  "name": "standup",
  "project": "terrarium",
  "enabled": true,
  "spec": {"kind": "cron", "expr": "0 9 * * MON-FRI", "tz": "Europe/London"},
  "action": {"type": "mail", "to": "lead", "subject": "standup", "body": "Post your status."},
  "catch_up": "skip",
  "max_concurrency": 1,
  "created_by": "agent:01M5PA0T0Z0K16V6G88P3SE78W",
  "run_as": "agent:01M5PA0T0Z0K16V6G88P3SE78W",
  "next_run_at": "2026-10-27T09:00:00.000Z",
  "last_run": {"at": "2026-10-26T09:00:01.204Z", "status": "ok",
               "summary": "mail 01MA... to terrarium/lead", "run_id": "01MA0V9..."},
  "runs_count": 12,
  "source": "api",
  "consecutive_skips": 0,
  "created_at": "2026-10-20T08:00:00.000Z",
  "updated_at": "2026-10-20T08:00:00.000Z"
}
```

| Field | Meaning |
|---|---|
| `name` | 1-63 of `a-z`, `0-9`, `-`, `_`. Unique in its scope: the project, or the global scope. |
| `project` | The project. `null` is a global schedule. |
| `spec` | When it runs. See below. |
| `action` | What it does. See below. |
| `catch_up` | `skip` (the default) or `once`. What the daemon does at its start with a run that it missed while it was down. |
| `max_concurrency` | Always `1`: one run at a time. |
| `created_by` | `operator` or `agent:<ulid>`. |
| `run_as` | The actor of the runs. It is `created_by`, until a caller changes the `action`: then it is that caller. |
| `next_run_at` | The next run. `null` when the schedule is disabled, or when an `at` schedule has run. |
| `last_run` | `{at, status, summary, run_id}` of the last run, or `null`. |
| `runs_count` | The runs so far, the skipped runs too. |
| `source` | `api`, `file` (`[[schedules]]` in `projects/<name>.toml`), or `fleet:<name>` (§18.1). |
| `consecutive_skips` | The runs in a row that the rights check skipped. |
| `disabled_reason` | Why the daemon disabled the schedule. Absent when it did not. |

The specs:

| `spec` | Meaning |
|---|---|
| `{"kind": "cron", "expr": "*/15 * * * *", "tz": "UTC"}` | Five fields: minute, hour, day of month, month, day of week. The day of week is `0`-`7` (`0` and `7` are Sunday) or `SUN`-`SAT`. `@hourly`, `@daily`, `@weekly`, `@monthly`, and `@yearly` are also valid. `tz` is `UTC` (the default) or an IANA zone, for example `Europe/London`. |
| `{"kind": "interval", "every": "30m"}` | A number and a unit (`s`, `m`, `h`, `d`), or a sum, for example `1h30m`. The minimum is 60 s. The runs are at `start + k * every`; a new spec sets a new start. |
| `{"kind": "at", "at": "2026-11-01T02:00:00Z"}` | One run at an RFC 3339 time in the future. |

The actions:

| `action` | Meaning |
|---|---|
| `{"type": "mail", "to": "<agent>\|project:<p>\|operator", "subject", "body", "thread"?}` | Send mail through the normal send path (§7) as `run_as`. The scope rules of §7.1 and the loop guard apply. With `thread`, each run posts on that thread, and the loop guard counts the messages. A bare agent name resolves in the schedule's project. An empty `subject` becomes `schedule <name>`. |
| `{"type": "turn", "agent", "text", "start_if_stopped"?: false}` | Type a turn, as `POST /api/agents/{id}/send` (§6.2). When the agent does not run, the run is `skipped: agent not running`. With `start_if_stopped`, the run starts the agent, waits until it is ready (at most 120 s), then types the turn. |
| `{"type": "exec", "target", "cmd": ["make", "test"], "cwd"?, "timeout_secs"?}` | Run a command on a target through the executor (§17.4), as `run_as`, with the same policy, leases, and `target.exec` audit row as any exec. `timeout_secs` is 300 by default, 3600 at most. |
| `{"type": "agent", "agent", "verb": "start\|stop\|restart"}` | The agent verb of §6.2, as `run_as`. |
| `{"type": "probe", "subscription"}` | Run the quota probe of the subscription (§16.3). |

A run:

```json
{
  "id": "01MA0V9...",
  "schedule": "01MA0V7N6Q1Y3K2P4R5S6T7U8V",
  "started_at": "2026-10-26T09:00:00.112Z",
  "finished_at": "2026-10-26T09:00:01.204Z",
  "status": "ok",
  "trigger": "cron",
  "summary": "exec on build-box: exit 0",
  "output": {"target": "build-box", "cmd": "make test", "exit_code": 0,
             "stdout": "...", "stderr": "", "end": {"type": "exit", "code": 0, "reason": "exited"}},
  "error": null,
  "actor": "agent:01M5PA0T0Z0K16V6G88P3SE78W"
}
```

| Field | Meaning |
|---|---|
| `status` | `running`, then `ok`, `failed`, or `skipped`. |
| `trigger` | `cron` (the timer, for each spec kind), `manual` (`POST .../run`), or `catch_up`. |
| `summary` | One line. A skip starts with `skipped:`. |
| `output` | `exec`: `target`, `cmd`, `exit_code`, `end`, and the last 4000 bytes of `stdout` and `stderr`. `mail`: `message_id`, `thread_id`, `to`, `held`, `held_reason`. `turn`: `agent`, `started`, `delivery`, `status_before`. `agent`: `agent`, `verb`, `status`. `probe`: the probe outcome with `windows`. |
| `error` | Why the run failed or was skipped. |
| `actor` | Who triggered the run: `run_as` for a timed run, the caller for a run-now. |

### 19.2 Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/schedules?project=&enabled=` | | `200`, a list. With `project`: the project's schedules, and the global ones when the caller may read them. Without: all for the operator and a sysadmin; the caller's project for another agent. |
| `POST /api/schedules` | `{name, project?, spec, action, enabled?: true, catch_up?: "skip"}` | `201`, the schedule. An absent `project` is the agent caller's project (the operator: global). `null` is global. |
| `GET /api/schedules/{id}` | | `200`, the schedule. `{id}` is a ULID, `project/name` (encode the `/` as `%2F`), or a name. |
| `PATCH /api/schedules/{id}` | `{name?, spec?, action?, enabled?, catch_up?}` | `200`, the schedule. A new `action` makes the caller the `run_as`. |
| `DELETE /api/schedules/{id}` | | `204`. The runs go with it. |
| `POST /api/schedules/{id}/run` | | `200`, the run when it ends. It runs as `run_as`. |
| `POST /api/schedules/{id}/enable` | | `200`, the schedule, with a new `next_run_at`. An enable sets `consecutive_skips` to 0. |
| `POST /api/schedules/{id}/disable` | | `200`, the schedule. |
| `GET /api/schedules/{id}/runs?limit=&cursor=` | | `200`, the runs, newest first, with `X-Next-Cursor` (§2.4). |

Errors:

| Error | When |
|---|---|
| `422 invalid` | A field is not valid. The message starts with the field, and `details.field` names it: `name`, `spec`, `spec.expr`, `spec.tz`, `spec.every`, `spec.at`, `action`, `action.to`, `action.body`, `action.agent`, `action.text`, `action.target`, `action.cmd`, `action.cwd`, `action.timeout_secs`, `action.subscription`, `max_concurrency`, `project`. A cron error shows the expression: ``spec.expr: bad cron expression `*/15 * * *`: use five fields ...``. An interval under 60 s and an `at` in the past are `422`. |
| `403 forbidden` | The rules of §19.3. |
| `409 conflict` | The name is in use in the scope, or the project has `TERRARIUM_MAX_SCHEDULES_PER_PROJECT` (default 20) schedules. |
| `409 file_managed` | A `PATCH`, `DELETE`, `enable`, or `disable` of a `file` schedule. Edit `projects/<name>.toml`, then `POST /api/reload`. A run-now is allowed. |

Example: a lead reminds its worker every weekday at 09:00 London time.

```sh
curl --unix-socket "$SOCK" -H 'content-type: application/json' localhost/api/schedules -d '{
  "name": "standup", "project": "terrarium",
  "spec": {"kind": "cron", "expr": "0 9 * * MON-FRI", "tz": "Europe/London"},
  "action": {"type": "mail", "to": "worker", "subject": "standup", "body": "Post your status on your card."}}'
```

Example: run the tests on a target every 30 minutes, and read the last run.

```sh
curl ... localhost/api/schedules -d '{"name": "tests", "project": "terrarium",
  "spec": {"kind": "interval", "every": "30m"},
  "action": {"type": "exec", "target": "terrarium-box", "cmd": ["make", "test"], "timeout_secs": 900}}'
curl ... 'localhost/api/schedules/terrarium%2Ftests/runs?limit=1'
```

### 19.3 Rules

| Caller | Read | Create, change, delete, run |
|---|---|---|
| operator | all | all |
| `sysadmin` | all | all, global schedules too |
| `project_admin` | its project's | its project's, when the action stays inside the project (below) |
| `member` | its project's | no |

"Inside the project" for a `project_admin`:

- `mail`: to an agent of the project, to `project:<project>`, or to `operator`.
- `turn` and `agent`: an agent of the project.
- `exec`: a global target or a target of the project. At run time, the
  executor checks the policy and the leases of `run_as`, as for any exec.
  When `run_as` may not use the target, the run is `failed` with the
  policy error.
- `probe`: a subscription that an agent of the project uses.

A global schedule needs a sysadmin. An enable and a run-now check the
action again; a disable and a delete check only the scope. An agent that
enables or runs a schedule whose `run_as` is another actor must also be
able to do the action itself: use the target of an `exec` (its policy or
a lease), and mail the recipient of a `mail` under its own §7.1 rules.
Else it is `403`, so a run-now cannot borrow the rights of `run_as`.

**The rights check at each run.** Before each run, the daemon reads the
`run_as` actor from the store and applies the rules above to it. When the
agent is gone (`agent rm`), lost its role, or the action names an object
that is gone, the run is `skipped` with the reason, and
`consecutive_skips` goes up by 1. At 3, the daemon disables the schedule,
writes `disabled_reason`, and mails the operator from `system` ("Schedule
`<project>/<name>` disabled"). Any other end of a run sets the count to 0.
A skip for another reason (`still running`, `agent not running`) does not
count.

**Safety.**

- One run at a time: a trigger while a run of the schedule is `running`
  makes a `skipped: still running` run.
- The minimum interval is 60 s. A cron expression has five fields, so it
  cannot run more often than once a minute.
- An exec run has a limit of 300 s by default, 3600 s at most. At the
  limit, the daemon kills the command, and the run is `failed`.
- Mail goes through the normal send path: the sender is `run_as`, so a
  schedule that an agent made sends as that agent, with its
  `policy.mail_to` and `cross_project`. The loop guard (PLAN §12) holds a
  message on a paused thread; the run is `ok`, and its summary says that
  the message is held.

**The timer.** Every 10 s, the daemon starts the runs of the enabled
schedules whose `next_run_at` has passed, and computes the next run of
each (in the `tz` of a cron spec). At a daemon start, a run that the old
daemon left `running` becomes `failed` ("the daemon stopped during the
run"). A schedule whose next run passed more than one tick before the
start computes its next run from now; with `catch_up: once`, it also
runs one time, with the trigger `catch_up`.

**Retention.** The daily prune timer keeps the newest 500 runs of each
schedule.

**Audit.** `schedule.create`, `schedule.update` (`enable` and `disable`
too), `schedule.delete`, and `schedule.run` (`{id, run, trigger, status,
summary, run_as}`), with the actor of the request, or `run_as` for a
timed run. The actions write their own rows as well: `agent.send`,
`agent.start`, `target.exec`, and so on, with the `run_as` actor.

**Fleet and definition files.** `[[projects.schedules]]` in a fleet file
(fleet.md §1) and `[[schedules]]` in `projects/<name>.toml` take the
fields `name`, `spec`, `action`, `enabled`, and `catch_up`. A fleet
schedule has the key `project/name` and the `source` `fleet:<name>`; an
apply makes or updates it, and `--prune` deletes one that the file drops.
A file schedule runs as `operator`.

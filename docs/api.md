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
[18 Fleet](#18-fleet) · [19 Schedules](#19-schedules) ·
[20 Event sources](#20-event-sources-and-ingest) ·
[21 Agent templates](#21-agent-templates).

## 1. Transports and callers

The daemon serves one router on two listeners.

| Listener | Address | Authentication |
|---|---|---|
| Unix socket | `$TERRARIUM_API_SOCKET` | None. The caller is the operator. |
| TCP | `$TERRARIUM_LISTEN` | `Authorization: Bearer $TERRARIUM_TOKEN` on `/api/*`, a CLI session token in the same header, or the session cookie of `POST /api/session` (§1.1). Without a token the TCP listener stays off. |
| TCP, no auth | `$TERRARIUM_LISTEN` | `GET /install/host.sh`, `GET /install/terrarium-host`, and `GET /install/terrarium-host.sha256`: the host installer (§17.9). They hold no secret. |
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

### 1.1 Operator login and sessions

The operator logs in with the **operator password** or with the static
token (`TERRARIUM_TOKEN`). A login gives a **session token**: a JWT in an
`HttpOnly` cookie for the dashboard, or in the answer body for the CLI
(`terrarium login`). So the dashboard keeps no credential that a script can
read (`docs/security-review.md`, F3 and F15), and the operator does not
paste the token again and again.

**The password.** The daemon keeps only an Argon2id hash (a PHC string, the
default parameters of the `argon2` crate: m = 19 MiB, t = 2, p = 1) and the
password version in `<state>/operator.password` (JSON, mode 0600). The
daemon never logs the password and never returns it. A password has 12
characters or more. Each write takes a `flock` on
`<state>/operator.password.lock` (the daemon and `terrariumd password set`
can write at the same time), writes a unique temp file, fsyncs it, renames
it over the file, and fsyncs the dir. A read that finds a looser mode sets
0600 again, with a warning. Set the password in one of these ways:

- `terrariumd password set` (asks twice on the terminal, with no echo;
  `--stdin` reads the first line of stdin). It needs no daemon: it writes
  the file in `--state-dir`. It refuses to run as another user than the
  owner of the state dir (the daemon could then not read the file): run it
  as the daemon's user, for example
  `docker compose exec terrarium terrariumd password set`. A running daemon
  reads the new file at the next request (it compares the inode, size, and
  mtime).
- `PUT /api/auth/password` (`terrarium admin password set`, or the
  dashboard Settings page).
- `TERRARIUM_PASSWORD` (or `TERRARIUM_PASSWORD_FILE`, its first line) at the
  start, only when no password is set. The daemon logs a warning: remove the
  variable from the env after the first start. The daemon reads the value
  from the env only (there is no flag, so it is never in the process
  arguments), then starts again without both variables, so no child
  process (a harness, git, npm, a login command) inherits them. The login
  commands and npm also strip them.

A new password adds one to the password version, so every session ends.

| Endpoint | Auth | Answer |
|---|---|---|
| `POST /api/session` | `{"password": "..."}` and no `Authorization`; or `Authorization: Bearer <TERRARIUM_TOKEN>` | `201 {"kind", "csrf", "exp", "expires_at"}` and `Set-Cookie`. With `"client": "cli"` in the body: `201 {"kind": "cli", "token", "exp", "expires_at"}` and no cookie. |
| `GET /api/session` | any | `200 {"kind", "csrf"?, "exp"?, "expires_at"?, "password_set"}`. `kind` is `browser` (the cookie, with its `csrf`), `cli`, `token` (the static token), or `local` (the Unix socket). |
| `DELETE /api/session` | none; see below | `204` and a `Set-Cookie` that clears the cookie |
| `DELETE /api/sessions` | the operator (CSRF with the cookie) | `204`. Log out everywhere: the password version goes up by one, so every browser and CLI session ends. The static token stays good. |
| `PUT /api/auth/password` | the operator (CSRF with the cookie) | `{"current"?, "new"}`. `204`. See below. |

**The password login.** `POST /api/session` with `{"password"}` and no
`Authorization`:

| Case | Answer |
|---|---|
| The right password | `201`, as above. An `auth.login` audit row with `ok: true` and the client IP. |
| A wrong password | `401 unauthorized`, `"login failed"`. The same text for every wrong password. The audit log gets one `auth.login_failed` row per address per 10 min (actor `anonymous`, `detail`: `ip`, `last_ip`, `count`, `window_secs`); each more failure in the window adds one to `count`. A lockout that starts writes an `auth.lockout` row. Never the password. |
| No password set | `409 conflict`, `"no password set; run terrariumd password set"`. Not a failure. |
| Too many failures, or too many checks in flight | `429 rate_limited`, `Retry-After: <seconds>`, and `details.retry_after`. No Argon2 check runs. The right password gets `429` too until the wait ends. |
| The password changed while the check ran | `409 conflict`. No token: log in again. |
| No password and no `Authorization` | `401 unauthorized`. |
| A CLI token or a cookie (not the static token) | `401 unauthorized`: a session cannot buy another session. |

**The login limit.** The daemon counts the password checks per client
address, in memory (a restart clears it). The address is the peer address
of the TCP connection; an IPv6 address counts as its /64. The daemon trusts
no `X-Forwarded-For`, so behind a reverse proxy every client shares the
proxy's address and its limit.

- A check takes a slot **before** the Argon2 check, and the checks in
  flight count against the budget. One address may have at most 2 checks
  in flight, and never more than the free failures it has left (one after a
  lockout ends). So a burst of parallel requests cannot pass the limit: the
  rest get `429` at once.
- Five failures are free. The fifth starts a lockout of 30 s; each more
  failure doubles it, up to 15 min. A good password clears the count; an
  hour with no failure starts again from zero.
- At most 16 checks are in flight from all addresses together, and at most
  2 Argon2 checks run at one time. Past 16, a login gets `429` at once, so
  one host cannot fill a queue in front of the operator.
- 100 failures from all addresses in 10 min lock the password login for
  every address until the window ends (`auth.lockout` with `scope:
  global`). The static token still works.
- The guard tracks at most 10,000 addresses, and prunes a full map at most
  once a minute. A new address past the bound gets no entry of its own: the
  lockout of another address never reaches it, and the global budget still
  counts it.
- After the wait for an Argon2 slot, the check looks at the lockouts again.
- The wrong `current` of `PUT /api/auth/password` counts too.

**The session token.** A compact JWT, HS256 only, with a fixed header:

```
header  {"alg":"HS256","typ":"JWT"}
claims  {"sub":"operator","iat","exp","jti","sid","pv","kind","auth_time"}
key     HMAC-SHA256(<state>/session.key, "terrarium-jwt.v2")
csrf    base64url(HMAC-SHA256(<state>/session.key, "terrarium-csrf.v2.<sid>"))
```

- `jti` is 16 random bytes, new for each token. `sid` names the session: the
  `jti` of its first token. The CSRF value is an HMAC of `sid`. `pv` is the
  password version at the login. `kind` is `browser` or `cli`.
  `auth_time` is the time of the login; a renewal copies it.
- The daemon mints a token only for the password version that it checked:
  the version of the hash that matched the password, or the `pv` of the
  token it renews. When the version changed in between, there is no token.
- The check is strict: three parts; a header with `alg` `HS256` (and `typ`
  `JWT`, if any) and no other field, so `none`, `HS512`, and every other
  algorithm fail; the signature, in constant time; `sub` `operator`;
  `auth_time` not after `iat`; `exp` after now; `pv` equal to the current
  password version; for a browser token, `auth_time` not older than
  `TERRARIUM_SESSION_MAX_AGE`; and neither `jti` nor `sid` in
  `revoked_sessions`.
- A `browser` token works only as the cookie. A `cli` token works only as
  `Authorization: Bearer <token>` (or the WebSocket subprotocol
  `bearer.<token>`, not `?token=`) and needs no CSRF header.
- `<state>/session.key` is 32 random bytes as hex, mode 0600. The daemon
  makes it at the first start. A new key ends every session.
- The `v1.` cookie of older daemons is not a token: it gets `401`, and the
  dashboard shows the login screen once.

```
Set-Cookie: terrarium_session=<jwt>; Path=/; Max-Age=<seconds left>; HttpOnly; SameSite=Strict[; Secure]
```

- The cookie is persistent (`Max-Age`), so it stays through a browser
  restart. `Secure` is set when the request is HTTPS or a proxy sends
  `X-Forwarded-Proto: https`.
- The life of a browser token is `TERRARIUM_SESSION_TTL`
  (`--session-ttl`), default `30d`. The life of a CLI token is
  `TERRARIUM_CLI_SESSION_TTL` (`--cli-session-ttl`), default `90d`. Each is
  seconds, or a number with `s`, `m`, `h`, or `d`. `exp` is a Unix time;
  `expires_at` is the same time in RFC 3339.
- **Sliding renewal.** A request with a valid cookie past half its life gets
  a new cookie on its answer: a new `jti` and `exp`, the same `sid`. So the
  CSRF value does not change, and requests in flight with the old cookie and
  the new one both pass the CSRF check. The old token stays valid until its
  own `exp`. `GET /api/session` shows the renewed `exp`. A WebSocket upgrade
  and an answer that sets its own cookie get no renewal. A CLI token is not
  renewed: run `terrarium login` again.
- **The absolute cap.** A renewal never sets `exp` past `auth_time` +
  `TERRARIUM_SESSION_MAX_AGE` (`--session-max-age`, default `90d`), and a
  browser token older than that gets `401`. So a stolen cookie that is in
  use cannot live for ever. A CLI token's cap is its own life.
- The server keeps no session rows, only the sessions that a logout ended
  (`revoked_sessions`, keyed by `sid`), until the last token of the session
  can end.

**`DELETE /api/session`** does not use the auth of the other routes. It
looks at the cookie and the bearer token itself, so that a logout from
another site (logout CSRF) fails:

| Request | Answer |
|---|---|
| A valid CLI token in `Authorization: Bearer` | `204`. The session is revoked. |
| No `terrarium_session` cookie, with or without the static token | `204` and the clearing `Set-Cookie`. A client that lost its cookie can log out again. |
| A valid session cookie and the right `X-CSRF-Token` | `204` and the clearing `Set-Cookie`. The session (its `sid`, so each renewed cookie too) is revoked. |
| A valid session cookie, and no `X-CSRF-Token` or a wrong one (the token of another session too) | `403 forbidden`, no `Set-Cookie`. The cookie stays. |
| A cookie that is wrong, from another key, revoked, or past `exp` | `204` and the clearing `Set-Cookie`. It is no session, so to clear it ends nothing. |

**`PUT /api/auth/password`** takes `{"current"?, "new"}`. The operator
only (an agent gets `403`).

- With the static token or on the Unix socket, `current` is not needed. If
  it is sent and a password is set, it must match.
- With a session (the cookie or a CLI token), `current` must match when a
  password is set. A missing or wrong `current` is `403 forbidden` (not
  `401`: the session is still good), and the login limit counts a wrong one.
- `new` with fewer than 12 characters: `422 invalid`.
- The password version goes up by one, so every session ends, also the
  caller's. With the cookie, the answer clears it. An `auth.password_set`
  audit row records the change (never the password).

The answers of `POST`, `GET`, and `DELETE` carry `Cache-Control:
no-store`. The Unix socket has no `POST /api/session` (`404 not_found`).

The session cookie authenticates every `/api` route and WebSocket upgrade
on TCP, as the token does. Order: `Authorization` (the static token, then a
CLI token), then (on an upgrade) the `bearer.` subprotocol, then the cookie,
then (when on) `?token=` (the static token only). A wrong bearer value is
`401` even with a good cookie. With the cookie:

- `POST`, `PUT`, `PATCH`, and `DELETE` must send `X-CSRF-Token: <csrf>`.
  No header or a wrong one: `403 forbidden`. `GET` needs no header.
- A WebSocket upgrade must send an `Origin` that is the dashboard origin:
  the origin of `TERRARIUM_PUBLIC_URL`, or `http(s)://<Host>` of the
  request itself. No `Origin`, `null`, or another origin: `403 forbidden`.
  The upgrade needs no subprotocol.
- A cookie that is wrong, from another key, revoked, past `exp`, or from an
  older password version: `401 unauthorized`. The dashboard then shows the
  login screen.

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
| `TERRARIUM_SESSION_TTL` | `--session-ttl` | `30d` | The life of a session cookie. The daemon renews it past half its life. |
| `TERRARIUM_CLI_SESSION_TTL` | `--cli-session-ttl` | `90d` | The life of a CLI session token (`terrarium login`). |
| `TERRARIUM_SESSION_MAX_AGE` | `--session-max-age` | `90d` | The longest life of a dashboard session through its renewals, from the login. |
| `TERRARIUM_PASSWORD`, `TERRARIUM_PASSWORD_FILE` | `--password-file` (no flag for the value) | unset | The first operator password, set at the start when none is set (§1.1). The daemon starts again without both variables. Remove them after the first start. |

Every API request has an **actor**:

- `operator`: a TCP caller with the token, a session cookie, or a CLI
  session token, or a Unix-socket caller.
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
| Read agent templates (§21) | own project and global | own project and global | all |
| Create, change, delete agent templates (§21) | no | own project | any, global too |
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

With a tools dir, each row also has these fields:

| Field | Meaning |
|---|---|
| `active` | The active version dir (`tools/current/<name>`): the version that the next agent start runs. |
| `versions` | The complete version dirs on disk, oldest first. |
| `agents` | The live agents on the harness: `[{"agent": "project/name", "id", "version"}]`. `version` is the version dir that the agent's launch resolved; `null` when it is not known (an agent that started before the version dirs). Absent when no agent runs. |

An agent whose `version` differs from `active` keeps its version until
its next start.

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
on the Docker host), with one npm prefix for each version:
`versions/<name>/<version>/`. An update runs
`npm install -g <package>@<version>` into a new version dir, in the
running container, checks `--version`, and switches `bin/<binary>` and
`current/<name>` to it with an atomic `rename`. It needs no image build,
no restart, and no host agent
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
 "output":"\nadded 2 packages in 2s\n","tested":">=0.164.0, <0.165.0","in_range":false,
 "reused":false,
 "agents_on_old_version":[{"agent":"terra/worker","id":"01K8...","version":"0.164.0"}],
 "note":"1 live agent(s) run another version of codex: terra/worker. Each keeps its version and moves to 0.165.0 at its next restart",
 "removed":["0.163.0"]}
```

| Field | Meaning |
|---|---|
| `from` | The version before; `null` when the package was not installed. |
| `to` | The version after: the `package.json` version of the new version dir. |
| `output` | npm's output: stdout, then stderr. |
| `tested`, `in_range` | The adapter's range, and whether `to` is in it (`null` for the extension). Outside the range, an agent start follows `TERRARIUM_HARNESS_VERSION_POLICY` (§6.2). |
| `reused` | `true` when the version dir of `to` was complete on disk, so npm did not run (for example a switch back). |
| `agents_on_old_version` | The live agents on the harness that run another version than `to`, each with the version it runs (`null`: not known). Empty for the extension. |
| `note` | Only with `agents_on_old_version`: one line that names them and says that each moves to `to` at its next restart. |
| `removed` | Only when the GC after the update removed a version dir of this pin: those versions. |
| `error` | `POST /api/harnesses/update` only, on a failed item: why. |

Rules:

- A live agent never stops an update. The launch of each agent runs the
  harness from the version dir that was active at that launch, with that
  `bin/` first on `PATH`. The agent keeps it until its next start (a
  restart, or a relaunch after an exit). `GET /api/agents/{id}` shows it as
  `harness_version`.
- `force: true` installs the version again, also when its version dir is
  complete on disk. Without it, a complete version dir is used as it is.
  The field is accepted for compatibility; no live agent needs it.
- After a done update, the daemon removes each version dir of the tools
  dir that is not active and that no live agent runs. It also removes the
  flat layout of before the version dirs (`tools/lib`) when no live agent
  started on it. The daemon start does the same.
- One update runs at a time. A second one gets `409 conflict`.
- A name that is not a pin, or a version that is not `latest` or semver,
  is `422 invalid`. Nothing reaches npm.
- When npm fails (for example a version that does not exist), the answer
  is `422 invalid` with `details`: `{output, package, version, from}`.
  Nothing changes: the daemon removes the temp dir, and the active version
  and the links stay. npm has 10 minutes; then the daemon kills it.
- `POST /api/harnesses/update` goes on after a failed item. When any item
  failed, the answer is `422 invalid` with `details.results`.
- A Copilot update also reinstalls the Copilot prompt extension into the
  Copilot home.
- Each update writes the audit entry `harness.update` (`detail`: `package`,
  `version`, `from`, `to`, `force`, `reused`, `ok`, `error`,
  `agents_on_old_version`). A done update also writes the event `harness`
  (§9; `data.agents_on_old_version` has the names) and runs
  `<binary> --version` again, so `GET /api/harnesses` shows the new version
  at once.
- A daemon with no tools dir (`TERRARIUM_TOOLS_DIR` empty) answers
  `501 not_available`.
- The tools dir keeps the manifest `terrarium-manifest.json`:
  `{"<name>": {"package", "version", "installed_at", "versions"}}`.
  `version` is the active version; `versions` lists the version dirs on
  disk.

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
| `actor` | `operator`, `system`, `anonymous` (a failed login), or `agent:<ulid>`. |
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
`target.snapshot_delete`, `target.reconcile` (actor `system`),
`target.grant`, `target.revoke`, `target.lease_extend`, and
`target.lease_expired` (actor `system`) (§17). The login (§1.1):
`auth.login` (a good login; `detail`: `ok`, `ip`, `method` (`password` or
`token`), and `kind`), `auth.login_failed` (actor `anonymous`; one row per
address per 10 min with a `count`), `auth.lockout` (`scope`: `address` or
`global`, and `seconds`), `auth.password_set`, and `auth.logout_all`.

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
`kind` is `token`, `ssh_key`, or `password`. A token is one line. A token's
`username` defaults to `x-access-token`. An `ssh_key` is a private key in
OpenSSH or PEM format, and has no `username`. A `password` (1 to 1024 bytes,
one line, spaces kept) is for a `vnc` or `rdp` target, with an optional
default `username` (§17.10); a repo cannot use it. A `POST` with an
existing name replaces the credential.

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
| `system_prompt` | The agent's base system prompt (below). Absent when not set. |
| `roles` | Roles above `member`: `project_admin`, `sysadmin`. |
| `status` | `starting`, `idle`, `waiting`, `working`, `blocked`, `error`, `stopped`, `restarting`. `waiting`: the turn ended, and the agent has one or more open waits (§6.7). `idle`: the turn ended, and the agent has no open wait. |
| `waits` | The number of open waits (§6.7). Absent when it is 0. |
| `waiting_on` | What the agent waits on, in one short line: the summaries of the oldest 3 open waits and a count of the others. Absent when the agent has no open wait. |
| `session` | The harness session ID of the current session, when known. |
| `harness_version` | The harness version that the live agent runs: the version dir in the tools dir that its launch resolved (§4, `POST /api/harnesses/{name}/update`). It stays until the next start, also after an update. Absent when the agent does not run, or when the version is not known. |
| `current_card` | The card the agent has in `in_progress`, if any. |
| `source` | `file`, `api`, or `fleet:<name>` (§18.1). |
| `template` | The agent template that the create copied: `{"id", "name", "project", "revision"}` (§21). Absent when the create named none. For display only. |

### 6.1 CRUD

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/agents?project=` | | `200` agents by ID | read; an agent defaults to its project |
| `POST /api/agents` | an agent definition, or `template` and the fields that differ (§21.3) | `201` agent | project_admin of `project`; some `policy` entries need sysadmin (below) |
| `GET /api/agents/{id}` | | `200` agent, with `subscription_detail` (below) | read |
| `PATCH /api/agents/{id}` | `{"harness"?, "subscription"?, "model"?, "prompt"?, "system_prompt"?, "policy"?, "restart"?, "new_session_on_prompt_change"?}` | `200` agent | project_admin; some `policy` entries need sysadmin (below) |
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

`system_prompt` (optional, at most 32 KiB, else `422 invalid`): the
agent's base system prompt (PLAN §8). It can use the variables of the
prompt templates (`prompts/README.md`). An empty text is the same as no
text.

- Not set: the harness keeps its own default system prompt, and the
  rendered `prompt` template is appended to it.
- Set: the rendered `system_prompt`, a blank line, and the rendered
  `prompt` template replace the harness's default system prompt. The
  Terrarium template always stays.

A change applies at the next start or restart. The prompt hash covers the
whole text, so with `new_session_on_prompt_change` a changed
`system_prompt` starts a new Codex session. The dashboard and the CLI
show the text as plain text. The audit row of a `PATCH` says `set` or
`cleared`, not the text.

A harness that takes a changed prompt on a resume (Claude Code with
`--system-prompt-snapshot off`, pi) resumes with the new prompt. The daemon
then writes the new hash to `private/session-prompt.sha256` and records a
`system` event with the message `prompt updated on resume`.

`GET /api/agents/{id}` (not the list) adds `subscription_detail` when the
agent's subscription exists:

```json
"subscription_detail":{"name":"zai-key","provider":"zai","plan":"coding","login_status":"api_key",
  "harnesses":["claude","pi"],"default_model":"glm-5.3-flash"}
```

`GET /api/agents/{id}` also adds `open_waits`: the open waits of the
agent, oldest first, in the shape of §6.7.

`default_model` is the model that the agent's harness gets on the
subscription when the agent sets no `model` (`default_models` in §16.1).
It is absent when the harness keeps its own default. The field has no
secret and no quota numbers (those are in `GET /api/usage/me`).

`PATCH` changes only the fields in the body. Send `null` for
`subscription`, `model`, `prompt`, or `system_prompt` to clear it
(`{"subscription": null}`). An empty `system_prompt` also clears it.
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
agent is `idle` or `waiting`, else after the next stop hook. One pointer covers all the
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

**Screen snapshot.** `GET /api/agents/{id}/screen` gives the harness screen
as plain text. The rights are the rights of the PTY: the project_admin of
the agent's project, a sysadmin, or the operator. A member gets
`403 forbidden`. The call is read-only. The daemon asks the runner for its
parsed screen (the control call `screen`, [runner.md](runner.md)). It does
not attach to the PTY, and it types nothing.

```json
{"agent":"terrarium/worker","agent_id":"01M5PA0T0Z0K16V6G88P3SE78W","status":"blocked",
 "rows":40,"cols":120,"text":"Do you want to make this edit to lib.rs?\n> 1. Yes\n  2. No",
 "cursor":{"row":1,"col":2},"at":"2026-10-27T10:16:30.120Z"}
```

| Field | Meaning |
|---|---|
| `agent` | `project/name`. |
| `status` | The live status of the agent. |
| `rows`, `cols` | The size of the screen. |
| `text` | The rows, joined with `\n`. There are no escape codes. The daemon removes the trailing blanks of each row and the blank rows at the end. |
| `cursor` | The cursor position, from 0. |
| `at` | RFC 3339: when the daemon took the snapshot. |

| Query | Meaning |
|---|---|
| `ansi` | `true`: `text` is the escape sequences that draw the screen on an empty terminal. Default `false`. |

An agent that is not running gives `409 not_running`. A runner that does
not have the control call `screen` gives `409 conflict` with the message
"runner too old; restart the agent". The runners of this version all have
it.

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

### 6.7 Waits, runs, and watches

PLAN §12d gives the design. The code is in
`crates/terrariumd/src/store/waits.rs` (the rows and the rules) and
`crates/terrariumd/src/waits.rs` (the timer, the wake mail, the runs, and
the watch dispatch).

A **wait** is one thing that an agent (the waiter) waits on. No call
blocks: the call registers the wait, and the agent ends its turn. When the
wait ends, the waiter gets mail from `system`, and the mail wakes it.

```json
{"id":"01JD...","agent":"01M5...","agent_name":"demo/lead","project":"demo",
 "kind":"agent","spec":{"agent":"01M6...","agent_name":"demo/w1","until":"done","from_status":"working"},
 "summary":"agent demo/w1 (done)","state":"open","reason":"review TER-3 after it",
 "created_by":"agent:01M5...","created_at":"2026-10-27T10:00:00.000Z",
 "timeout_at":"2026-10-28T10:00:00.000Z"}
```

| Field | Meaning |
|---|---|
| `kind` | `agent`, `process`, `time`, or `card`. |
| `spec` | `agent`: `{agent, agent_name, until, from_status}`. `process`: `{target, session, name?, command?, run}`; `run` is true when the daemon started the command (`POST .../run`) and reads its output. `time`: `{due_at}`. `card`: `{card, column}`. |
| `state` | `open`, `resolved`, `timed_out`, `cancelled`, or `failed`. Only `open` changes. |
| `reason` | The waiter's note, at most 500 characters. It comes back in the mail. |
| `result` | After the end. `agent`: `{outcome, status}`. `process`: `{exit_code, signal, reason, duration_secs, stdout_tail, stderr_tail}`, or `{error, message}` for a run that the daemon lost. `time`: `{outcome: "due"}`. `card`: `{outcome: "reached", card, column}`. A timeout: `{outcome: "timed_out"}`. A cancel: `{outcome: "cancelled", by}`. |
| `resolved_at`, `notified_at` | When the wait ended, and when its mail went out. |

The kinds:

| Kind | Resolves when |
|---|---|
| `agent` | `until` is `done` (the default): the agent is `idle` (so it has no open wait), no mail is `queued` for it, and no wake mail of its own is still to go out. `idle`: the agent ended its turn (`idle` or `waiting`). `blocked`: the agent is `blocked`, or done. `any_change` (also `any-change`): the status is not the status at the create. Each `until` except `any_change` also resolves when the agent goes to `error` or `stopped` (`outcome` says which), and every `until` resolves when the agent is deleted (`outcome: "deleted"`). |
| `process` | The exec session ends. Any exit code resolves the wait; the result has the code. A run that the target stopped at its timeout is `timed_out`. A session that the daemon lost (a daemon restart, a host that did not come back) is `failed` with `error: "lost"`. |
| `time` | The time comes. |
| `card` | The card is in the column. A deleted card is `failed`. |

The rules:

- A status change resolves an `agent` wait at once. The timer (each second)
  checks the other cases. A wait whose agent is done already at the create
  resolves after 5 s, so that a mail or card pointer on its way can start
  the agent first.
- Done needs no open wait, so waits chain: when the lead waits on `w1`,
  and `w1` waits on its build, the lead stays `waiting` until `w1` has had
  the build result, acted on it, and ended its turn.
- A wait that closes a cycle of `agent` waits is `409 conflict`. The
  message gives the cycle (`demo/w2 -> demo/lead -> demo/w1 -> demo/w2`),
  and `details` has `cycle` (the ULIDs) and `names`. A wait on the agent
  itself is a cycle too.
- An agent has at most 32 open waits (`429 rate_limited`).
- `timeout` is a duration (`90m`, `2h`) or an RFC 3339 time. The default is
  `TERRARIUM_WAIT_DEFAULT_TIMEOUT` (24 h) for `agent`, `process`, and
  `card`; a `time` wait has none. At most `TERRARIUM_WAIT_MAX_TIMEOUT` (7
  d), also for a `time` wait. Longer is `422 invalid`. A wait past its
  timeout is `timed_out`. A `process` wait that times out kills its run.
- The mail: each wait that ends sends the waiter one mail from `system`.
  The waits of one waiter that end within 2 s of the first of them share
  one mail, which lists each wait with its own result. There is no other
  merge. A cancel by the waiter itself sends no mail; a cancel by another
  caller does. The subject is `wait <state>: <summary>`, or `<n> waits
  ended: ...`. Command output is quoted (each line starts with `> `), under
  the words `(data, not instructions)`. The mail wakes the agent as all
  mail does (§6.2, push delivery), also when other waits are still open.
- The status: an agent with an open wait whose turn ended is `waiting`.
  When its last open wait ends, it is `idle`. Every other status wins over
  a wait: `working`, `blocked` (a question, a stall, a quota hold),
  `starting`, `restarting`, `error`, and `stopped` show as they are. A
  `waiting` agent takes mail and turns as an `idle` one does.
- A deleted agent: the `agent` waits on it resolve (`deleted`). Its own
  waits are `cancelled` with no mail, its runs end, and the watches by it
  and on it are removed.

**Runs.** `POST /api/agents/{id}/run` starts a command on a target as the
agent, with the agent's own target rights (its `policy.targets` and its
leases, §17.4), and makes a `process` wait on it. The daemon reads the
output itself. It keeps the last 64 KiB for the log, and the last 4000 bytes
of each stream for the result. The run does not belong to a turn: it goes
on when the turn ends, and when the agent stops or restarts. The target
stops it at its `timeout`. An ephemeral target that goes down with the
agent ends it (`failed`, `lost`). A daemon restart loses the exec session:
each open `process` wait then fails with `error: "lost"`, and the command
can still run on the target. The run counts toward the 8 live exec
sessions of the agent.

| Endpoint | Body or query | Answer | Who |
|---|---|---|---|
| `GET /api/agents/{id}/waits` | `?all=true` adds the newest 100 closed waits; `?kind=` | `200` open waits, newest first, then the closed ones | the agent, a project_admin of its project, the operator |
| `POST /api/agents/{id}/waits` | below | `201` the wait | the agent itself, the operator |
| `POST /api/agents/{id}/run` | `{"target", "cmd": [..], "name"?, "cwd"?, "env"?, "timeout"?, "reason"?, "agent"?}` | `201 {"wait", "session_id", "target"}` | the agent itself, the operator; the run uses the agent's rights |
| `GET /api/waits/{id}` | | `200` the wait | as for the list; else `404` |
| `DELETE /api/waits/{id}` | `?kill=true` also ends the run of a `process` wait | `200` the cancelled wait; a wait that is not open is `409 conflict` | as for the list |
| `GET /api/waits/{id}/log` | `?max=` bytes (at most 65536) | `200 {"wait_id", "session_id", "target", "running", "output", "end"?}` | as for the list; a wait that is not `process` is `422` |

The create body:

```json
{"kind":"agent","agent":"w1","until":"done","timeout":"2h","reason":"merge after it"}
{"kind":"time","for":"30m"}
{"kind":"time","at":"2026-10-27T15:00:00Z"}
{"kind":"process","session":"01JC..."}
{"kind":"card","card":"TER-3","column":"done"}
```

- `agent` takes a ULID, `project/name`, or a name. An agent may wait on the
  agents of its own project. Another project needs a `cross_project` entry
  (`project/name` or `project/*`), as mail does (§7.1). A sysadmin and the
  operator reach every agent.
- `process` takes an exec session that the waiter opened (or, for the
  operator, a session of the operator or the waiter). The daemon does not
  read its output; whoever opened it reads it. The result has no tail.
- `card`: the column must be a column of the card's board (`422`). The
  waiter must be able to read the card's project.
- Each create writes a `wait.create` audit row; each cancel writes
  `wait.cancel`.

A **watch** mails its watcher when the watched agent goes to one of its
statuses. The watcher is an agent or `operator` (the mail log of the
dashboard).

```json
{"id":"01JE...","watcher":"01M5...","watcher_name":"demo/lead","agent":"01M6...",
 "agent_name":"demo/w1","project":"demo","on":["blocked","error"],"once":false,
 "debounce":true,"created_by":"agent:01M5...","created_at":"...","fired":2,
 "last_fired_at":"..."}
```

| Endpoint | Body or query | Answer | Who |
|---|---|---|---|
| `GET /api/agents/{id}/watches` | | `200` the watches on the agent and by it | the agent, a project_admin of its project, the operator |
| `POST /api/agents/{id}/watches` | `{"on": ["blocked", "error"], "once"?, "debounce"?, "watcher"?}`; `{id}` is the watched agent | `201` the watch | an agent for itself, with the reach rule of waits; the operator for `operator` (the default) or an agent |
| `GET /api/watches` | `?watcher=` (default: the caller, or `operator`) | `200` the watches of the watcher | the watcher, a project_admin of its project, the operator |
| `DELETE /api/watches/{id}` | | `204` | the watcher, a project_admin of its project, the operator |

- `on` takes `idle`, `waiting`, `working` (also `busy`), `blocked`, `error`,
  `stopped`, `starting`, and `restarting`, as a list or as one
  comma-separated string. A watcher has at most 32 watches.
- With `debounce` (the default), a change waits 5 s (10 s for `idle`,
  `waiting`, and `working`). Then the daemon mails only when the agent is
  still in one of the statuses. Changes in the window go out as one mail,
  so a flap sends nothing. `debounce: false` mails each change at once.
- `once` removes the watch after its first mail.
- The mail is from `system`: subject `<project/name> is now <status>`, and
  the body gives the reason of the change, quoted.
- The queued changes of a debounce live in memory: a daemon restart drops
  them. The watches stay.
- Each create writes a `watch.create` audit row; each remove writes
  `watch.delete`.

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
| `actor` | `operator`, `system`, or `agent:<ulid>`. An event of an event source has `source:<project>/<name>` (§20). |
| `agent` | The agent **name**. `agent_id` and `data.agent_id` carry the ULID. Absent for project events. |
| `project`, `card`, `session` | Absent when they do not apply. |
| `summary` | A short line for lists. |
| `data` | The kind's payload. |

Kinds:

| `kind` | `data` |
|---|---|
| `status` | `{status, from, reason, waits, waiting_on?}`. `from` is never the same as `status`: a runner report of the status the agent already has (for example the runner's `spawned <program>` at start) goes in as a `system` event `{message, status}`. `waits` is the number of open waits; `waiting_on` is the line of §6.7. |
| `wait` | `{wait_id, change, kind, summary, state, result?}`. `change` is `opened`, `resolved`, `timed_out`, `cancelled`, or `failed`. The `agent` is the waiter (§6.7). |
| `watch` | `{watch_id, change, watcher, watcher_name, on, status?}`. `change` is `created`, `fired` (with `status`), or `removed`. The `agent` is the watched agent (§6.7). |
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
| `target` | `{change, target, ...}`. `change` is `reconciled` (`from`, `state`, `snapshots_cleared`): a `status` found the target other than the store said. `snapshots_dropped` (`snapshots_dropped`): a snapshot list found that the host lost stored names. `snapshot_deleted` (`snapshot`, `scope`, `machines`, `agent`?): a snapshot delete (§17.3). |
| `schedule` | `{change, schedule, name, project, run?, status?, trigger?}`. `change` is `created`, `updated`, `deleted`, `enabled`, `disabled`, `run_started`, or `run_finished`. `schedule` is the schedule ULID. A run event has `run`, `status`, and `trigger` (§19). |
| `harness` | `{change, harness, package, from, to, forced}`. `change` is `updated`: an update in the tools dir (§4, `POST /api/harnesses/{name}/update`). |
| `host.connected`, `host.disconnected` | `{host, host_id, ...}`: `version`, `os`, `arch`, `capabilities`, `summary`, `resumed` (sessions taken back), `reason`. See §17. |
| `event_source` | `{change, source, name, key, kind, enabled}` after a change of an event source (§20). |
| `ci.run.*`, `ci.job.failed`, `git.push`, `pr.*`, `ext.*` | A webhook of an event source (§20.4), or `POST /api/events` (§20.5). The actor is `source:<project>/<name>`, or the caller of `POST /api/events`. |

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
 "local_estimate_only":false,"has_api_key":false,"held_agents":0,"default_models":{}}
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
| `default_models` | The model that each harness gets on this subscription when the agent sets no `model`, by harness name, for example `{"pi":"glm-5.3-flash","claude":"glm-5.3"}` for a Z.ai key. Only the harnesses where the daemon picks the model: pi (the provider's default from the pi adapter's table; no entry when pi refuses the subscription) and Claude Code on Z.ai. `{}` when every harness keeps its own default. Always present. |

Names follow the project name rules; `login` and `detect` are reserved.

Probes: `anthropic-oauth-usage`, `openai-codex-usage` (alias
`codex-rate-limits`), `github-copilot-quota` (alias
`copilot-premium-requests`), `zai-usage` (the default of a `zai`
subscription on the `coding` plan that names no probe), and
`openrouter-credits` (the default of an `openrouter` subscription that names
no probe). `docs/compat.md`, "Usage probes", records each endpoint.

A window of `openrouter-credits` is `credits`: the share of the bought
credits that is used, with no reset time. Its window in a list has
`balance: {total, used, remaining, currency}` (US dollars). The dashboard
shows "$10.65 left" and "$49.35 used of $60.00" in place of the percentage. At 100 % the account has no money left, and
the quota policy acts as for a full window.

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
- `GET /api/subscriptions/{name}/models`: the operator, a `sysadmin`, or a
  `project_admin` of a project with an agent on the subscription.
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
| `GET /api/subscriptions/{name}/models` | `?refresh=true` | The models the subscription can use (below). Cached for 6 h per subscription; `refresh` reads the list again. |
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

`GET /api/subscriptions/{name}/models`:

```json
{"models":[{"id":"glm-4.7","context":204800},{"id":"glm-5.3-flash","context":1000000}],
 "source":"pi","fetched_at":"2026-10-26T09:00:00.000Z",
 "notes":["GET https://api.z.ai/api/coding/paas/v4/models: status 500"]}
```

The daemon tries four sources, in order, and answers with the first list
that has a model:

1. `provider`: `GET <base>/models` (the OpenAI list shape) with the stored
   API key as a bearer token, for an API-key subscription of `zai` (the
   plan's base URL: `coding` `https://api.z.ai/api/coding/paas/v4`, `api`
   `https://api.z.ai/api/paas/v4`), `openai-compatible` (the stored
   `base_url`), `openai`, or `openrouter`. The key stays in the daemon.
2. `plan`: the model list of the harness login, with the credential of the
   shared harness home. The usage probes read the same credentials.
   - `anthropic` with `claude` in `harnesses`, or with a stored Anthropic
     key: Anthropic's `GET /v1/models`, with the OAuth token, or with the
     key in `x-api-key`. The API order stays.
   - `openai` with `codex` in `harnesses` and no stored key: the ChatGPT
     plan's Codex model list. Only the models that Codex shows, by
     priority.
   - `github` with `copilot` in `harnesses`: Copilot's `GET /models`, with
     the stored token or the Copilot login. Only the chat models of the
     model picker that the policy does not disable. The ids keep their
     dots (`claude-sonnet-4.6`), as Copilot's `--model` takes them.

   The token stays in the daemon. `docs/compat.md`, "Model lists (plan
   logins)", records the requests.
3. `pi`: `pi --offline --list-models <pi provider>`, pi's own catalogue. For
   an API key, pi runs in an empty pi dir with a placeholder in the key env
   var (`--offline` sends no request); for a plan login, in the shared pi
   home.
4. `fallback`: the values of `default_models` (§16.1).

`notes` (absent when empty) says why an earlier source gave no list.
`name` and `context` (tokens) are in a model only when the source gives
them. `docs/compat.md`, "Z.ai models", records the live answer.

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
| `POST /api/hosts` | `{"name", "enrol"?}` | `201` one flat host object (the shape below) with `token`, `token_kind`, and for an enrol token `token_expires_at` and `install_command` (§17.9) | the operator |
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
- An enrol answer also has `install_command`: the line that installs the
  host agent on the host with this token,
  `curl -fsSL <daemon>/install/host.sh | sudo sh -s -- --token <token> --name <name>`.
  `<daemon>` is `TERRARIUM_PUBLIC_URL`, unless that names a loopback host;
  then it is `http://` and the `Host` of the request. With neither (the
  Unix socket and no public URL), the field is absent.
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
| `snapshots` | The snapshot names that `POST .../snapshot` or `snapshot_before_task` took. A destroy (the state `absent`) clears them. A snapshot delete removes a name. A snapshot list removes the names that the host no longer has (§17.3, "Snapshots"). |
| `host_online` | The link of the target's host agent. `null` for `ssh`. |
| `usable` | The caller may use the target (§17.4). |
| `access` | Why the caller may use it: `operator`, `policy`, `lease`, or `null`. |
| `lease` | The caller's active lease on the target (§17.7), when `access` is `lease`; else `null`. |
| `has_display` | The target has a screen (§17.10): `vnc`, `rdp`, `desktop`, `vmlab` with a `machine` (or a lab instance kind), and a `docker` target with `display`. |
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
| `docker` | `host`, and `image` or `container` | `ephemeral`, `keep`, `display` (`x11`, `wayland`, `auto`), `display_name` (§17.10) |
| `vmlab` | `host`, and one of: `lab` (an absolute path); `lab_repo` with `machine`; `lab_from_workspace = true` with `machine` | `machine`, `lab_ref`, `lab_path`, `ephemeral`, `keep`, `snapshot_before_task` |
| `vnc` | `host` (the host agent that connects), `address` (`host:port`, port default 5900) | `username`, `credential` (a `password` in the credential store) |
| `rdp` | `host`, `address` (port default 3389) | `username`, `domain`, `credential` (a `password`) |
| `desktop` | `host` | `display` (default `auto`), `display_name` |

`vnc`, `rdp`, and `desktop` targets have only a display (§17.10): exec,
shell, cp, checkout, and snapshots are `422 invalid`. `display` and
`display_name` go with `docker` and `desktop` only; `username` and
`domain` with `vnc` and `rdp` only (`domain`: `rdp` only).

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
`machine`, `credential`, `username`, `domain`) must not be empty, must not
hold spaces, and must not start with `-`. A bad value is `422 invalid`.

File sync, at start and on `POST /api/reload`: a file target is made or
updated and becomes `file`. An API target with the same name becomes
`file`. When the file is gone, the target is deleted. The API refuses to
change a file target (`409 file_managed`).

### 17.3 Lifecycle verbs

`POST /api/targets/{name}/up|down|snapshot|restore|status`. The body is
empty, or `{"name"?, "discard_guest_changes"?, "destroy"?, "scope"?,
"agent"?}`. `scope` is `machine` or `lab`, for `snapshot` and `restore`
("Snapshots", below).
`agent` names the holder whose instance of a `lab_from_workspace` target
the verb acts on; an agent caller always acts on its own. The caller
may use the target (§17.4), or may define it (§17.2). The answer is the
executor report, not the target: `{"target", "verb", "state", "snapshot"?,
"result"?}`. `target` is the name, `state` is the new state, `snapshot` and `scope` are
there for `snapshot` and `restore`, and `result` is the result of the
host agent verb when it gives one. The daemon stores the state and writes
a `target.<verb>` audit row with the same report.

| Kind | `up` | `down` | `snapshot` / `restore` | `status` |
|---|---|---|---|---|
| `host`, `ssh` | no-op | no-op | `422 invalid` | `running` when the host agent is online, else `unknown` |
| `vnc`, `rdp` | no-op | no-op | `422 invalid` | a TCP connect from the host agent to `address` (the `probe` frame): `running` when it connects, else `unknown`, with `result: {reachable, error?, elapsed_ms}` |
| `desktop` | no-op | no-op | `422 invalid` | `running` when the host agent is online and its desktop display is on, else `unknown` |
| `docker` | `docker.inspect`; when absent, `docker.run -d --init --name <c> --label terrarium.target=<name> <image> sleep infinity`; when stopped, `docker.start` (an ephemeral target with `image`: `docker.rm -f -v` and a new `docker.run`) | `docker.stop`; an ephemeral target without `keep` (or `destroy: true`): `docker.rm -f -v` | `422 invalid` | `docker.inspect` |
| `vmlab` | `vmlab.up [machine]` | `vmlab.down [machine]`; ephemeral without `keep` (or `destroy: true`): `vmlab.destroy` | `vmlab.snapshot_take` / `vmlab.snapshot_restore`: with `--vm machine` for the scope `machine`, without `--vm` (every machine of the lab) for the scope `lab` | `vmlab.status`, the row of `machine` (no row: `absent`). A lab that is not running (not in `vmlab lab list`): `absent` for an ephemeral target without `keep`, else `stopped` |

The container `<c>` is `container`, else `terrarium-<name>`. The snapshot
name is `name`, else `terrarium`.

**Snapshots.** A vmlab target has snapshots. vmlab keeps each snapshot on
one machine. A lab-wide snapshot is one snapshot with the same name on
each machine.

| Endpoint | Query | Answer |
|---|---|---|
| `GET /api/targets/{name}/snapshots` | `scope`?, `agent`? | `200` the list, below |
| `DELETE /api/targets/{name}/snapshots/{snap}` | `scope`?, `agent`?, `force`? | `200` `{"target", "verb": "snapshot_delete", "snapshot", "scope", "machines", "agent"?}` |

```json
{"target":"win","scope":"machine","machines":["win11"],"dropped":[],
 "snapshots":[{"name":"terrarium-clean","kind":"online","taken":"2026-10-26T08:00:00Z",
               "machines":["win11"],"managed":true}]}
```

Rules:

- The rights are those of the verbs: the caller may use the target
  (§17.4), or may define it (§17.2). Else `403 forbidden`.
- `scope` is `machine` (the target's `machine`) or `lab` (every machine of
  the lab). The default is `machine` when the target has a `machine`, else
  `lab`. The scope `machine` on a target with no `machine` is `422
  invalid`. The same rule applies to the `snapshot` and `restore` verbs.
- `agent`: on a `lab_from_workspace` target, the holder whose instance.
  An agent caller always acts on its own instance, also when it sends
  another `agent`. The operator must name one (`422 invalid`). A lab
  instance target with no instance on the host is `409 conflict`.
- The list runs `vmlab snapshot list <machine>` for each machine. For the
  scope `lab`, the machines come from `vmlab status`. Each row is one name,
  with `kind` and `taken` from the first machine that has it, and
  `machines`: the machines that have it. `managed` is `true` when the name
  is in the target's `snapshots`, so Terrarium took it. A snapshot that
  Terrarium did not take shows with `managed: false`. The list does not
  add it to `snapshots`.
- The list reconciles the stored names: a name in `snapshots` that the
  host does not report goes, and `dropped` holds it. Then the daemon writes
  the audit row `target.reconcile` (`{snapshots_dropped}`, actor `system`)
  and a `target` event. A `lab_from_workspace` target is not reconciled,
  because its stored names cover the instances of all holders.
- For the scope `lab`, the answer has `lab_running`. A lab that does not
  run has no machines in `vmlab status`. Then the list is empty, and it
  drops nothing.
- The delete reads the snapshot list first, then runs `vmlab snapshot
  delete <machine> <snap>` on each machine (of the scope) that has the
  snapshot. When no machine has it, the answer is `404 not_found`. For the
  scope `lab`, a lab that does not run is `409 conflict`. The list comes
  first because vmlab 0.34 answers "deleted" with exit 0 for a snapshot
  that does not exist.
- A delete of `terrarium-clean` on a target with `snapshot_before_task` is
  `409 conflict`, because the daemon restores that snapshot when an agent
  stops. Send `force=true` to delete it.
- The delete removes the name from `snapshots`, and writes the audit row
  `target.snapshot_delete` with the answer and a `target` event.
- A snapshot name must not be empty, start with `-`, or hold a space or a
  control character (`422 invalid`).
- A docker, host, or ssh target is `422 invalid`. An offline host is `409
  conflict`.
- On a lab instance target, the snapshot verbs, the list, and the delete
  run in the lab dir of the instance, as `up` and `exec` do (§17.8). No
  vmlab verb of Terrarium sends `--instance`. So vmlab uses its default
  instance (`$VMLAB_INSTANCE` of the host agent, else the git worktree of
  the lab dir, else none), and it is the same for each verb.

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

### 17.9 The host installer

The daemon serves the installer of the host agent on the dashboard port
(`TERRARIUM_LISTEN`) and on the Unix socket. The routes are outside `/api`,
so they need no token, as the dashboard assets need none. They hold no
secret. The enrol token comes only from `POST /api/hosts` (§17.1). The host
listener and the ingest listener do not serve these routes.

| Endpoint | Answer |
|---|---|
| `GET /install/host.sh` | `200` `text/plain`: `scripts/install-host.sh` with the defaults filled in (below). |
| `GET /install/terrarium-host?arch=<arch>` | `200` `application/octet-stream`: the static musl `terrarium-host` for the arch. |
| `GET /install/terrarium-host.sha256?arch=<arch>` | `200` `text/plain`: `<sha256 hex>  terrarium-host-<arch>-linux`. |

Rules:

- Each answer has the header `X-Terrarium-Version` (the daemon version),
  `Cache-Control: no-store`, and the API CSP (`default-src 'none'`).
- `arch` is the name that the script makes from `uname -m`: `x86_64`
  (also for `amd64`) or `aarch64` (also for `arm64`). Only `x86_64` has a
  build now. Another arch gets `404` with a text message that names the
  arches that the daemon serves. No `arch` gets `400`.
- The binary comes from `TERRARIUM_HOST_DIST_DIR` (default
  `/opt/terrarium/dist`): `terrarium-host-<arch>-linux`, else
  `terrarium-host` when the arch is the daemon's own (the output of
  `just host::build`). No binary gets `404`. The SHA-256 comes from the
  `.sha256` file next to the binary, else the daemon hashes the binary.
- The script gets these defaults. A value with a character outside
  letters, digits, and `:/._-[]%~` is left out:
  - The daemon URL for the downloads: `TERRARIUM_PUBLIC_URL`, unless it
    names a loopback host; then `http://` and the `Host` of the request.
  - The host listener URL (`daemon_url`): `TERRARIUM_HOST_DAEMON_URL`.
    Else, with TLS on the host listener,
    `wss://<host of the daemon URL>:<port of TERRARIUM_HOST_LISTEN>`.
    Else, for a loopback daemon URL, `ws://<host>:<port>`. Else none, and
    the script asks for `--host-url`.
  - The pin: the SHA-256 of `TERRARIUM_HOST_TLS_CERT`, for a `wss://` URL.
  - The systemd unit, `crates/terrarium-host/terrarium-host.service`.
- The script downloads the binary over the same link as the script. The
  SHA-256 finds a broken download; it does not prove where the binary
  came from. Serve the dashboard port over HTTPS (a proxy) when the hosts
  reach it over a network that you do not trust.

### 17.10 Target displays

PLAN §19b. An agent uses the screen of a target: it takes a screenshot,
looks at the PNG, moves and clicks the mouse, and types. The daemon sends
each operation to the target's host agent in one `display` frame
([host-protocol.md "`display`"](host-protocol.md#display)). The code is
`crates/terrariumd/src/display.rs` (the daemon side) and
`crates/terrarium-host/src/display/` (the drivers).

| Kind | The screen | Backend |
|---|---|---|
| `vmlab` | The screen of `machine` | `vmlab vm screenshot`, `mouse-move`, `click`, `drag`, `sendkeys`, `ocr`, `find-image`; `vmlab record` |
| `docker` with `display` | An X11 or Wayland session in the container | `docker exec` of the tools below |
| `desktop` | The desktop of the host agent's machine | the tools below, on the host. Only when the host agent opts in (below) |
| `vnc` | A VNC server at `address` | the host agent's VNC client (RFB). No tool |
| `rdp` | An RDP server at `address` | the host agent's RDP client (IronRDP). No tool |

The tools for `docker` and `desktop`:

| `display` | Screenshot | Pointer | Keys and text |
|---|---|---|---|
| `x11` | `import -window root png:-` (ImageMagick), else `xwd \| convert`, else `scrot` | `xdotool` | `xdotool key`, `xdotool type --file -` |
| `wayland` | `grim -` | `ydotool` (needs `ydotoold` and its socket) | `wtype` |
| `auto` | `x11` when `xdotool` is in the container (or on the host), else `wayland` when `grim` is | | |

`display_name` is the X display (default `:0`) or the Wayland socket
(default `wayland-0`, or an absolute path when the image sets no
`XDG_RUNTIME_DIR`). Wayland input works with wlroots compositors (sway,
cage, labwc). GNOME and KDE on Wayland refuse synthetic input: use a
`vnc` or `rdp` target for them. `examples/display/` has test images for
X11, Wayland, VNC, and RDP.

**VNC and RDP.** The host agent has its own clients, so a `vnc` or `rdp`
target needs no tool on the host. `ops` of both has `scroll`; `ocr` is
`tesseract` on the host agent, when it is there.

| | `vnc` | `rdp` |
|---|---|---|
| Protocol | RFB 3.8 (3.7 and 3.3 also) | RDP, IronRDP |
| Login | VNC authentication (security type 2) with the credential's password, or none (type 1). The VNC protocol has no user name: `username` is not used | NLA (CredSSP, NTLM) with `username`, `domain`, and the password, when the server offers NLA. Else TLS, and the password goes in the RDP login packet |
| Encryption | None. The screen and the keys cross the link in clear text: keep it on a trusted network or in a tunnel. No VeNCrypt or TLS | TLS. The host agent pins the server's certificate on the first connection (below) |
| Screen | A full update for each screenshot: ZRLE, CopyRect, or Raw. The server may change the size (DesktopSize) | The server pushes updates; IronRDP decodes them (RemoteFX, bitmaps). The client asks for 1280 x 800 |
| Keys | X keysyms | set-1 scancodes (US layout) for `keys`; `pause` has none (`422 unsupported`) |
| Text | One keysym for each character: Latin-1 is the code point, other Unicode is `0x01000000` + the code point. The server must map the keysym | Unicode key events; newline and tab are keys |
| Scroll | Buttons 4 and 5 (up, down), 6 and 7 (left, right), one press for each step | Wheel events of 120 units for each step |

The host agent keeps one connection for each target and credential, so
a live view does not log in each time. Operations on a target run one at
a time. A connection with no operation for 60 s closes. When a kept
connection fails (the server closed it), the host agent connects again
and tries the operation once more. A login takes at most 20 s.

**The RDP certificate pin.** An RDP server has a self-signed certificate,
as a rule. On the first connection, the host agent accepts it and writes
its SHA-256 to its pin file (trust on first use): `[display]
rdp_known_hosts`, by default `~/.local/share/terrarium-host/rdp-known-hosts`
of the host agent's user. Each later connection must show the same
certificate, else the answer is `403 forbidden` (`details.code`
`refused`), "the server's TLS certificate changed", and the host agent
sends no credential. When the server has a new certificate on purpose,
run `terrarium-host rdp-forget <address>` on the host agent's machine
(with no address, it lists the pins), then connect again.

**RDP with TLS and no NLA.** xrdp, for example, has no NLA. With TLS
alone, a wrong password does not fail the connection: the server shows
its own "login failed" screen, and a screenshot shows it.

**Endpoints.**

| Endpoint | Body or query | Answer |
|---|---|---|
| `GET /api/targets/{name}/display/info` | `?agent=` | `200` the info, below |
| `POST /api/targets/{name}/display/screenshot` | `{"region"?, "scale"?, "agent"?}`; `?format=json` | `200` `image/png`, or JSON with `png_base64` |
| `POST /api/targets/{name}/display/move` | `{"x", "y"}` | `200` `{"target", "op", "ok", ...}` |
| `POST /api/targets/{name}/display/click` | `{"x"?, "y"?, "button"?, "double"?}` | `200` |
| `POST /api/targets/{name}/display/drag` | `{"x1", "y1", "x2", "y2"}` | `200` |
| `POST /api/targets/{name}/display/scroll` | `{"x"?, "y"?, "dx"?, "dy"?}` | `200` |
| `POST /api/targets/{name}/display/keys` | `{"chord"}` | `200` `{..., "chord"}`, the chord in its canonical spelling |
| `POST /api/targets/{name}/display/type` | `{"text"}` | `200` `{..., "chars"}` |
| `POST /api/targets/{name}/display/ocr` | `{"region"?}` | `200` `{"target", "op", "engine", "text", "region"}` |
| `POST /api/targets/{name}/display/find` | `{"image_base64", "threshold"?, "region"?}` | `200` `{"target", "op", "found", "x"?, "y"?, "w"?, "h"?, "score"?, "center"?}` |
| `POST /api/targets/{name}/display/record` | `{"action": "start"\|"stop"}` | `200` `{"target", "op", "action", "recording", "output"}` |
| `POST /api/targets/{name}/display/info` | `{}` | the same as the `GET` |

Every body also takes `agent`: on a `lab_from_workspace` target, the
holder whose instance (an agent caller always uses its own; the operator
must name one), as for exec (§17.4).

The values:

- `x`, `y`, `x1`, `y1`, `x2`, `y2` are pixels from the top-left corner of
  the full screen, `0` to `32767`. `click` and `scroll` take both `x` and
  `y` or neither (neither: where the pointer is).
- `button` is `left` (default), `right`, or `middle`. `double: true` clicks
  two times.
- `dx` and `dy` are wheel steps, at most 100. A positive `dy` scrolls down,
  a positive `dx` scrolls right.
- `chord` is key names joined with `-` or `+`: `ctrl-l`, `ctrl+alt+delete`,
  `Return`, `shift-f10`. Modifiers: `ctrl`, `alt`, `shift`, `super`
  (`win`, `meta`). Keys: a letter, a digit, `f1` to `f24`, `enter`
  (`return`), `tab`, `escape` (`esc`), `space`, `backspace`, `delete`,
  `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`,
  `pagedown`, `menu`, `print`, `pause`, `capslock`, `numlock`,
  `scrolllock`, and the punctuation names `minus`, `equal`, `bracketleft`,
  `bracketright`, `backslash`, `semicolon`, `apostrophe`, `comma`,
  `period`, `slash`, `grave`. The names do not depend on case.
- `text` is 1 to 2000 Unicode characters. Newline and tab are keys; other
  control characters are `422`. The host agent sends the text on the
  tool's stdin, never on a command line.
- `region` is `{"x", "y", "w", "h"}`. `scale` is `0.1` to `1`: the host
  agent crops first, then scales.
- `image_base64` is a PNG of at most 512 KiB. `threshold` is `0` to `1`
  (vmlab's default is `0.9`).

The screenshot answer:

- Without `format=json`: `image/png`, with the headers `X-Display-Width`,
  `X-Display-Height` (the image), `X-Screen-Width`, `X-Screen-Height` (the
  full screen), and `Cache-Control: no-store`.
- With `?format=json`: `{"target", "op": "screenshot", "format": "png",
  "width", "height", "screen": {"width", "height"}, "region", "scale",
  "bytes", "sha256", "png_base64"}`. The CLI and the dashboard use this
  form, because the agent tunnel carries JSON only.
- The PNG is at most 8 MiB. Ask for a `region` or a smaller `scale` for a
  larger screen. The daemon does not store a screenshot: the caller saves
  it.

The info answer:

```json
{"target":"win","kind":"vmlab","backend":"vmlab","width":1280,"height":800,
 "ops":["info","screenshot","move","click","drag","keys","type","ocr","find","record"],
 "ocr":"vmlab","input":{"allowed":false,"reason":"input on target `win` needs an exclusive lease ..."}}
```

`ops` lists what the backend does. `ocr` is the OCR engine: the backend's
own (`vmlab`), `tesseract` on the host agent, or `null`. `input` says if
the caller may send input now (the rules below). `info` takes a
screenshot to read the size.

**Rights.**

- `info`, `screenshot`, `ocr`, and `find` read the screen. The caller may
  use the target (§17.4: the operator, `policy.targets`, or an active
  lease), or is an admin of it (a sysadmin, or the project_admin of the
  target's project). Else `403 forbidden`.
- `move`, `click`, `drag`, `scroll`, `keys`, `type`, and `record` are
  input. The operator may always send input. An agent may send input when
  it holds an active **exclusive** lease on the target, or when it is an
  admin of the target. Else the answer is `409 conflict`, with a message
  that names `terrarium admin target grant <target> <agent> --exclusive`.
  While another agent holds an exclusive lease on the target, input from
  any agent other than that holder is `409 lease_held` (the message and
  the details name the holder and its `expires_at`).
- A lead grants `--exclusive` for display work, so two agents do not move
  one mouse.
- Each input operation writes the audit row `target.display` with the
  operation, the coordinates, the button, the chord, and for `type` only
  the length (`chars`), never the text. A read writes no audit row.
- Input is rate limited: at most 20 operations in one second on one
  target (`429 rate_limited`).

**Errors.**

| Answer | When |
|---|---|
| `403 forbidden` | The caller may not use the target. Also a `desktop` target on a host agent whose config does not opt in: "the desktop display of host `<h>` is off". Also an RDP server whose certificate is not the pinned one (`details.code` `refused`). |
| `404 not_found` | No target, or no such operation. |
| `409 conflict` | Input without an exclusive lease (above). The host agent is offline. A lab instance target with no instance. |
| `409 lease_held` | Another agent holds an exclusive lease on the target. |
| `422 invalid` | A bad value, a target with no display, or a host agent that cannot do the operation: `details.code` is `bad_request`, `unsupported` (a missing tool, with the package to install; a key with no RDP scancode; "no OCR"), or `failed` (the tool ran and failed, or a VNC or RDP connection or login failed, with its message). |
| `429 rate_limited` | Over 20 input operations in one second on the target. |

**The opt-in of a desktop.** The host agent's own config turns on its
desktop: `[display] enabled = true`, and optional `allowed_displays`
([host-protocol.md "Host agent configuration"](host-protocol.md#host-agent-configuration)).
The daemon cannot turn it on. The host agent sends the state in `hello`
(`capabilities.display.enabled`); `GET /api/hosts` shows it, and
`summary.display`. The daemon refuses a `desktop` target on a host with
the display off before it sends a frame, and the host agent refuses the
frame too.

**Secrets.** A `vnc` or `rdp` target names a `password` credential
(`POST /api/credentials {"name", "kind": "password", "value",
"username"?}`). The daemon reads it only to build the `display` frame,
and sends it only to the target's host agent in that frame. No answer to
an agent or to the dashboard holds it, and the audit rows do not. The host
agent does not log it, and its error texts do not hold it.

## 18. Fleet

A fleet file declares projects, their repos, agents, and targets, global
targets, and subscriptions in one TOML file. [fleet.md](fleet.md) has the
file format (§1), the ownership rules (§2), the CLI (§3), the config dir
(§4), and the policy (§5). This section is the HTTP contract.

All routes take the usual auth (§1). The bodies are JSON.

### 18.1 The `source` of a fleet object

A project, agent, target, subscription, schedule, or agent template that
a fleet owns shows `"source": "fleet:<name>"` in `GET /api/projects`,
`/api/agents`, `/api/targets`, `/api/subscriptions`, `/api/schedules`,
`/api/templates`, and their single-object routes. The
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
| `kind` | `subscription`, `project`, `target`, `template`, `agent`, or `schedule`. The key of a template is `project/name`, or `_/name` for a global one. |
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
target, an agent verb, a quota probe, or a card on the board. A trigger is
a schedule whose spec is `on`: an event of the log (§9, §20) that matches
starts its run (§19.4). The daemon runs a schedule as the actor that made
it (`run_as`). Before each run, the daemon checks the rights of that
actor again. PLAN §12b and §12c have the design.

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
| `catch_up` | `skip` (the default) or `once`. What the daemon does at its start with a run that it missed while it was down. For a trigger: `skip` ignores the events that came while the daemon was down; `once` runs them. |
| `max_concurrency` | Always `1`: one run at a time. |
| `created_by` | `operator` or `agent:<ulid>`. |
| `run_as` | The actor of the runs. It is `created_by`, until a caller changes the `action`: then it is that caller. |
| `next_run_at` | The next run. `null` when the schedule is disabled, when an `at` schedule has run, and always for a trigger. |
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
| `{"kind": "on", "kinds": ["ci.run.failed"], "match"?, "dedupe"?, "escalate"?, "max_per_hour"?: 30}` | A trigger: each matching event starts a run. See §19.4. |

The actions:

| `action` | Meaning |
|---|---|
| `{"type": "mail", "to": "<agent>\|project:<p>\|operator", "subject", "body", "thread"?}` | Send mail through the normal send path (§7) as `run_as`. The scope rules of §7.1 and the loop guard apply. With `thread`, each run posts on that thread, and the loop guard counts the messages. A bare agent name resolves in the schedule's project. An empty `subject` becomes `schedule <name>`. |
| `{"type": "turn", "agent", "text", "start_if_stopped"?: false}` | Type a turn, as `POST /api/agents/{id}/send` (§6.2). When the agent does not run, the run is `skipped: agent not running`. With `start_if_stopped`, the run starts the agent, waits until it is ready (at most 120 s), then types the turn. |
| `{"type": "exec", "target", "cmd": ["make", "test"], "cwd"?, "timeout_secs"?}` | Run a command on a target through the executor (§17.4), as `run_as`, with the same policy, leases, and `target.exec` audit row as any exec. `timeout_secs` is 300 by default, 3600 at most. |
| `{"type": "agent", "agent", "verb": "start\|stop\|restart"}` | The agent verb of §6.2, as `run_as`. |
| `{"type": "probe", "subscription"}` | Run the quota probe of the subscription (§16.3). |
| `{"type": "card", "op": "open\|close", "key", "title"?, "body"?, "column"?}` | A card on the board of the schedule's project (§8), as `run_as`. `open`: make a card with the label `trigger:<key>` in `column` (default `backlog`), or, when a card with that label is open (not in `done`, not archived), add `body` to it as a comment. `close`: move the open card with that label to `column` (default `done`), with `body` as the comment. No open card to close is `ok`. A global schedule cannot have a card action. |

A trigger runs only `mail`, `exec`, and `card`. To notify an agent, a
trigger mails it: `turn`, `agent`, and `probe` on a trigger are `422`
(`details.field` is `action`).

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
| `trigger` | `cron` (the timer, for each spec kind), `manual` (`POST .../run`), `catch_up`, or `event` (a trigger). |
| `summary` | One line. A skip starts with `skipped:`. |
| `output` | `exec`: `target`, `cmd`, `exit_code`, `end`, and the last 4000 bytes of `stdout` and `stderr`. `mail`: `message_id`, `thread_id`, `to`, `held`, `held_reason`. `turn`: `agent`, `started`, `delivery`, `status_before`. `agent`: `agent`, `verb`, `status`. `probe`: the probe outcome with `windows`. `card`: `card` (the ID, or `null`), `op` (`open`, `comment`, or `close`), and `column` for a close. |
| `error` | Why the run failed or was skipped. |
| `actor` | Who triggered the run: `run_as` for a timed run, the caller for a run-now, the actor of the event for a trigger run (for example `source:terra/github`). |
| `event_seq` | The `seq` of the event that started a trigger run. `null` for the other runs. |
| `event` | `{seq, kind, summary}` of that event, while the log has it. Absent for the other runs. |

### 19.2 Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/schedules?project=&enabled=&kind=` | | `200`, a list. With `project`: the project's schedules, and the global ones when the caller may read them. Without: all for the operator and a sysadmin; the caller's project for another agent. `kind=time` gives only the time schedules, `kind=trigger` only the triggers; another value is `400`. |
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
| `422 invalid` | A field is not valid. The message starts with the field, and `details.field` names it: `name`, `spec`, `spec.expr`, `spec.tz`, `spec.every`, `spec.at`, `action`, `action.to`, `action.body`, `action.agent`, `action.text`, `action.target`, `action.cmd`, `action.cwd`, `action.timeout_secs`, `action.subscription`, `action.subject`, `action.key`, `action.title`, `action.column`, `max_concurrency`, `project`, and for a trigger `spec.kinds`, `spec.match`, `spec.dedupe.key`, `spec.dedupe.window`, `spec.escalate.key`, `spec.escalate.after`, `spec.escalate.reset_on`, `spec.escalate.to`, `spec.max_per_hour`. A cron error shows the expression: ``spec.expr: bad cron expression `*/15 * * *`: use five fields ...``. An interval under 60 s and an `at` in the past are `422`. |
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
- `card`: the board of the project.
- A trigger's `escalate.to`: as for `mail`.

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
A file schedule runs as `operator`. A trigger in a file has the spec
`on`, with the same fields as in JSON:

```toml
[[schedules]]
name = "ci-failed"
[schedules.spec]
kind = "on"
kinds = ["ci.run.failed"]
match = [{ path = "data.branch", op = "glob", value = "main" }]
dedupe = { key = "{{event.data.repo}}:{{event.data.branch}}", window = "10m" }
[schedules.action]
type = "mail"
to = "worker"
subject = "CI failed: {{event.data.repo}}"
body = "Fix the build. Run: {{event.data.run_url}}"
```

### 19.4 Triggers

A trigger is a schedule with the spec `on`. It has the same rows, rights
(§19.3), run history, cap (`TERRARIUM_MAX_SCHEDULES_PER_PROJECT` counts
schedules and triggers together), project files, and fleets.

The spec:

```json
{
  "kind": "on",
  "kinds": ["ci.run.failed", "ci.job.*"],
  "match": [
    {"path": "data.branch", "op": "glob", "value": "main"},
    {"path": "data.repo", "op": "in", "value": ["wil/terrarium", "wil/site"]}
  ],
  "dedupe": {"key": "{{event.data.repo}}:{{event.data.branch}}", "window": "10m"},
  "escalate": {"key": "{{event.data.repo}}:{{event.data.branch}}", "after": 3,
               "reset_on": ["ci.run.succeeded"], "to": "operator"},
  "max_per_hour": 30
}
```

| Field | Meaning |
|---|---|
| `kinds` | One or more event kinds. `*` matches any text, `?` one character. |
| `match` | Zero or more rules; every rule must hold. `path` is `project`, `kind`, `actor`, `summary`, or `data.<dotted path>` (a number indexes a list). `op` is `eq` (the default), `ne`, `glob`, or `in` (`value` is a list). The value compares as text; a number compares as its JSON text. A missing value fails `eq`, `glob`, and `in`, and passes `ne`. |
| `dedupe` | `{key, window}`. One run for each rendered `key` in `window` (`90s`, `10m`, `1h`; 1 s to 7 d). A match in the window starts no run and records no run. |
| `escalate` | `{key, after, reset_on?, to?}`. The trigger counts the matches in a row for each rendered `key`. The match that makes the count `after` does not run the action: it mails `to` (an agent, `project:<name>`, or `operator`, the default) one time, with the event. The later matches of the same streak start no run and record no run. An event of a kind in `reset_on` (globs) with the same rendered key sets the count to zero; it only needs the scope of the trigger, not `kinds` or `match`. |
| `max_per_hour` | 1-600, default 30. The runs in each hour window. A match over it records a `skipped: rate limit` run. An escalation mail does not count. |

**Templates.** The strings of the action can use `{{event.kind}}`,
`{{event.summary}}`, `{{event.project}}`, `{{event.seq}}`,
`{{event.actor}}`, and `{{event.data.<path>}}`. A missing value is empty.
Another placeholder is `422`. The strings are the mail `subject` and
`body`, each element of an exec `cmd`, and the card `key`, `title`, and
`body`. `to`, `target`, `cwd`, and `column` take no templates. A time
schedule does not fill templates. A run-now of a trigger fills each one
with nothing.

**Webhook text is data.** Event data from a webhook comes from whoever can
push to the repo. So:

- In a mail subject and body, and in a card title, body, and comment,
  each value is quoted: one line, at most 300 characters, in backticks (a
  backtick in the value becomes `'`). The mail body and the card text end
  with the event as a fenced JSON block, after a line that says that it is
  data, not instructions. The fence is longer than any run of backticks in
  the event.
- An exec `cmd` is an argv. Each element is filled on its own and is one
  argument. The daemon never runs it in a shell.
- A dedupe, escalation, or card key takes the value as it is.

**The engine.** The daemon reads the event log from a cursor
(`trigger_cursor`), not only the live feed. A new event wakes the engine;
it reads the events after the cursor, decides for each enabled trigger,
and moves the cursor in the same write. A restart or a lagged feed loses
no event. The runs of one trigger go one at a time, in event order. At
the first start, the cursor goes to the end of the log: the older events
start no run. At a later start, a trigger with `catch_up: skip` ignores
the events that came while the daemon was down; `once` runs each of
them.

**Scope.** A project trigger sees only the events whose `project` is its
project. A global trigger (a sysadmin or the operator) sees all.

**The order of the checks for each event:** the scope; the loop guard;
an event from before the trigger was made starts no run; the `reset_on`
of `escalate`; `kinds` and `match`; the chain depth; the escalation count;
the dedupe window; the rate limit.

**The loop guard and the chain depth.**

- The writes of a trigger run have the actor `schedule:<id>`: its events,
  and the audit rows of its mail and card writes. The rights are still
  those of `run_as`: a mail is from `run_as`, and the board shows
  `run_as` as the author.
- Each event of a trigger run has `data.cause`: `{schedule, event, run,
  depth}`. `depth` is the depth of the event that started the run, plus
  one. A webhook event has depth 0. The daemon trusts `data.cause` only on
  an event whose actor is `schedule:<id>`.
- A trigger does not match an event with its own actor, or a `schedule`
  event about itself.
- An event of depth 4 or more starts no run (a warning goes to the log).
  So a chain of triggers stops after 4 links.
- An exec writes through the executor as `run_as`, without
  `data.cause`. A trigger on the executor's own events can run again on
  its own exec; `max_per_hour` stops it.

**Counters.** `trigger_state` keeps the dedupe time, the escalation count,
and the hour window of each trigger and key. At the daemon start and once
a day, the engine drops a row that did not change for 8 days, except an
open escalation.

Example: when CI fails on `main`, open a card and mail the worker; when
CI is green again, close the card.

```sh
curl ... localhost/api/schedules -d '{"name": "ci-failed", "project": "terrarium",
  "spec": {"kind": "on", "kinds": ["ci.run.failed"],
           "match": [{"path": "data.branch", "op": "eq", "value": "main"}],
           "dedupe": {"key": "{{event.data.repo}}", "window": "10m"},
           "escalate": {"key": "{{event.data.repo}}", "after": 3, "reset_on": ["ci.run.succeeded"]}},
  "action": {"type": "mail", "to": "worker", "subject": "CI failed: {{event.data.repo}}",
             "body": "Fix the build on {{event.data.branch}}. Run: {{event.data.run_url}}"}}'
curl ... localhost/api/schedules -d '{"name": "ci-card", "project": "terrarium",
  "spec": {"kind": "on", "kinds": ["ci.run.failed"]},
  "action": {"type": "card", "op": "open", "key": "ci:{{event.data.repo}}",
             "title": "CI failed on {{event.data.repo}}"}}'
curl ... localhost/api/schedules -d '{"name": "ci-card-close", "project": "terrarium",
  "spec": {"kind": "on", "kinds": ["ci.run.succeeded"]},
  "action": {"type": "card", "op": "close", "key": "ci:{{event.data.repo}}", "body": "Green again."}}'
curl ... 'localhost/api/schedules?kind=trigger'
```

## 20. Event sources and ingest

An event source is one webhook sender: `github`, `forgejo`, or `generic`.
It posts to the ingest listener, and each good request becomes one event
in the log (§9). A trigger (§19) can then act on the event. PLAN §12c has
the design.

### 20.1 Shapes

A source:

```json
{
  "id": "01MA1C3K8V0Q6Y2W4E7R9T5U1P",
  "name": "gh",
  "project": "terrarium",
  "kind": "github",
  "enabled": true,
  "created_by": "operator",
  "last_seen": "2026-10-26T09:04:13.120Z",
  "last_error": null,
  "received": 12,
  "created_at": "2026-10-26T08:00:00.000Z",
  "updated_at": "2026-10-26T08:00:00.000Z",
  "hook_path": "/hooks/terrarium/gh",
  "hook_url": "https://hooks.example.com/hooks/terrarium/gh",
  "setup": ["GitHub: the repository (or organisation) > Settings > Webhooks > Add webhook.", "..."]
}
```

| Field | Meaning |
|---|---|
| `name` | 1-63 of `a-z`, `0-9`, `-`, `_`. Unique in its scope: the project, or the global scope. |
| `project` | The project. `null` is a global source. The events of the source go into this project; a global source makes events with no project. |
| `kind` | `github`, `forgejo`, or `generic`. It sets the signature check and how the body becomes an event (§20.3). |
| `last_seen` | The last request with a good signature. |
| `last_error` | The time and the reason of the last failed request, for example `bad or missing signature`. A good request clears it. The sender gets no detail; the admin reads it here. |
| `received` | The events that the source added to the log. |
| `hook_path` | `/hooks/{project}/{name}`. `_` is the project part of a global source. |
| `hook_url` | `TERRARIUM_INGEST_PUBLIC_URL` and `hook_path`. Without it, the daemon makes the URL from `TERRARIUM_INGEST_ADDR` (`0.0.0.0` becomes `localhost`), and `hook_note` says so. `null` when the ingest listener is off. |
| `setup` | Short setup steps for the sender: the content type, the secret, and which events to select. |
| `secret` | 32 random bytes in hex. **Only** in the answers of `POST /api/event-sources` and `POST .../rotate`. No list, show, event, or audit row has it. |

### 20.2 Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/event-sources?project=` | | `200`, a list. With `project`: that project's sources, and the global ones when the caller may read them. Without: all for the operator and a sysadmin; the caller's project for another agent. |
| `POST /api/event-sources` | `{name, kind, project?, enabled?: true}` | `201`, the source with `secret`. An absent `project` is the agent caller's project (the operator: global). `null` is global. |
| `GET /api/event-sources/{id}` | | `200`, the source. `{id}` is a ULID, `project/name`, `_/name` (encode the `/` as `%2F`), or a name. |
| `PATCH /api/event-sources/{id}` | `{name?, enabled?}` | `200`, the source. A new name changes the hook URL. |
| `POST /api/event-sources/{id}/rotate` | | `200`, the source with a new `secret`. The old secret stops at once. |
| `DELETE /api/event-sources/{id}` | | `204`. |
| `POST /api/events` | `{kind, summary?, data?, project?}` | `201`, the event (§9). See §20.5. |

Errors: `422 invalid` (a bad name or kind), `409 conflict` (the name is
in use in the scope), `404 not_found` (no such source or project), `403
forbidden` (the rules below).

| Caller | Read | Create, change, rotate, delete |
|---|---|---|
| operator | all | all |
| `sysadmin` | all | all, global sources too |
| `project_admin` | its project's | its project's |
| `member` | its project's | no |

Each change writes an audit row (`event_source.create`, `.update`,
`.rotate`, `.delete`) and an `event_source` event (`{change, source, name,
key, kind, enabled}`; `change` is `created`, `updated`, `enabled`,
`disabled`, `rotated`, or `deleted`).

### 20.3 The ingest listener

The ingest listener is a TCP listener of its own on
`TERRARIUM_INGEST_ADDR` (unset: off). Compose sets `0.0.0.0:8083` and
publishes `${TERRARIUM_INGEST_PORT:-8083}`. The listener serves only two
routes. The API and the dashboard are not on it, so you can open this
port to a forge and keep the API closed.

| Route | Answer |
|---|---|
| `GET /healthz` | `200 ok`. |
| `POST /hooks/{project}/{source}` | See below. `_` as `{project}` names a global source. |

The body limit is 1 MiB (`413` above it). The body must be
`application/json`. A form body (`application/x-www-form-urlencoded`) gets
`415`, with a hint to change the content type of the webhook.

The signature check of each kind:

| `kind` | Signature | Event | Delivery ID |
|---|---|---|---|
| `github` | `X-Hub-Signature-256: sha256=<hex>`, the HMAC-SHA256 of the body with the secret | `X-GitHub-Event` | `X-GitHub-Delivery` |
| `forgejo` | `X-Forgejo-Signature: <hex>` or `X-Gitea-Signature: <hex>` (no prefix); else `X-Hub-Signature-256` | `X-Forgejo-Event`, else `X-Gitea-Event` | `X-Forgejo-Delivery`, else `X-Gitea-Delivery` |
| `generic` | `X-Terrarium-Signature: sha256=<hex>`, or `Authorization: Bearer <secret>` | the `kind` of the body | `X-Terrarium-Delivery` (optional) |

The compare is constant-time. The answers:

| Status and body | When |
|---|---|
| `401` `{"error": {"code": "unauthorized", "message": "unauthorized"}}` | An unknown source, a source of another project, a disabled source, or a bad or missing signature. The answer is the same for each, with no detail. The source keeps the reason in `last_error`. |
| `200 {"status": "accepted", "seq": 812, "kind": "ci.run.failed"}` | The event is in the log. |
| `200 {"status": "duplicate"}` | The source saw this delivery ID before (the daemon keeps them for 7 days). No event. |
| `200 {"status": "pong"}` | A GitHub `ping`. No event. |
| `200 {"status": "ignored", "reason": "..."}` | A known webhook that makes no event, for example a job that succeeded or a run that waits. |
| `400 {"status": "rejected", "error": "..."}` | A good signature, but the body is not JSON, the event header is missing, or a generic body is not valid. |

A good request sets `last_seen`. The event has the actor
`source:<project>/<name>` (`source:_/<name>` for a global source) and the
project of the source.

A generic body is `{kind, summary?, data?}`. The kind starts with `ci.`,
`git.`, `pr.`, `ext.`, or `agent.`, so a script cannot pose as the daemon.
`data` is a JSON object.

### 20.4 Standard events

`terrarium-core` (`ci.rs`) turns a GitHub or Forgejo webhook into one
event. `docs/compat.md`, "Webhooks (GitHub, Forgejo)", records the payload
shapes and the versions.

| Webhook | Event |
|---|---|
| GitHub `workflow_run` `requested` | `ci.run.started` |
| GitHub `workflow_run` `completed` | by `conclusion`: `success` `ci.run.succeeded`; `failure`, `timed_out`, `startup_failure` `ci.run.failed`; `cancelled` `ci.run.cancelled`; any other (`neutral`, `skipped`, `stale`, `action_required`) `ci.run.completed` |
| GitHub `workflow_run` `in_progress` | ignored |
| GitHub `workflow_job` `completed` with `failure` or `timed_out` | `ci.job.failed` |
| GitHub `workflow_job`, any other | ignored |
| Forgejo 17 `workflow_run_running`, `_success`, `_failure`, `_cancelled`, `_skipped` | `ci.run.started`, `.succeeded`, `.failed`, `.cancelled`, `.completed` |
| Forgejo 17 `workflow_run_waiting`, `_blocked` | ignored |
| Forgejo 17 `workflow_job_failure` | `ci.job.failed` |
| Forgejo 17 `workflow_job_*`, any other | ignored |
| Forgejo 12-16 `action_run_failure` | `ci.run.failed` |
| Forgejo 12-16 `action_run_success`, `action_run_recover` | `ci.run.succeeded` |
| `status` (GitHub; Gitea 1.23 and later. Forgejo does not send it) | by `state`: `pending` `ci.run.started`; `success` `ci.run.succeeded`; `failure`, `error` `ci.run.failed` |
| `push` | `git.push` |
| `pull_request` `opened`, `reopened` | `pr.opened` |
| `pull_request` `closed` | `pr.merged` when `pull_request.merged` is true, else `pr.closed` |
| `pull_request` `synchronize` (GitHub), `synchronized` (Forgejo), `edited`, `ready_for_review`, `converted_to_draft` | `pr.updated` |
| GitHub `ping` | no event (`pong`) |
| Any other webhook, and any other `pull_request` action | `ext.<provider>.<event>`, with `data.action` |

`data` of each event:

| Field | Meaning |
|---|---|
| `source` | The source key, `project/name` or `_/name`. |
| `provider` | `github`, `forgejo`, or `generic`. |
| `event`, `action`, `delivery` | The event header, the `action` of the payload, and the delivery ID. |
| `repo`, `repo_url` | `owner/name` and its web URL. |
| `branch`, `commit` | The branch (`null` for a tag or a Forgejo pull request run) and the commit SHA. |
| `run_id`, `run_url` | The run, and the web page of the run (of the job, for `ci.job.failed`). |
| `workflow`, `job` | The workflow name (Forgejo: the workflow file, for example `ci.yml`; `status`: the `context`), and the job name. |
| `conclusion`, `attempt` | The conclusion or status, and the run attempt. |
| `sender` | The login of the user who caused the webhook. |
| `pr` | `{number, title, url, head, base, author, merged, merged_by, draft}` for `pr.*`; `{number, head, base}` for a run of a pull request; else `null`. |
| `ref`, `tag`, `before`, `deleted`, `commits`, `message`, `pusher`, `compare_url` | `git.push` only. `message` is the first line of the head commit. |

A field that the webhook does not have is `null`. All text from a payload
is one line: the daemon changes control characters to spaces and cuts a
field at 200 characters (a summary at 300). The text still comes from
whoever can push to the repository. The daemon never puts it in a shell,
and a mail shows it as quoted data.

### 20.5 `POST /api/events`

A client adds one event to the log. The answer is `201` with the event.

| Caller | May emit |
|---|---|
| operator | Any kind, into any project (`project`), or into no project (absent or `null`). |
| agent | Into its own project only (an absent `project` is its project; another is `403`). Only the kinds that start with `ci.`, `git.`, `pr.`, `ext.`, or `agent.`; a kind of the daemon (`status`, `mail`, `card`, `turn_start`, `schedule`, `memory.*`, ...) is `403`. The event names the agent in `agent` and `agent_id`. |

`kind` is 1-100 of `a-z`, `0-9`, `_`, `-`, in segments split by `.`,
starting with a letter (else `422`). `data` is a JSON object (default
`{}`). `summary` is one line of at most 300 characters (default: the
kind). The actor is the caller (`operator` or `agent:<ulid>`).

```sh
curl --unix-socket "$SOCK" -H 'content-type: application/json' localhost/api/events \
  -d '{"kind": "ci.run.failed", "project": "terrarium", "summary": "nightly failed", "data": {"branch": "main"}}'
```

Example: a project_admin agent makes a GitHub source and gives the URL
and the secret to the repository owner.

```sh
terrarium event source add gh --kind github
```

**Retention.** The daily prune timer deletes the delivery IDs that are
older than 7 days.

## 21. Agent templates

An agent template is a named set of agent settings without a name and a
project. A create copies them: `POST /api/agents` with `template`. A
project_admin lead makes its workers from a template, so it gives each
setting one time. PLAN §5b has the design. An agent template is not a
prompt template (`prompt`, a file under `prompts/`).

### 21.1 Shapes

A template:

```json
{
  "id": "01MA2T0K8V0Q6Y2W4E7R9T5U1P",
  "name": "worker",
  "project": "terrarium",
  "description": "A board worker",
  "spec": {"harness": "claude", "subscription": "anthropic-max", "prompt": "default",
           "policy": {"targets": [], "mail_to": ["*"]}, "restart": "on-failure"},
  "revision": 2,
  "created_by": "agent:01M5PA0T0Z0K16V6G88P3SE78W",
  "source": "api",
  "created_at": "2026-10-26T08:00:00.000Z",
  "updated_at": "2026-10-26T09:12:40.031Z"
}
```

| Field | Meaning |
|---|---|
| `name` | 1-63 of `a-z`, `0-9`, `-`, `_`. Unique in its scope: the project, or the global scope. A project template and a global template can have the same name. |
| `project` | The project. `null` is a global template. |
| `spec` | An agent definition (§6.1) without `name` and `project`: `harness` (required), `subscription`, `model`, `prompt`, `system_prompt`, `roles`, `policy`, `restart`, `new_session_on_prompt_change`. The daemon checks it with the rules of an agent definition and keeps it in a normal form: the defaults of `policy` and `restart` filled in, no empty `roles`, no empty `system_prompt`, no `new_session_on_prompt_change` when it is `true`. A `system_prompt` over 32 KiB is `422 invalid`. Another key (`name`, `project`, `template`, or an unknown key) is `422 invalid` with `details.field = "spec"`. |
| `revision` | `1` at the create. Each change of the name, the description, or the spec adds `1`. A `PATCH` with the same values changes nothing. |
| `source` | `api`, `file` (a `[[templates]]` of `projects/<name>.toml`), or `fleet:<name>` (§18.1). A `file` template refuses `PATCH` and `DELETE` with `409 file_managed`. |

A template key in a path is a ULID, `project/name`, `_/name` or
`global:name` (a global template), or a name. A name finds the template of
the caller's project first, then the global one. Encode `/` as `%2F`.

### 21.2 Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/templates?project=&scope=` | | `200`, a list (not paged): global first, then by project and name. `?project=P`: the templates of `P` and the global ones. `?scope=global`: the global ones only. Neither: all for the operator and a sysadmin; the caller's project and the global ones for another agent. |
| `POST /api/templates` | `{name, spec, project?, description?}` | `201`, the template. An absent `project` is the agent caller's project (the operator: global). `null` or `""` is global. |
| `GET /api/templates/{key}` | | `200`, the template. |
| `PATCH /api/templates/{key}` | `{name?, description?, spec?}` | `200`, the template. `spec` replaces the whole spec. |
| `DELETE /api/templates/{key}` | | `204`. The agents made from it stay as they are. |
| `GET /api/templates/{key}/resolve?project=P` | | `200`, `{template: {id, name, project, revision}, agent: {...}}`: the template that a create of an agent in `P` with `template: "{key}"` uses, and its spec with `project: P`. Without `project`: the caller's project. The dashboard fills its agent form from `agent`. |
| `POST /api/templates/from-agent/{agent}` | `{name, project?, description?}` | `201`, a template with the settings of the agent now. An absent `project` is the agent's project; `null` is global. The default description is `from agent <project>/<name>`. |

Errors: `422 invalid` (a bad name or spec), `409 conflict` (the name is in
use in the scope), `409 file_managed`, `404 not_found` (no such template or
project), `403 forbidden` (the rules below).

| Caller | Read | Create, change, delete |
|---|---|---|
| operator | all | all |
| `sysadmin` | all | all, global templates too |
| `project_admin` | its project's and the global ones | its project's |
| `member` | its project's and the global ones | no |

Each change writes an audit row (`template.create`, `.update`, `.delete`)
and a `template` event: `{change, template, name, key, revision}`;
`change` is `created`, `updated`, or `deleted`, and `key` is
`project/name` or `_/name`. A project template's event has the project.
Deleting a project deletes its templates.

### 21.3 Create an agent from a template

`POST /api/agents` takes `template`, a template name. `name` and `project`
are required; each other field of the definition is optional.

```json
{"name": "w1", "project": "terrarium", "template": "worker", "model": "sonnet",
 "policy": {"targets": ["terrarium-box"]}}
```

1. **Resolve.** A name picks the template of `project`, else the global
   one. `global:<name>` (or `_/<name>`) picks the global one. A ULID
   picks that template. A template of another project is `422 invalid`
   (`details.field = "template"`); no template is `404 not_found`.
2. **Merge.** The daemon puts the body over the spec. A field of the body
   wins over the same field of the spec. `policy` merges one level deeper:
   each of `targets`, `mail_to`, and `cross_project` in the body wins over
   the same key of the spec. A list is not merged: the body's list
   replaces the spec's list. `null` removes the field, so the agent gets
   the default (`"model": null` clears the template's model, and
   `"system_prompt": null` gives the harness's default system prompt;
   `"policy": {"mail_to": null}` gives `["*"]`).
3. **Check.** The result is an agent definition, and the create checks it
   as it checks any definition (§6.1): project_admin of `project`, the
   grant rights of each role (§3), and the `policy` rules of F13. A
   template never lets a caller grant a role or a target that it could not
   grant in the body itself.

The answer is the agent with `template: {id, name, project, revision}`.
The daemon copies the values: a later change or delete of the template
does not change the agent. The audit row `agent.create` has the template.

### 21.4 Files and fleets

A project file has `[[templates]]` (`source = file`). A fleet has
`[[templates]]` (global) and `[[projects.templates]]` (`source =
fleet:<name>`). The spec keys sit next to `name` and `description`:

```toml
[[projects.templates]]
name = "worker"
description = "A board worker"
harness = "claude"
prompt = "default"
[projects.templates.policy]
mail_to = ["lead"]

[[projects.agents]]
name = "w1"
template = "worker"     # or "global:worker"
model = "sonnet"        # a key of the entry wins over the template
```

A template or an agent entry can set `system_prompt = """..."""`. A fleet
file or a project file read from disk can give `system_prompt_file =
"<path>"` instead, relative to the file: the CLI (`fleet validate | plan |
apply`) and the config dir read the file into `system_prompt`. A fleet
text that the API gets with `system_prompt_file` is an error, because the
daemon does not know the path. `GET /api/fleet/export` writes
`system_prompt`.

A fleet agent entry with `template` gets `[defaults]`, then the
template, then its own keys, with the merge of §21.3. The template comes
from the file first, then from the daemon. An apply writes the agent from
the template again, as the file is the truth for a fleet (§18). A fleet
export writes the project's templates, and `template = "..."` and the keys
that differ for each agent of a template.

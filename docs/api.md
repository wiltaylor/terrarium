# Terrarium daemon API

## Request threads

Ask an agent and Message an agent open `POST /api/threads` with
`{report_to: "project/lead", topic, text}`. Only the operator opens a
request, and `report_to` must name a project_admin. The scope is that lead's
project. The daemon spawns `global:request` (META-136), with no named agent
or alternative template accepted. A missing template fails the start with
the existing closed-thread error rather than silently sending plain mail.
Forwarding an existing Inbox question still uses the watched-mail path.

The request agent acknowledges the request, mails the selected lead, links
and tracks the lead's cards, and reports in the thread. It does not change
code or perform the work. Existing thread card-link and tracking endpoints
apply. An answer-only close proposal waits for one-click operator
confirmation, or is confirmed automatically after 24 hours without activity.
An operator reply withdraws the proposal. Threads with linked cards retain
the ordinary close lifecycle. The operator can close a request at any time.

`POST /api/threads/{id}/swap` with `{template, reason}` replaces an open
request's agent with a new agent from `global:architect` or
`global:architect-alt`. Project templates and other global templates are
refused. Only Architect and Member roles are allowed, with the resolved
definition checked again in the agent creation transaction. The new agent
is spawned as and owned by the report-to lead, not the operator. The operator
or report-to lead can do this. `terrarium thread swap <id> --template <key>
--reason <text>` is the CLI equivalent.

`POST /api/threads/{id}/reopen` with `{reason, card?}` reopens a closed request.
The optional card ID is linked atomically, so a lead can reopen a request
by linking new related work from its project, even while the thread is closed.
The operator, report-to lead, or a project_admin whose project has a card
linked to the thread can reopen it. `terrarium thread reopen <id> --reason
<text>` is the CLI equivalent. Reopening creates a new `global:request`
agent, never restores the previous session. The reason is a visible post.
Both replacement paths keep the conversation and linked cards and include
the thread history as quoted data in the new agent's task.
The replacement epoch is checked during creation and attachment. A stale
spawn cannot attach or close a newer replacement. Automatic answer-only
closure reads ordered batches of at most 64 requests, using the last
`(updated_at, id)` key to continue after releasing the database read lock.

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
[12 Artifacts](#12-artifacts) · [13 Versions (removed)](#13-versions-removed) ·
[14 Not in this phase](#14-not-in-this-phase) ·
[15 Notes for clients](#15-notes-for-clients) ·
[16 Subscriptions and usage](#16-subscriptions-and-usage) ·
[17 Targets, hosts, and exec](#17-targets-host-agents-and-exec-sessions) ·
[18 Fleet](#18-fleet) · [19 Schedules](#19-schedules) ·
[20 Event sources](#20-event-sources-and-ingest) ·
[21 Agent templates](#21-agent-templates) · [27 DB](#27-db) ·
[28 System prompts](#28-system-prompts) · [29 Test runs](#29-test-runs) · [30 Monitors and the project dashboard](#30-monitors-and-the-project-dashboard) ·
[34 Self-update](#34-self-update).

## 1. Transports and callers

### Operator preferences

`GET /api/preferences` reads the operator's server-side preferences.
`PUT /api/preferences` replaces them with `{"default_agent":"meta/lead"}`
or `{"default_agent":null}` to clear the default. Both are operator-only
(`403 forbidden` for agents) and return the same object. The PUT accepts
an existing agent's ULID or `project/name` (`404 not_found` otherwise).
The response uses `project/name`. The preference is stored in SQLite,
survives daemon and browser restarts, and becomes null if the agent is deleted.
Ask an agent and Message an agent preselect this recipient, while allowing
an override. With no default they retain the empty picker. Home and Inbox
use the same dialogs.

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
  dashboard Settings, Account tab).
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
| `TERRARIUM_GIT_NAME` | none | `Terrarium` | Author and committer name for agent commits, independent of forge authentication. Set in the deployment `.env` to override per instance. |
| `TERRARIUM_GIT_EMAIL` | none | `terrarium@localhost` | Author and committer email for agent commits, independent of forge authentication. Set in the deployment `.env` to override per instance. |
| `TERRARIUM_MAIL_LOOP_LIMIT` | none | `6` | The mail loop guard (§6, PLAN §12): the agent-to-agent messages a thread can have since the last operator or project-inbox message before new agent mail on it types no pointer. The daemon reads it once. |
| `TERRARIUM_HARNESS_AUTO_UPDATE` | `--harness-auto-update` | `daily` | The background check for harness updates (§4, "Auto-update and pins"): `daily`, `off`, or a duration of `1h` or more (`6h`). A value that does not parse stops the daemon at start. |
| `TERRARIUM_HARNESS_UPDATE_MAIL` | `--harness-update-mail` | `off` | An Inbox mail for each update that the auto-update installs (§4, "Auto-update and pins"): `off` or `on`. A failed install sends one mail for each failed version with both values. |
| `TERRARIUM_SYSTEM_PROMPT_MAX` | none | `32K` | The most bytes in a system prompt: an agent's `system_prompt` and a file of the system prompt library (§28), on every route (the API, a fleet or agent file, and the agent start). Bytes, or a size with `K` (`64K`). The highest value is `96K`: Claude Code gets the system prompt as one command line argument, and Linux refuses an argument over 128 KiB. A value that is not valid gives `32K`, with a warning. The daemon reads it once. |
| `TERRARIUM_IDLE_STOP` | none | `10m` | The idle stop of an agent that sets no `idle_stop` (§6.2, PLAN §18): a duration (`30m`, `1h30m`), or `off` (also `0`). The daemon reads it once. |
| `TERRARIUM_COMPACT_ON_IDLE` | none | `off` | The `compact_on_idle` of an agent that sets none (§6.2, "Auto compact"): `on` or `off` (also `1`, `true`, `yes`, and `0`, `false`, `no`). The daemon reads it once. |
| `TERRARIUM_COMPACT_MAX_AGE` | none | `off` | The `compact_max_age` of an agent that sets none (§6.2, "Auto compact"): a duration (`24h`, `12h30m`), or `off` (also `0`). The daemon reads it once. |
| `TERRARIUM_THREAD_REMINDER` | `--thread-reminder` | `3d` | The time that an Inbox thread waits on the operator with no activity before its one Inbox reminder (§6.13, "Untouched threads"): a duration (`12h`), or `off` (also `0`). |
| `TERRARIUM_THREAD_STALE` | `--thread-stale` | `14d` | The time that an Inbox thread waits on the operator with no activity before the daemon closes it as stale and mails its report-to lead (§6.13): a duration, or `off` (also `0`). |
| `TERRARIUM_QUOTA_SAMPLE_KEEP` | `--quota-sample-keep` | `48h` | The quota probe samples (`quota_samples`, §16.6) younger than this stay; each hour the scheduler deletes the older ones in transactions of at most 500 rows, and the newest sample of each window stays at any age. The default history of `GET /api/subscriptions/{name}/quota` (§16.3). A duration of at least `24h`. |
| `TERRARIUM_CARD_REMINDER` | none | `on` | The `card_reminder` of an agent that sets none (§6.2, "Card reminder"): `on` or `off` (also `1`, `true`, `yes`, and `0`, `false`, `no`). Empty or not valid: on. The daemon reads it once. |
| `TERRARIUM_ARTIFACT_LISTEN` | `--artifact-listen` | `0.0.0.0:8081` | The artifact origin listener. Empty: none, and the dashboard port serves `/a/...`. |
| `TERRARIUM_ARTIFACT_PUBLIC_URL` | `--artifact-public-url` | unset | The artifact origin in signed URLs and the CSP. |
| `TERRARIUM_WS_QUERY_TOKEN` | `--ws-query-token` | `0` | `1`: take `?token=` on an upgrade. |
| `TERRARIUM_PROJECT_PAGES_V2` | `--project-pages-v2` | `0` | Project pages (TER-1077). Strict switch parser. `1` turns them on: see "Project page layout v2" for the switch-on step and what the first start initializes. `0` is the rollback. |
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
  (§10, §12). Wiki pages have ULID IDs, and also keys by
  path (§11).
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
| 401 | `unauthorized` | No token or a wrong token on TCP (also a wrong, rotated, or revoked link key, and a failed link pairing, §36); `X-Terrarium-Agent` without `--insecure-agent-header`, or an unknown agent in it. |
| 403 | `forbidden` | The actor does not have the capability (§3). |
| 403 | `link_forbidden` | A link key called a route of the link deny list, or a read-only link tried to write (§36.4). |
| 404 | `not_found` | No such route or object, or the actor may not read the message. |
| 405 | `method_not_allowed` | The route exists, but not with this method. |
| 409 | `conflict` | The name exists, the card is archived or assigned, a column has cards. |
| 409 | `file_managed` | The object is declared in a definition file. Edit the file, then `POST /api/reload`. |
| 409 | `confirm_required` | The delete needs `?confirm=true`. |
| 409 | `wip_limit` | The target column is full. `details: {"column", "wip_limit"}`. |
| 409 | `dependencies_unmet` | The card cannot enter the `working` column (§8.2). `details: {"blocked_by": [card ids]}`. |
| 409 | `not_running` | The agent is not running. |
| 409 | `already_running` | The agent is running already. |
| 409 | `stale_base` | A wiki edit names a base revision that is not current. `details: {"base", "current", "diff"}`. |
| 409 | `lease_held` | Another agent holds a lease on the target that the new lease clashes with (§17.7). `details: {"target", "holder", "holder_id", "lease_id", "exclusive", "expires_at"}`. |
| 413 | `too_large` | An upload is over the per-file limit or the project quota. |
| 415 | `unsupported_media_type` | An attachment is not a PNG, JPEG, GIF, or WebP image (§6.2). |
| 429 | `rate_limited` | Too many calls of one kind; try again later. The credential helper (§5.1) has a limit per agent. An agent has too many live exec sessions (§17). A host agent runs its maximum of sessions. All upload slots of the daemon are busy (§12.2). |
| 501 | `not_available` | The route exists, but this phase does not do the request. |
| 503 | `unavailable` | The daemon cannot do the request on this machine: the repo browser has no `bwrap` to read an agent tree (§5.3). |
| 422 | `invalid` | Well-formed values that break a rule: a bad name, an empty title, a missing column. |
| 422 | `comment_required` | A move to the `blocked` column (§8.2) without a comment; a version rejection or a wiki review comment without a comment. |
| 500 | `internal` | A daemon bug or an I/O error. |

## 3. Roles and rules

The daemon checks the actor on every call (PLAN §10). The operator may do
everything. Every privileged action writes an audit row (§9).

| Capability | member | project_admin | sysadmin | system architect |
|---|---|---|---| --- |
| Read own project: project, agents, repos, targets, board, cards | yes | yes | all projects | all projects |
| Browse a repo (§5.3): its trees, files, changes, commits | own project | own project | all projects | all projects |
| Mail (§7.1 scope rules) | own project | own project | any agent | own project (as a member); reads every thread |
| Read and reply from the project inbox | no | own project | any project | read any project, reply no |
| Create, change, start, stop, restart, delete agents; send turns; answer; attach the PTY | no | own project | any | no (reads agents, sessions, transcripts, the screen) |
| Read agent templates (§21) | own project and global | own project and global | all | all |
| Create, change, delete agent templates (§21) | no | own project | any, global too | no |
| Assign, reassign, reopen any card; edit columns and WIP limits; archive any card | no | own project | any | no |
| Attach repos; change the project | no | own project | any | no |
| Grant, extend, or revoke a target lease (§17.7) | no | a target of own project, to an agent of own project | any | no |
| Check out a repo into its own workspace; ask the credential helper (§5.1) | own project | own project | own project | own project |
| Check out a repo into another agent's workspace | no | own project | any | no |
| Store, list, show, or delete secrets (§5.2); never a value | no | own project | any, global too | list and show any (metadata); store and delete no |
| Reveal the value of a secret (§5.2) | no | no | no (operator only) | no (operator only) |
| Create and delete projects; `POST /api/reload` | no | no | yes | no |
| Read the self-update state, check for a release, update, roll back (§34) | no | no | yes | read only |
| Drain the daemon for an update (§34.5) | no | no | no (operator only) | no (operator only) |
| Grant or revoke `project_admin` | no | own project | any | no |
| Grant or revoke `sysadmin` | no | no | no (operator only) | no (operator only) |
| Read the audit log | no | own project's rows | all | all |
| Read memories, pages, artifacts; write memories, links, tags, pages, uploads | own project | own project | any | read any; write own project |
| Read and write the DB (§27): types, records, queries | own project | own project | any | read any; write own project |
| Delete a DB type with its records (§27) | no | own project | any | no |
| Edit or delete any memory; move, delete, or undelete a page; delete any artifact; release a version | no | own project | any | no |
| Approve or reject a version | no | no | no (operator only) | no (operator only) |
| Prune artifacts | no | no | yes | no |
| Grant or revoke `architect` | no | no | no (operator only) | no |
| Run a command, a shell, a copy, or keys on a target or an agent | per the target rules (§17.4) | per the target rules (§17.4) | any | no |

The `system architect` (role `architect`, TER-47) plans large changes: it
reads what a sysadmin reads, in every project (the lists, boards, cards,
agents and their sessions, transcripts, and screens, mail threads and
project inboxes, the audit log, the event stream, secret metadata, the
self-update state), and writes only what a member of its own project may
write: its own cards, comments, memories, pages, uploads, and mail. In
another project it changes nothing, and it never runs a command, opens a
terminal, controls an agent, grants, or leases. Only the operator grants
or revokes it. An agent has `architect` alone or with `project_admin` of
its own project; with `sysadmin` it is `422 invalid` (sysadmin already
reads everything). An architect gets the worker persona unless it is also
a project_admin.

Limits that always hold:

- An agent cannot change its own roles or delete itself.
- Deleting a project, or an agent with a live session, needs `?confirm=true`.

## 4. System

### `GET /api/health`

```json
{"status":"ok","version":"0.1.0+6e31754","uptime":1,"started_at":"2026-10-24T06:36:53.143044982Z",
 "definitions":{"projects":1,"agents":1,"targets":0,"subscriptions":0},"idle_stop":"10m",
 "compact_on_idle":false,"compact_max_age":"off","card_reminder":true,
 "compact_harnesses":["claude","codex","copilot","pi"]}
```

`version` is the build version of the daemon (below). `uptime` is in
seconds. `definitions` counts the files loaded at the last
start or reload. `idle_stop` is the idle stop of an agent that sets none
(`TERRARIUM_IDLE_STOP`, §6.2): `off`, or a duration such as `10m`.
`compact_on_idle` and `compact_max_age` are the auto compact settings of
an agent that sets none (`TERRARIUM_COMPACT_ON_IDLE`,
`TERRARIUM_COMPACT_MAX_AGE`, §6.2). `card_reminder` is the card
reminder of an agent that sets none (`TERRARIUM_CARD_REMINDER`, §6.2).
`compact_harnesses` lists the harnesses whose agents the daemon can
compact.

### `GET /api/system/version`

The build of the daemon. Any caller that passes auth may read it: the
operator, a dashboard session, and an agent.

```json
{"version":"0.1.0+6e31754","cargo_version":"0.1.0","git_ref":"6e31754",
 "dirty":false,"build_time":"2026-11-04T10:20:30Z"}
```

- `version` is the build version: `<cargo_version>+<git_ref>`, with
  `-dirty` when the build tree had changes to tracked files
  (`0.1.0+6e31754-dirty`). The daemon, the CLI, the runner, and the host
  agent have the same form (`--version`). It is `+unknown` when the build
  did not know its commit.
- `git_ref` is the short commit (7 characters), or `unknown`.
- `build_time` is RFC 3339 UTC: `SOURCE_DATE_EPOCH` when the build set it,
  else the time of the build.
- The image build gets the commit from the `TERRARIUM_GIT_REF` build arg:
  its source (`git archive`) has no `.git`. `just docker::image-build`,
  `just host::build`, and the deploy (`src/.deployed-ref`) set it.
- `terrarium version` shows this next to the CLI's own build, and warns when
  the two refs differ. `GET /api/hosts` marks a host agent of another build
  `stale` (§17.1).
- `GET /api/system/update` (§34) adds the newest release and the state of
  the updater.

### `GET /api/harnesses`

Every caller may read it. For each registered harness: the base version
and the package from `harnesses.toml`, and the version that
`<binary> --version` reports. Terrarium keeps no range of supported versions (the harnesses
release about daily), so any installed version can start an agent. The
daemon caches each version for 10 minutes; `?refresh=true` runs the
binaries again.

```json
{"harnesses":[{"harness":"claude","package":"@anthropic-ai/claude-code","base":"2.1.310",
   "pinned":"2.1.310","installed":"2.1.310","checked_at":"2026-10-25T10:00:00Z"},
  {"harness":"pi","package":"@earendil-works/pi-coding-agent","base":"0.85.1",
   "pinned":"0.85.1","installed":null,
   "error":"cannot run pi: No such file or directory","checked_at":"2026-10-25T10:00:00Z"}]}
```

`base` is the version of the first install (`harnesses.toml`). It is not
a pin: the runtime pin is `auto_update.pinned_version` (below). `pinned`
is deprecated: it has the same value as `base`, for older clients.
`installed` is `null` when the version is not known, and `error` says why.
`terrarium admin harness ls` prints this table.

With a tools dir, each row also has these fields:

| Field | Meaning |
|---|---|
| `active` | The active version dir (`tools/current/<name>`): the version that the next agent start runs. |
| `versions` | The complete version dirs on disk, oldest first. |
| `agents` | The live agents on the harness: `[{"agent": "project/name", "id", "version"}]`. `version` is the version dir that the agent's launch resolved; `null` when it is not known (an agent that started before the version dirs). Absent when no agent runs. |

An agent whose `version` differs from `active` keeps its version until
its next start.

Each row also has `auto_update`: the update policy of the harness and its
last auto-update check (see "Auto-update and pins" below). The report has
`auto_update` too: `{"schedule", "next_check_at"}`. `schedule` is
`TERRARIUM_HARNESS_AUTO_UPDATE` (`daily`, `off`, or a duration such as
`6h`); `next_check_at` is `null` when the check is off.

```json
"auto_update":{"policy":"pinned","pinned_version":"0.164.0","previous":"0.165.0",
  "latest":"0.165.0","checked_at":"2026-10-31T10:05:12.000Z","result":"pinned",
  "detail":"pinned to 0.164.0; no auto-update",
  "changelog":"https://github.com/openai/codex/releases"}
```

| Field | Meaning |
|---|---|
| `policy` | `auto` (the default): the check installs each newer release on npm, majors too. `pinned`: the check installs nothing. |
| `pinned_version` | The version of a `pinned` policy; `null` for `auto`. |
| `previous` | The rollback target: the active version before the last switch, when its version dir is on disk; else the newest version dir under `active`. `null` when there is none. |
| `latest` | The latest release on npm at the last check; `null` before the first check. |
| `checked_at`, `result`, `detail` | The last check: when, the result (`current`, `updated`, `pinned`, `failed`, or `skipped`), and one line about it. `null` before the first check. |
| `changelog` | The release notes of the harness, when the daemon knows where they are; else `null`. |

`?latest=true` adds three fields to each row, from the npm registry
(`https://registry.npmjs.org/<package>`: `dist-tags.latest` and its entry
in `time`). The daemon caches each answer for 1 hour, and a failed lookup
for 5 minutes. `?latest=true&refresh=true` asks npm again.

```json
{"harness":"codex","package":"@openai/codex","base":"0.164.0","pinned":"0.164.0",
 "installed":"0.164.0","checked_at":"2026-10-26T09:00:00Z",
 "latest":{"version":"0.165.0","published_at":"2026-10-25T09:30:00Z"},
 "update":"major"}
```

| Field | Meaning |
|---|---|
| `latest` | `{version, published_at}`. `published_at` is `null` when the registry gives no time. `latest` is `null` when npm did not answer. |
| `update` | The bump from `installed` (when it is known, else `base`) to `latest`: `none`, `minor` (a patch or a minor), or `major`. `null` when `latest` is `null`. |
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
 "output":"\nadded 2 packages in 2s\n","reused":false,
 "agents_on_old_version":[{"agent":"terra/worker","id":"01K8...","version":"0.164.0"}],
 "note":"1 live agent(s) run another version of codex: terra/worker. Each keeps its version and moves to 0.165.0 at its next restart",
 "removed":["0.163.0"]}
```

| Field | Meaning |
|---|---|
| `from` | The version before; `null` when the package was not installed. |
| `to` | The version after: the `package.json` version of the new version dir. |
| `output` | npm's output: stdout, then stderr. |
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
  dir that is not active, that no live agent runs, and that the retention
  of "Auto-update and pins" (below) does not keep. It also removes the
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

A done update of a harness also records the version before it as the
`previous` version of the harness (the rollback target). When the harness
is pinned, the pin moves to the new version.

### Auto-update and pins

A daemon task checks npm for each harness on the schedule of
`TERRARIUM_HARNESS_AUTO_UPDATE` (§1.3; default `daily`). The first check is
5 minutes after the daemon start, or one interval after the last check
when that is later. Each check has a random delay of up to a tenth of the
interval (at most 30 minutes). A daemon with no tools dir has no check.

For each harness (not the extension), the policy decides:

- `auto`: when npm has a release that is newer than the active version (a
  major too), the check installs it into its version dir and switches to
  it, as an update does. Live agents keep their version; each moves to the
  new version at its next start or wake. With
  `TERRARIUM_HARNESS_UPDATE_MAIL=on`, the operator gets one mail (§6, from
  `system`) for each installed update: the harness, the old and the new
  version, the live agents on the old version, the changelog link (else
  the npm versions page), the rollback command, and the dashboard link.
  With `off` (the default), an installed update sends no mail (`mailed:
  false`); the audit entry, the `harness` event, and the dashboard table
  show it.
- `pinned`: no install. The check still asks npm, so `latest` shows the
  newest release.

A failed install keeps the active version. The operator gets one mail for
each version that failed, with both values of
`TERRARIUM_HARNESS_UPDATE_MAIL`; the next checks try again with no new mail. A
check skips a harness while a manual update runs. Each install, done or
failed, writes the audit entry `harness.auto_update` (actor `system`;
`detail`: `package`, `from`, `to`, `latest`, `ok`, `detail`),
and a done one the event `harness` with `change: "auto_updated"`.

The policy is in the state dir (the table `harness_updates`), so it stays
through a restart and a deploy. `harnesses.toml` gives the versions of a
fresh install only.

| Endpoint | Body | Answer |
|---|---|---|
| `POST /api/harnesses/check` | none | `200` `{"checked_at", "results": [{harness, result, detail, from?, to?, latest, mailed}]}`: one check now, as the task runs it. `detail` of an install is `<from> -> <to>`. `mailed` is `true` when the check sent a mail. `409 conflict` while a check runs. |
| `POST /api/harnesses/{name}/pin` | none, or `{"version"?}` | `200` `{harness, policy, pinned_version, from, to, output?, agents_on_old_version, note?, removed?}` |
| `POST /api/harnesses/{name}/unpin` | none | `200` the same object, with `policy: "auto"` and `pinned_version: null` |

A pin switches the harness to `version` for new agent starts and sets the
policy `pinned`. Without `version`, it takes `previous` (a rollback); a
harness with no previous version is `422 invalid`. A version that is not
on disk is installed first (an npm failure is `422 invalid` with
`details.output`). `version` must be semver (`latest` is `422 invalid`).
Live agents keep their version until their next start. A pin while an
update runs is `409 conflict`. An unpin sets the policy `auto`; the active
version stays until the next check.

Retention: after a switch, the GC keeps the active version, the newest two
version dirs under it, the `previous` and the `pinned_version` of each
harness, and each version that a live agent runs. It removes the rest.

All three need sysadmin or the operator. Each pin writes the audit entry
`harness.pin` (`detail`: `version`, `from`, `ok`, and `error` or
`agents_on_old_version`), and each unpin `harness.unpin` (`detail.was`:
the old pin). A done pin and an unpin also write the event `harness`
(`change`: `pinned` or `unpinned`). A daemon with no tools dir answers
`501 not_available`.

`terrarium admin harness pin <name> [version]`, `terrarium admin harness
unpin <name>`, and `terrarium admin harness check` call these endpoints.

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

Rows oldest first. Cursor: `seq`. The operator and a sysadmin read every
row. A project admin reads the rows of its project. Another agent gets
`403 forbidden`.

| Query | Meaning |
|---|---|
| `since` | Rows at or after this time (§2.1: RFC 3339 with any offset, or a duration such as `1h`). |
| `actor` | `operator`, `system`, `anonymous` (a failed login), or `agent:<ulid>`. |
| `action` | For example `agent.start`. |
| `project` | Rows of one project. A project admin gets its own project only. |
| `order` | `asc` (the default): oldest first, and the cursor goes on to newer rows. `desc`: newest first, and the cursor goes on to older rows. Another value is `400`. |

New and changed rows also come live, as `audit` frames on the firehose
stream with `audit=1` (§9.1). The same read rule applies.

A row of an agent actor can have `session_id` and `session_label`: the
actor's session at `at`, when the reader may read that session (§9,
"Session links").

```json
[{"id":"01M5PA0S0RK019JM7AD46MKF6B","seq":1,"at":"2026-10-24T06:36:53.144Z","actor":"system",
  "action":"project.create","target":"terrarium","project":"terrarium",
  "detail":{"card_prefix":"TER","source":"file"}}]
```

`detail` is a JSON object. Actions: `project.create`, `project.update`,
`project.delete`, `project.detach`, `repo.attach`, `agent.create`,
`agent.update`, `agent.delete`, `agent.roles`, `agent.start`, `agent.stop`,
`agent.restart`, `agent.send`, `agent.send_raw` (`bytes`, `first_word`;
never the text), `agent.keys` (`keys`: the key names; a text key as
`text(<length>)`), `agent.interrupt` (`harness`), `agent.answer`, `agent.attach`,
`repo.checkout`, `repo.credential` (each answer of the credential helper),
`secret.put`, `secret.delete`, `secret.reveal`, `secret.grant`,
`secret.revoke`, `secret.get`, `secret.env`, `secret.use`, `secret.migrated` (actor
`system`), and `secret.rotate_key` (actor `system`) (§5.2),
`mail.read` (the first read of a message; `detail` has `thread`, `from`,
`to`, and `via`: `read` or `reply`), `board.columns`, `card.reassign`, `card.reopen`, `card.archive` (by an admin
on a card that is not its own), `card.file_add`, `card.file_replace`, `card.file_remove` (§8.4), `definitions.sync`, `memory.edit` and
`memory.delete` (§10), `wiki.delete`, `wiki.move`, `wiki.undelete` (§11),
`artifact.delete`, `artifact.prune_warning`, `artifact.prune` (§12),
`version.approve`, `version.reject`, `version.release` (§13; old rows
only, TER-58 removed the versions),
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

Projects may opt into automatic reviews with `review_dispatcher: "<project-admin>"`
in a project file, fleet project, or project PATCH. The default is off, and
`"off"` disables it. This is independent of ready-card dispatch. The dispatcher
uses the project's `card-review` template, falling back to `global:card-review`,
and that admin's spawn rights, caps, and template grants.

Each card in review gets at most one review agent at a time. `review_round`
on the card counts successful review spawns. A card of a project without a
review dispatcher counts each move into its review column instead (TER-614),
so a review by the lead counts too; `0` means it was never in review.
`review_round` is in `GET /api/cards/{id}` and in each card of the board list.
`trouble` on a card holds its active trouble signals (TER-79): the trouble
pass sets them while the card is in a working or review column, and clears
them when it leaves, so a done card shows `[]`. Only the current review agent's
comment can supply a verdict: its first line must be `Review verdict: accept` or
`Review verdict: changes` (bare `accept` and `changes` also work), with findings below it.
At three rounds, automatic reviews stop and the owner is notified.
If a verdict move fails, the findings stay on the card and the owner is notified.
Accept moves to `merge_queue`, never
merges. Changes moves to the top of `ready`, carrying the findings and
waking the bound implementation agent through the normal card-move flow.
The `review:lead` label retains both decisions for the lead without an
automatic move. Moving the card invalidates the previous round's verdict.

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
| `PATCH /api/projects/{name}` | `{"description"?, "card_prefix"?, "idle_alert"?, "github_authors"?}` | `200` project | project_admin; `github_authors`: the operator |
| `DELETE /api/projects/{name}?confirm=true` | | `204` | sysadmin |
| `GET /api/projects/{name}/agents` | | `200` agents by ID | read |
| `GET /api/projects/{name}/targets` | | `200` targets, the project's and the global ones | read |
| `GET /api/projects/{name}/counts` | | `200` `{"blocked_cards", "questions"}`, below | read |
| `GET /api/projects/{name}/target-grants` | | `200` the global targets granted to the project for its project-owned monitors and schedules (§30.7) | read |
| `POST /api/projects/{name}/target-grants` | `{target, for}` | `201` the grant | sysadmin |
| `DELETE /api/projects/{name}/target-grants/{target}/{for}` | | `204` | sysadmin |
| `GET /api/projects/{name}/repos` | | `200` repos by name | read |
| `POST /api/projects/{name}/repos` | repo, below | `201` repo | project_admin |

Names are 1 to 63 of `a-z`, `0-9`, `-`, `_`, and start with a letter or a
digit. `card_prefix` is 1 to 10 of `A-Z` and `0-9`, and starts with a
letter. The prefix cannot change after the first card (`conflict`).

`GET /api/projects/{name}/counts` gives the counts of the project tabs of
the dashboard (TER-154). `blocked_cards` counts the cards in the columns with
the `blocked` role, archived cards not counted. `questions` is `{"open",
"urgent", "critical"}`: the open questions, bundles, and tool run approvals
of the project, also those in a thread (a notice needs no decision).
`urgent` counts the urgent and the critical ones, `critical` only the
critical ones. `questions` is `null` for an actor that may not list the
questions of the project (`GET /api/questions?project=`: the operator, a
sysadmin or architect, or a project_admin of the project). The counts use
indexes and read no card or question rows, so the dashboard reads them again
on each `card` and `question` event of the project. An unknown project is
`404`.

`idle_alert` is the threshold of the idle alert (§6.2, "Idle alert"): a
duration (`15m`, `1h`), or `off` (also `0`). `null` sets the default
(15m), and the project then shows no `idle_alert`. Another value is
`invalid`. A file project cannot set it yet.

`github_authors` is the allow list of the GitHub author gate (§8.3,
"GitHub author gate"): the GitHub logins whose issues count as work. The
`PATCH` replaces the whole list; `[]` ends the gate. Only the operator
sets it (`403` for every agent, a sysadmin too). A login is 1 to 39 of
`a-z`, `A-Z`, `0-9`, `-`, and does not start with `-`; a leading `@` is
dropped, and a copy in another case is dropped. Another value is
`invalid`. A file project sets it with the key `github_authors` in
`projects/<name>.toml`, and its `PATCH` is `file_managed` as for every
other field. A fleet file cannot set it: the key in `[[projects]]` is an
error. The project shows the list, and no `github_authors` when it is
empty.

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

`credential` is the name of a secret (§5.2), never the secret. The daemon
uses the project's secret of that name, else the global one. A global
secret needs a grant to the project, unless the operator or a sysadmin
attaches the repo (§5.2, "A global secret on a project object"). Else
the attach is `403 forbidden`. `credential_admin` in the answer is `true`
when the operator, a sysadmin, or a project file attached the repo.
`branch` is an alias of `default_branch`. A body with the two is `400
bad_request`. A body with an unknown field is `400 bad_request`, and the
message names the fields.

`hooks_dir` (optional) is a tracked dir of the repo, for example
`.githooks`: a path relative to the repo root, with no `.`, `..`, or `.git`
part. `push_deny` (optional) is a list of at most 64 branch globs, for
example `["op/*"]`: letters, digits, `.`, `_`, `-`, `/`, `+`, `@`, `*`
(any text, `/` too), and `?`, with no `refs/heads/`. A bad value is `422
invalid`. Both are in the answer only when they are set. See §5.1, "Git
hooks". `push_via` (optional) is `proxy` or `direct`: how the agent
checkouts reach the remote (§5.1, "The git proxy"). Without it, the daemon
picks `proxy` for an `https` repo with a `credential` and a `push_deny`
list (when the proxy is on), and `direct` for every other repo. `proxy`
needs an `http` or `https` URL with no user, password, query, or fragment
(`422 invalid`). Another value is `400 bad_request`. It is in the answer
only when it is set. The daemon trims spaces at the ends of `url`. The
answer of an attach, `GET /api/projects/{p}/repos`, and `GET
/api/repos/{id}` also have `via` (`proxy`, `direct`, or `direct (<why
push_deny is only the hook>)`) and `warnings` (a `push_deny` that is only
the hook, and a secret that a proxied repo shares with a direct repo).
There is no edit of a repo: detach it and attach it
again, or change the project file.

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
calls `POST /api/repos/credential` through the runner socket. For a repo
that goes through the git proxy (below), `origin` is the proxy, and the
helper gives the agent's proxy credential, not the forge token.

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/repos/{id}` | | `200` repo | read |
| `DELETE /api/repos/{id}` | | `200` the removed repo; `409` while a queued or running job, `next_repos` of the delivery config, a `github` pipeline of the same `owner/name`, or an open bound agent's checkout at the repo's `path` uses it; `409 file_managed` for a repo of a project file. The clones stay. Audit and event `repo.delete` | project_admin |
| `POST /api/repos/{id}/checkout` | below | `200` checkout result | own workspace: read; another agent's: project_admin |
| `POST /api/repos/credential` | `{"protocol", "host", "path"?}` | `200` `{"username", "password"}` | an agent |
| `GET /api/repos/{id}/trees...` | | the repo browser (§5.3) | read |
| `GET /api/credentials` | | `200` `[{"name", "kind", "username"?, "created_at"}]` | sysadmin (an alias, §5.2) |
| `POST /api/credentials` | `{"name", "kind", "value", "username"?}` | `201` credential, without `value` | sysadmin (an alias, §5.2) |
| `DELETE /api/credentials/{name}` | | `204` | sysadmin (an alias, §5.2) |

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
`push_via` is `proxy` or `direct` (below, "The git proxy").
`hooks` lists the hooks that the checkout wrote into `.git/hooks` (below).
It is absent when there are none.

**Git hooks.** git never copies hooks on a clone. So for a repo with
`hooks_dir` or `push_deny`, each clone and each update in a workspace
(not on a target) writes hooks into `.git/hooks` of the checkout:

- `hooks_dir`: each regular file directly in the dir at `HEAD` (mode
  `100644` or `100755`, a name of letters, digits, `-`, `_`, `.`, not
  starting with `.`) goes to `.git/hooks/<name>` with mode 0755. The daemon
  reads the files from the commit (`git ls-tree`, `git cat-file`), not from
  the work tree, so a symlink is never followed. A symlink, a subdir, or a
  file over 1 MiB in the dir is skipped or refused. More than 64 files is
  `409 conflict`. A `HEAD` with no such dir copies nothing. A hook that is
  in `.git/hooks` and not in the dir stays.
- `push_deny`: the daemon writes its own `pre-push`. It refuses a push when
  the local branch or the remote branch matches a glob, with the message
  `pre-push: the branch <b> matches push_deny of this repo`. The repo's own
  `pre-push` from `hooks_dir` becomes `pre-push.repo`, and runs after the
  check with the same input. A repo attached again without `push_deny`
  keeps the old `pre-push` in a checkout until the agent removes it.
- The hooks run for the agent's git commands in the sandbox. The daemon's
  own git commands run with `core.hooksPath=/dev/null` and never run them.
  `core.hooksPath` in the repo config stays refused (`409`): `hooks_dir`
  is the supported way, and the refusal keeps the daemon's second line of
  defence if a future git command of the daemon forgets the override.
- A client-side hook is a guard against mistakes, not a hard control. The
  agent can push with `git push --no-verify` or edit `.git/hooks`, and the
  next checkout by the daemon writes the hooks again. The hard rule is the
  git proxy (below): for a proxied repo, the proxy refuses the push of a
  matching branch, and the agent has no token to push past it. The hook
  stays as the first, friendly check. A `direct` repo has only the hook
  (an SSH repo, a local repo, or `push_via = direct`); for a hard rule
  there, use a branch protection rule on the git server.

**The git proxy.** The daemon serves a smart HTTP git proxy on its own
listener, `TERRARIUM_GIT_PROXY_LISTEN` (default `127.0.0.1:8084`, loopback:
the sandboxes share the daemon's network; empty turns it off). A repo goes
through it when its `push_via` is `proxy`, or when `push_via` is not set
and the repo has an `https` URL, a `credential`, and a `push_deny` list.
For such a repo:

- A checkout in a workspace sets `origin` (fetch and push) to
  `http://127.0.0.1:8084/git/<project>/<repo>.git`, and removes a
  `pushurl` of `origin`. The daemon's own clone and fetch still go to the
  stored URL with the token, outside the sandbox. A repo that goes back to
  `direct` gets the stored URL in `origin` again at its next checkout.
  With `push_via = proxy` and the proxy off, a checkout is `409 conflict`.
- The credential helper gives the proxy host (`http`, `127.0.0.1:8084`)
  the agent's own proxy credential: user `terrarium`, and a token that is
  an HMAC of the agent ULID (key `<state>/git-proxy.key`, made at the first
  start, mode 0600). It never gives the forge token of a proxied repo: a
  question for the forge URL of the repo is `409 conflict` with a hint to
  run `terrarium repo checkout <repo>` (a checkout from before the proxy
  still names the forge). So the token is "use only".
- "Use only" is a property of the **secret**: each secret that a proxied
  repo names (in any project) is use only everywhere. The helper refuses
  it for every other repo too (`409 conflict`), a grant of it gives no
  helper answer (`409`), no `terrarium secret get` (`409`), and no env
  var at the start (a warning event). Else a direct repo with the same
  token would open the proxied repo past the proxy. Give the other repos
  their own token, or let them go through the proxy. The attach answer
  warns when a secret is shared this way.
- An update of a proxied checkout fetches from the daemon's mirror of the
  repo (§5.3), with no credential: the token never goes to git in a repo
  that the agent controls. The first clone still runs with the token.
- The proxy serves `GET .../info/refs?service=git-upload-pack|git-receive-pack`,
  `POST .../git-upload-pack`, and `POST .../git-receive-pack`, and nothing
  else (no dumb HTTP, `404`). No credential or a bad one is `401` with a
  Basic challenge. An agent of another project is `403`. A repo that is
  not there, or not proxied, is `404`.
- A push: the proxy reads the ref commands. When a pushed branch
  (`refs/heads/<b>`, a delete too), or the branch `<b>` of a review ref
  `refs/for/<b>[%...]` (Gerrit `%submit` lands on `<b>`), matches
  `push_deny`, it refuses the whole push and changes no ref. A repo with
  `push_deny` also refuses each ref outside `refs/heads/`, `refs/tags/`,
  and `refs/for/` (`refs/meta/config`, `refs/drafts/...`). Push options
  (`git push -o`, GitLab `merge_request.*`) are not advertised and are
  refused (`400`): some forges change a branch from them. git shows `! [remote rejected] <ref>
  (denied by push_deny `<glob>` of repo <repo>)` for each denied ref, and
  `not pushed: another ref of this push is denied` for the others. Else it
  forwards the push to the forge with the repo's token, and streams the
  answer back. A signed push (`--signed`), a ref name outside `refs/`, and
  a compressed push body are refused. Each push with refs writes a
  `repo.push` audit row (`detail.result`: `forwarded` or `refused`, with
  the refs).
- A fetch or a clone is forwarded as it is (protocol v2 too).
- Limits: a push body of 2 GiB, a fetch request of 256 MiB, an answer of
  8 GiB; 30 s to connect, 5 min with no bytes from the forge, 60 s with
  no bytes from the agent, 1 h for one request; 32 requests at one time,
  4 of one agent (`503`). The proxy follows no
  redirect. It connects with the address rules of the webhooks
  (`TERRARIUM_WEBHOOK_PRIVATE`, one DNS answer for each request, pinned).
  An error of the forge is `502` with a text that git shows; it never
  holds the token.
- Not covered: SSH repos and local repos (the hook only), Git LFS (its API
  is not proxied: set `push_via = direct` for an LFS repo), and the
  checkouts on targets. A grant of a token for the forge host gives the
  agent that token through the helper (§5.2), and a token with push rights
  undoes the hard rule.

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
secret of kind `token` or `github_token` (the project's secret of that
name, else the global one). A global secret needs a grant to the
project, or a repo that the operator or a sysadmin attached (§5.2, "A
global secret on a project object"). Else the daemon refuses it: the
answer is `404`, and a `secret.refused` event records it. When no repo matches, the daemon answers for
an `https` host of a `token` or `github_token` secret that is granted to
the agent (§5.2, "Grants"); git does not need to send the path for it.
Every other case is `404 not_found`, so git falls back to its next
helper. An SSH key is never answered. Each answer for a repo writes a
`repo.credential` audit row, and each answer for a grant writes a
`secret.use` audit row (without the secret). An agent gets at most 30
answers a minute (`429 rate_limited`).

The answer holds the token in plain text, because git needs it. So an
agent that can call the helper can read the token, also with a `use`
grant (§5.2, "Grants": `use` is `read` for tokens). The exceptions are the
repos that go through the git proxy (§5.1, "The git proxy"): for the host
of the proxy, the helper answers the agent's proxy credential (not
audited, in the same rate limit), and for the forge URL of a proxied repo
it answers `409 conflict`, so their token stays in the daemon.

```
POST /api/repos/credential          (agent)
{"protocol":"https","host":"github.com","path":"wiltaylor/infra.git"}

200
{"username":"x-access-token","password":"ghp_..."}
```

For one clone or fetch, the daemon puts the secret in a 0700 run dir under
`<state>/credentials/.run/`, and removes the dir after the run. A token goes
to git through a one-shot helper script that answers only for the repo's
protocol and host. An SSH key goes to `ssh -i` through `GIT_SSH_COMMAND`.
A key with a passphrase goes there without its passphrase, in the 0600
file of the run only. The secret is never on a command line, in an env
variable, or in the repo's `.git/config`.

### 5.2 Secrets

PLAN §16b. The secret manager holds the API keys, tokens, logins, SSH keys,
and custom secrets that the daemon and the agents use. The code is in
`crates/terrariumd/src/secrets/`, `api/secrets.rs`, and
`api/secret_grants.rs`. An agent gets a secret only through a grant (see
"Grants" below).

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/secrets?scope=` | | `200` `[secret]` | below |
| `POST /api/secrets` | `{"scope"?: "global", "name", "kind", "fields", "secret_fields"?}` | `201` secret; `409` when the name exists | below |
| `GET /api/secrets/{scope}/{name}` | | `200` secret | below |
| `PUT /api/secrets/{scope}/{name}` | `{"kind", "fields", "secret_fields"?}` | `201` new, `200` replaced | below |
| `DELETE /api/secrets/{scope}/{name}?force=` | | `204`; `409` when a target uses it | below |
| `POST /api/secrets/{scope}/{name}/generate` | `{"type"?: "ed25519" \| "rsa", "bits"?: 3072 \| 4096, "comment"?, "replace"?: false}` | `201` secret with `public_key` and `fingerprint`; `200` replaced | below |
| `POST /api/secrets/{scope}/{name}/reveal` | `{"password"}` | `200` secret with `values` | the operator |
| `GET /api/secrets/{scope}/{name}/grants` | | `200` `[grant]` | below |
| `POST /api/secrets/{scope}/{name}/grants` | `{"grantee", "level"?, "env"?, "targets"?}` | `201` new, `200` replaced | below |
| `DELETE /api/secrets/{scope}/{name}/grants?grantee=` | | `204` | below |
| `GET /api/agent/secrets` | | `200` `[granted]` | an agent |
| `POST /api/agent/secrets/{name}/get` | `{"field"?}` or none | `200` `{scope, name, kind, field, value}` | an agent with a `read` grant |

**Scope.** `scope` is `global` or a project name. A global secret needs a
sysadmin or the operator. A project secret needs a project_admin of that
project, a sysadmin, or the operator. A member gets `403`. A list with no
`?scope=` shows each secret for the operator and a sysadmin, and the
secrets of its own project for a project_admin. A scope that is not
`global` and not a project is `404`. No project can have the name
`global`: a create is `422`. A project named `global` from before has no
project secrets, and a list by its project_admin is `403`. A project
delete deletes the secrets of the project.

The name is unique in its scope (1-63 of `a-z`, `0-9`, `-`, `_`). A
repo, a target, or the config repo remote names a secret by its name in
`credential`. The daemon uses the secret of the owner's project first,
then the global secret of the same name. So a project secret hides a
global secret of the same name for that project. A global target and the
config repo remote use global secrets only.

**A global secret on a project object** (PLAN §16b.6a). A project repo
or a project target can name a global secret only in one of these cases:

- The project holds a grant of the secret: a `project:<name>` grant, at
  any level.
- The operator, a sysadmin, or a definition file (a project file, a
  target file, a fleet file in `fleets/`) set the object.

The daemon checks this when it saves the object: an attach of a repo,
`POST` and `PATCH /api/targets`, a fleet apply, and a config import. A
refusal is `403 forbidden`. The message names the rule and the fix:
`terrarium secret grant <name> project:<project>`. A name that finds no
secret passes, and a project secret of the same name needs no grant.

The daemon checks again at each use: the credential helper answer for a
repo, a checkout, an ssh exec, a display login, and a target test. So a
revoke of the project grant stops an object that a project_admin set. The
use is refused (`403`; the credential helper answers `404`), and the
daemon emits a `secret.refused` event on the project with `data: {level:
"warning", scope, name, object, message}`. The target view shows the
refusal in `credential_info.warning`.

The repo and the target rows record who set them in `credential_admin`.
A `PATCH` by a project_admin clears it, unless the patch changes only
`keep` and `snapshot_before_task`. So a project_admin cannot move the
login of an operator's target to another address. The repos and targets
from before this rule count as set by an admin: the daemon cannot know
who made them, and an upgrade does not break them. Check the old project
repos and targets that name a global secret, and grant the secret or
change the object.

**Kinds.** `fields` holds all the fields of the kind. The secret fields
are in bold.

| Kind | Fields | Rules |
|---|---|---|
| `api_key` | **`value`**, `env`? | `value` is one line, trimmed. `env` is an env var name. |
| `token` | `username`?, **`value`**, `hosts`? | `username` defaults to `x-access-token`. `value` is one line, trimmed. `hosts` is a list of git host names (or a comma list). |
| `github_token` | **`value`**, `host`? | `host` defaults to `github.com`. |
| `login` | `username`?, **`password`**, `url`?, `domain`? | The password is one line of 1 to 1024 bytes. The daemon keeps its spaces. |
| `ssh_key` | **`private_key`**, **`passphrase`**?, `public_key`, `fingerprint` | The daemon derives `public_key` (the `authorized_keys` line) and `fingerprint` (`SHA256:<base64>`, as `ssh-keygen -l` shows it); a body cannot set them. |
| `generic` | any | `secret_fields` names the secret fields. The other fields are plain. At most 32 fields. |

A plain field is 1 to 1024 bytes with no control characters. A secret
field is 1 to 64 KiB. A field that the kind does not have is `422
invalid`. A bad value is `422 invalid`, and the message never holds the
value.

**Values that redaction cannot use** (PLAN §16b.6). A put (`POST`, `PUT`,
`/api/credentials`) checks each secret field, and each line of 8 bytes or
more of it:

- A value that looks like an identifier is `422 invalid`, for each kind:
  a ULID, an actor label (`agent:<ULID>`), a word of the records
  (`operator`, `sysadmin`, `grantee_label`, a kind or field name), or a
  name of this instance (a project, an agent, `project/agent`, a repo, a
  host, a target, a subscription, a schedule, a template, an event
  source, a secret).
- A weak value is shorter than 16 bytes, and is one lower-case word,
  name, or number (`a-z`, `0-9`, `._:/-`), or one word of letters only,
  or has less than 28 bits of entropy. A weak `value` of an `api_key`, a
  `token`, or a `github_token` is `422 invalid`. A weak `password` of a
  `login`, `passphrase` of an `ssh_key`, or field of a `generic` is
  stored. The answer then has `warnings` (a list of texts), and the
  daemon does not redact this value.

```
PUT /api/secrets/ops/win
{"kind":"login","fields":{"username":"bob","password":"summer2024"}}

201
{...,"name":"win","kind":"login",...,"warnings":["the value of `password` is weak (it is a lower-case word, name, or number): it is stored, but redaction does not hide it in records, because a weak value matches ordinary text"]}
```

An `ssh_key` is a private key in the OpenSSH format (with or without a
passphrase), or in PEM with no passphrase: PKCS#1 RSA, PKCS#8 (RSA,
Ed25519, P-256), or SEC1 P-256. A PEM key with a passphrase is `422`:
write it again in the OpenSSH format with `ssh-keygen -p -f <file>`. A
wrong passphrase is `422`.

**The secret shape.** No answer holds a secret field, except a reveal.

```json
{"id":"01MB2V8Q4X0K16V6G88P3SE78W","scope":"global","name":"github-deploy","kind":"token",
 "fields":{"username":"x-access-token","hosts":["github.com"]},"secret_fields":["value"],
 "has_value":true,
 "created_at":"2026-10-28T09:00:00.000Z","updated_at":"2026-10-28T09:00:00.000Z",
 "created_by":"operator"}
```

`fields` holds the plain fields. `secret_fields` holds the names of the
secret fields that the secret has. `created_by` is the actor label.

`has_value` is `false` for a placeholder: a secret that a config repo
import made with no value (§25.3, PLAN §16b.5). A placeholder has its
name, kind, plain fields, and grants. The dashboard marks it, and
`terrarium secret list|show` shows "no value". A reveal, a `secret get`
of a secret field, and a target login with it are `422 invalid` ("has no
value yet"). A grant of a placeholder gives nothing: the brokers skip it,
the start sets no env var for it, and the start emits a warning event. A
target with a placeholder login saves, and its view shows a
`credential.warning`. A `PUT` with the value makes it a normal secret.
The granted shape of `GET /api/agent/secrets` has `has_value` too.

```
POST /api/secrets
{"scope":"ops","name":"door","kind":"generic","fields":{"user":"bob","pin":"1234"},"secret_fields":["pin"]}

201
{"id":"01MB2V9...","scope":"ops","name":"door","kind":"generic","fields":{"user":"bob"},"secret_fields":["pin"],...}
```

**Reveal.** `POST /api/secrets/{scope}/{name}/reveal` is for the operator
only (a sysadmin agent gets `403`). The body must hold the operator
password. The daemon checks it as a login checks it (§1.1): a wrong
password is `403`, and the login limiter counts it (`429` after too many).
With no password set, the answer is `409`. The answer is the secret shape
with `values` (the secret fields by name), and `Cache-Control: no-store`.
Each reveal writes a `secret.reveal` audit row.

```
POST /api/secrets/global/github-deploy/reveal
{"password":"..."}

200
{"id":"...","scope":"global","name":"github-deploy","kind":"token",...,"values":{"value":"ghp_..."}}
```

**Generate.** `POST /api/secrets/{scope}/{name}/generate` makes a new
SSH key pair as an `ssh_key` secret (PLAN §16b.6a). The rights are the
rights of a create. The daemon makes the key from the OS random source.
The private key never leaves the daemon, and it has no passphrase (the
store seals it). `type` is `ed25519` (the default) or `rsa`. `bits` is for
`rsa` only: 3072 (the default) or 4096. Another value is `422`.
`comment` goes at the end of the public key; the default is
`terrarium:<scope>/<name>`. A name that exists is `409`, unless
`"replace": true`; then the old key is gone and the answer is `200`. An
empty body makes an ed25519 key.

```
POST /api/secrets/global/deploy/generate
{}

201
{"id":"...","scope":"global","name":"deploy","kind":"ssh_key",
 "fields":{"public_key":"ssh-ed25519 AAAAC3Nza... terrarium:global/deploy",
           "fingerprint":"SHA256:3Qm4..."},"secret_fields":["private_key"],...}
```

Add the `public_key` line to `~/.ssh/authorized_keys` of the user on the
target. A target uses the key through its `credential` (§17.2).

**The login of a target.** A target names a secret in `credential`
(§17.2). A delete of a secret that a target uses is `409 conflict`. The
message and `details.targets` name the targets. With `?force=true`, the
daemon deletes the secret, and each target keeps a `credential` with no
secret. The view of such a target shows a warning, and an exec on it
fails until the operator stores a secret of that name again. A target of
project `p` uses a global secret only when `p` has no secret of the same
name, and only with a grant to `p` or an admin who set the target (see
"A global secret on a project object" above).

**Grants.** A grant gives one secret to an agent or to a whole project
(PLAN §16b.3). A grant to a project applies to each agent of the project,
also to the agents that start later.

| Level | The agent gets |
|---|---|
| `use` | The brokers only: the git credential helper and the ssh-agent socket. The agent cannot see an `ssh_key`. For a `token` or a `github_token`, `use` is `read`: see below. |
| `run` | `use`, and the value in the env of `terrarium run -e KEY=@secret:<name>` on the targets that the grant names (TER-142, "Secrets in a run" below). No env var at the start, no `terrarium secret get`. |
| `read` | `run` on any target that the agent may use, the env var at the start, and `terrarium secret get`. |

The levels are ordered `use` < `run` < `read`. An agent's own grant and
its project's grant add up to the higher level; two `run` grants add up
to the union of their targets, and a `read` grant has no target limit.

**`use` is `read` for tokens.** The git credential helper hands the token
to git in plain text (§5.1). The agent runs git, so it can call the helper
(`terrarium repo credential get`) and read the token. So give a `token` or
a `github_token` only to an agent that may read it. Only an `ssh_key`
stays hidden under `use`: the ssh-agent socket signs and never gives the
key out. This is intended behaviour (2026-10-29). The token of a repo that
goes through the git proxy (§5.1) is the exception: the proxy adds it, and
the helper never gives it out. A grant of the same token still gives it.

| Secret | Grant to | Who |
|---|---|---|
| global | any agent or project | a sysadmin, the operator |
| project | an agent of that project, or that project | a project_admin of the project, a sysadmin, the operator |

`grantee` is `agent:<project>/<name>`, `agent:<id>`, or
`project:<name>`. The daemon stores an agent by its ID. A project
secret to another project, or to an agent of another project, is `403`.
An unknown agent or project is `404`. A second grant to the same grantee
replaces the first.

`level` defaults to `use` for `token`, `github_token`, and `ssh_key` (the
kinds that a broker serves). It defaults to `read` for `api_key`,
`login`, and `generic`, because no broker serves them. `env` names an env
var for the value. An env var puts the value into the agent's
environment, where the agent can print it, so `env` needs the level
`read` (`422` otherwise).

`targets` names the targets of a `run` grant: 1 to 32 target names, each
one a target that exists (`404` otherwise) and, for a project target, a
target of the grantee's project (`403`). A `run` grant with no target, or
`targets` on another level, is `422`. The answer and the list show
`targets` on a `run` grant, sorted. A grant check does not need the
grantee to hold the target in its policy: the run still checks the
agent's target rights (§17).

```
POST /api/secrets/ops/deploy/grants
{"grantee":"agent:ops/builder","level":"run","targets":["prod-1","prod-2"]}
```

*The name rule.* Each final env var name must obey this rule. The final
names are the `env` name, and each `<ENV>_<FIELD>` of a `generic` secret
with more than one secret field (below). A name that breaks the rule is
`422`.

- The name matches `^[A-Z][A-Z0-9_]{0,127}$`: an upper-case letter, then
  upper-case letters, digits, or `_`, at most 128 characters. A lower-case
  name and a name that starts with `_` are refused.
- The name is not in the deny set. The deny set holds the vars that the
  loader, a shell, the sandbox, a language runtime, a proxy, or a harness
  reads or sets:

| Group | Refused names |
|---|---|
| Loader and C library | `LD_*`, `MALLOC_*`, `GCONV_PATH`, `GLIBC_TUNABLES`, `LOCPATH`, `NLSPATH`, `HOSTALIASES`, `RES_OPTIONS`, `LOCALDOMAIN`, `TMPDIR` |
| Shell | `BASH_*` (also `BASH_ENV`, `BASH_FUNC_*`), `ENV`, `SHELLOPTS`, `BASHOPTS`, `PS1`, `PS2`, `PS4`, `PROMPT_COMMAND`, `IFS`, `CDPATH`, `GLOBIGNORE`, `HISTFILE`, `INPUTRC`, `POSIXLY_CORRECT` |
| Session and sandbox | `HOME`, `PATH`, `USER`, `LOGNAME`, `SHELL`, `TERM`, `TERMINFO`, `TERMINFO_DIRS`, `TERMCAP`, `LANG`, `LANGUAGE`, `LC_*`, `TZ`, `TZDIR`, `PWD`, `OLDPWD`, `DISPLAY`, `WAYLAND_*`, `MAIL`, `MAILPATH`, `XDG_*`, `SSH_*`, `TERRARIUM_*` |
| git | `GIT_*` (all) |
| Programs that other programs start | `EDITOR`, `VISUAL`, `PAGER`, `MANPAGER`, `LESSOPEN`, `LESSCLOSE`, `BROWSER` |
| Language runtimes | `NODE_*`, `NPM_CONFIG_*`, `YARN_*`, `BUN_*`, `DENO_*`, `PYTHON*`, `PIP_*`, `UV_*`, `CONDA_*`, `VIRTUAL_ENV`, `PERL5*`, `PERLLIB`, `RUBY*`, `GEM_*`, `BUNDLE_*`, `JAVA_*`, `JDK_JAVA_OPTIONS`, `CLASSPATH`, `DOTNET_*`, `COREHOST_*`, `GOFLAGS`, `GOPATH`, `GOROOT`, `GOPROXY`, `GOENV`, `GOTOOLCHAIN`, `GOINSECURE`, `GOPRIVATE`, `GONOSUMDB`, `GONOPROXY`, `CGO_*`, `CARGO_*`, `RUSTC*`, `RUSTDOC*`, `RUSTFLAGS`, `RUSTUP_*`, `LUA_*`, `TCLLIBPATH`, `PHPRC` (`_JAVA_OPTIONS` breaks the shape) |
| OpenSSL, Kerberos and D-Bus (ssh and the git and docker credential helpers load them) | `OPENSSL_*` (`OPENSSL_CONF`, `OPENSSL_MODULES`, `OPENSSL_ENGINES`), `KRB5*` (`KRB5_CONFIG`, `KRB5CCNAME`), `DBUS_*` (`DBUS_SESSION_BUS_ADDRESS`) |
| Proxies and CA files | `*_PROXY` (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, ...; a lower-case `http_proxy` breaks the shape), `NO_PROXY`, `SSL_CERT_*`, `CURL_CA_BUNDLE`, `CURL_HOME`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, `SSLKEYLOGFILE`, `WGETRC`, `GNUPGHOME` |
| Harnesses and their accounts | `ANTHROPIC_*`, `CLAUDE_*`, `OPENAI_*`, `CODEX_*`, `COPILOT_*`, `PI_*`, `DISABLE_*`, `IS_SANDBOX`, `ENABLE_CLAUDEAI_MCP_SERVERS`, the provider keys of pi (`GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `XAI_API_KEY`, `GROQ_API_KEY`, `DEEPSEEK_API_KEY`, `MISTRAL_API_KEY`, `ZAI_API_KEY`, `KIMI_API_KEY`, `MOONSHOT_API_KEY`), and the harness home vars |

`GH_TOKEN` (the default of a `github_token`) and `GITHUB_TOKEN` stay
allowed. The daemon checks the rule in three places: when the grants API
writes a grant, when a config import writes one (§25.3), and again at
each start for each final name, just before the name goes into the
launch env. A grant row from an older daemon with a name that the rule
refuses now gives nothing at the start: the daemon sets no var for it and
emits a warning event. The start does not fail.

```
POST /api/secrets/ops/db/grants
{"grantee":"agent:ops/builder","level":"read","env":"DATABASE_URL"}

201
{"scope":"ops","name":"db","grantee":"agent:01MB2V...","grantee_label":"agent:ops/builder",
 "level":"read","env":"DATABASE_URL","created_by":"operator","created_at":"..."}
```

The delete takes the grantee in the query (`?grantee=agent:ops/builder`).
A grant that does not exist is `404`. The list shows each grant of the
secret. A grant change applies at the next start of the agent. The
brokers, `get`, and `terrarium run` check the grants at each call, so a
revoke stops them at once, and a `run` grant applies at the next run with
no restart. A delete of the secret, of the agent, or of the project removes
its grants. A new agent with the name of a removed agent gets no grant.

**Delivery to agents.** PLAN §16b.4.

- *Env vars.* At each start, the harness gets each secret with a `read`
  grant and an `env` name in its environment. The agent's own grant gives
  the name first, then the project grant. A `github_token` with a `read`
  grant and no `env` name sets `GH_TOKEN`. The value is the main secret
  field: `value`, `password` (`login`), or `private_key` (`ssh_key`). A
  `generic` secret with one secret field sets that field. A `generic`
  secret with more fields sets `<ENV>_<FIELD>` for each (upper case, `_`
  for other characters). Each final name obeys the name rule (above). A
  var that the harness or its account sets already wins, and the daemon
  shows a warning event. No value goes into the start log, an event, or an
  audit row.

  The values do not go through the launch spec. The sandbox starts the
  harness through `terrarium exec-env -- <harness> <args>`, with the CLI
  that the sandbox binds. Inside the sandbox, `exec-env` asks the daemon
  for the vars (`POST /api/agent/secrets/env`, below), adds each var that
  the env does not hold yet, and replaces itself with the harness. So no
  value is in the runner spec file (`private/runner.json`), on a command
  line, or in the env of the `bwrap` process outside the sandbox. A
  revoke applies at the next start. When the daemon does not answer,
  `exec-env` prints a warning and starts the harness with no secret var.
  With no `terrarium` CLI (`--cli`), the start sets no secret var and
  emits a warning event.
- *Harness credentials.* The stored API key of the agent's subscription
  (PLAN §16.1) and the token of a Copilot plan account (PLAN §16.3) take
  the same path. The launch spec holds their names only. `exec-env` sets
  them over a var of the same name (the harness's or the account's). A
  grant with the name of a credential var gives nothing. The sandbox then
  starts `terrarium exec-env --require-credentials`: when the daemon does
  not answer after 5 tries (about 3 s), `exec-env` exits 78 and the
  harness does not start. The status reason of the exit names the cause.
  With no `terrarium` CLI, such a start is `422`.
- *Git hosts.* The start sets `credential.https://<host>.helper = !terrarium
  repo credential` (through `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, and
  `GIT_CONFIG_VALUE_<n>`) for each host of a granted `token` (`hosts`) or
  `github_token` (`host`), at any level. The credential helper (§5.1) then
  answers for the host.
- *ssh-agent.* An agent with at least one `ssh_key` grant (any level) gets
  an ssh-agent socket. The daemon serves it at
  `<runtime>/ssh-agent/<agent id>/agent.sock` (the dirs 0700, the socket
  0600). The sandbox binds the dir read-only at `/run/terrarium/ssh-agent`
  (a client can connect to a socket in a read-only bind), and
  `SSH_AUTH_SOCK` is `/run/terrarium/ssh-agent.sock` (a symlink to the
  socket). With the sandbox mode `none`, `SSH_AUTH_SOCK` is the host path.
  The socket lists and signs with the granted keys only, and refuses add,
  remove, and lock requests. It checks the grants at each list and each
  sign. The key comment is `<scope>/<name>`. Each socket allows 10 signs
  a second, with a burst of 30 (a token bucket). A sign over the limit
  answers `SSH_AGENT_FAILURE`. The daemon keeps an opened key for 60
  seconds, so a loop of signs does not decrypt the key (and run the KDF of
  its passphrase) at each sign. A revoke removes the kept key at the next
  sign; a stop of the socket drops all of them. Each sign counts in a
  `secret.use` audit row: one row each minute for each key and agent, with
  `signs` (the good signs in that minute) and `limited` (the signs over
  the limit). The socket stops when the agent stops. The harness runs as
  the daemon's user, so the 0600 socket is open to it.
- *ssh host keys.* The daemon's git over ssh records each new repo host
  key (trust on first use) in `<state>/ssh/sandbox/ssh_known_hosts`. The
  sandbox binds that dir read-only at `/etc/ssh`, so ssh in the sandbox
  reads the file as its global known hosts file and trusts the same hosts.
  When the daemon makes the file, it copies the lines of its user's
  `~/.ssh/known_hosts` (the file that its git used before) into it once,
  so an upgrade keeps the trusted hosts. GitHub's published host keys (ed25519, ECDSA, RSA, from
  `https://api.github.com/meta`, built into the daemon) are added to the
  file at each start and git run when it lacks them, so git over ssh to
  github.com works in a new sandbox at once. A line that the file has is
  never changed or removed.

**The agent's grants.** `GET /api/agent/secrets` lists the secrets that
are granted to the calling agent: its own grants and its project's, one
entry for each secret, with the highest level. A project secret comes
before a global secret. No entry holds a value. The operator gets `403`.

```json
[{"scope":"ops","name":"db","kind":"api_key","level":"read","env":"DATABASE_URL",
  "fields":{},"secret_fields":["value"],"via":["agent"]}]
```

`via` holds `agent`, `project`, or both. An entry at the level `run` also
has `targets`: the targets where `terrarium run` gets the value.

**Secrets in a run** (TER-142). `terrarium run <target> -e
KEY=@secret:<name> -- <cmd>` and `-e KEY=@secret:<name>.<field>` ask the
daemon for a secret value in the env of the command. The CLI sends the
reference as it is, in `env` of `POST /api/agents/{id}/run` (§6.7). A
value that starts with `@secret:` is always a reference: a literal value
cannot start with it. A secret name has no `.`, so the first `.` starts
the field. The default field is the main secret field; a `generic`
secret with several secret fields needs one (`422`).

- The daemon reads the agent's grants at each run start: no cache, so a
  grant that is added or revoked while the agent works applies at its
  next run. A `run` grant that names the run's target, or a `read`
  grant, resolves. A `run` grant on other targets is `403` with the
  allowed targets named. A `use` grant is `403`, no grant `404`, a
  placeholder with no value `409`, and the token of a repo that goes
  through the git proxy `403`. A refused reference starts nothing.
- The value goes into the env of the remote process only: on an `ssh`
  target as one base64 line on the stdin of the remote shell, which reads
  and exports it; on a `docker` target through `docker exec --env-file`
  with a 0600 file on the host that is removed when the `docker` process
  exits (TER-490); a multi-line value goes instead through `-e KEY` into
  the env of the `docker` process on the host, and some names and long
  lines are refused ([host protocol](host-protocol.md#docker)); on a `host`
  target in the env of the process. It is never on a command line (local
  or remote), in the sandbox env, in the daemon log, or in a record. A
  `vmlab` target takes no env (`422`).
- The output is redacted first with the run's own values (each value of
  at least 4 bytes, also a weak one that the daemon matcher leaves out,
  label `[secret:run/<scope>/<name>]` or `[secret:run/<name>]`), then
  with the daemon matcher. This covers the session output, `terrarium run
  log`, the output tail in the wait result, and the mail of the run end.
- Each reference writes one `secret.run` audit row: `{agent, scope, name,
  field, env, level, target, session, run}`, where `run` is the wait ID.
  The `target.exec` row of the run has the env names only. No row holds a
  value.

**The limit.** The redaction matches the known value as bytes. The
command that gets the value can still leak it: print it encoded, split,
or reversed, write it to a file, or send it elsewhere. A `run` grant stops
an accidental exposure (a value in a transcript, a log, a command line,
or the sandbox env). It does not stop a hostile agent or a hostile
command: give a `run` grant only to an agent that may use the value on
those targets.

**The env of the start.** `POST /api/agent/secrets/env` answers the env
of the calling agent's harness. `terrarium exec-env` calls it inside the
sandbox at each start of the harness.

```json
{"vars": {"DATABASE_URL": "..."},
 "credentials": {"ANTHROPIC_API_KEY": "..."}}
```

- `vars`: the env vars of the agent's `read` grants with an `env` name, by
  the rules of *Env vars* above. A grant with the name of a credential var
  is not in it.
- `credentials`: the credential env of the agent's harness. First the
  token of the Copilot plan account that the last launch picked
  (`COPILOT_GITHUB_TOKEN`), then the stored API key of the agent's
  subscription and the vars that go with it (for example
  `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, and `ZAI_API_KEY` for
  Z.ai). The daemon reads the subscription at the request, so a key
  change applies at the next start. A subscription that the harness
  cannot spend gives nothing.

The answer holds values and has `Cache-Control: no-store`. The daemon
zeroes its copies after the answer. Each answer writes a `secret.env`
audit row with the names only (`{agent, names, credentials}`). The call
has its own limit of 30 a minute for each agent (`429 rate_limited`), so
many `get` calls cannot stop a start. The operator gets `403`. An agent
gets only its own env: the grants that `get` gives it, and the key that
its harness has already. A subscription key is not a secret of the store:
no grant and no `get` reads it.

**Get.** `POST /api/agent/secrets/{name}/get` answers one value for a
`read` grant. The name resolves among the granted secrets: the project
secret first, then the global one. A secret with no grant is `404`. A
`use` or a `run` grant is `403`. `field` names a field; the default is the main
secret field (above). A `generic` secret with more than one secret field
needs `field` (`422`). A plain field answers its value too. The answer
has `Cache-Control: no-store`. Each answer writes a `secret.get` audit
row (`{agent, scope, name, kind, field}`). An agent gets at most 30
answers a minute (`429 rate_limited`).

```
POST /api/agent/secrets/db/get      (agent)
{}

200
{"scope":"ops","name":"db","kind":"api_key","field":"value","value":"postgres://..."}
```

**Audit.** `secret.put` (`{scope, name, kind, created, plain_fields,
secret_fields}`: field names, no value; a generate adds `"generated":
true`, `type`, `bits`, and `fingerprint`), `secret.delete` (with
`targets`: the targets that a forced delete left with no secret),
`secret.reveal` (`{scope, name, kind, secret_fields, ip}`),
`secret.grant` (`{scope, name, kind, grantee, grantee_label, level, env,
targets, created}`), `secret.revoke` (`{scope, name, kind, grantee}`),
`secret.get`, `secret.run` (above), and `secret.use` (`{agent, scope, name, broker, host}` for
the git helper; `{agent, scope, name, broker, signs}` for the ssh-agent).
The target is `<scope>/<name>`. The project of a project secret is the
row's `project`. No row holds a value.

**Storage.** The `secrets` table holds the plain fields and `sealed`: the
XChaCha20-Poly1305 ciphertext of the JSON of the secret fields. The
associated data is the ID, the scope, the name, and the kind, so a row
copied to another name does not open. The master key is
`<state>/secrets.key` (32 bytes, mode 0600), outside the database. The
daemon makes it at the first start. A database backup without the key
file shows no secret. When the key file is missing and secrets exist, the
daemon does not start: restore the key file from a backup. The daemon
decrypts a secret only when it uses one. The sandbox never binds the key
file or the database.

`terrariumd secrets rotate-key` (with the daemon stopped) makes a new key
and seals each secret again in one transaction. It writes the new key to
`secrets.key.new` first, and moves it over `secrets.key` after the
commit. When a crash stops it between the two, the next start finishes
the move. It writes a `secret.rotate_key` audit row.

**The move of the old credentials.** Before this store, each credential
was `<state>/credentials/<name>` (the secret) and `<name>.json`. At each
start, the daemon moves each such pair into a global secret: `token` to
`token`, `ssh_key` to `ssh_key`, `password` to `login`. It copies the two
files to `<state>/credentials/backup/` (mode 0600) first. It writes a
`secret.migrated` audit row (actor `system`) for each, and then removes
the files. The secret keeps the old `created_at`. A pair that the store
refuses stays, and the daemon logs a warning. `subscriptions/`,
`accounts/`, and `backup/` stay as they are.

**Redaction.** PLAN §16b.6. The daemon keeps a matcher of each secret
field of each secret, and of each stored subscription API key
(§16, `POST /api/subscriptions/{name}/api-key`). It replaces each match
with `[secret:<label>]`. The label of a global secret is its name
(`[secret:gh]`). The label of a project secret is `<project>/<name>`
(`[secret:web/deploy]`). The label of a subscription key is
`subscription/<name>` (`[secret:subscription/zai]`).

- The matcher skips a value shorter than 8 bytes. A short value matches
  too much ordinary text.
- The matcher also finds the JSON-escaped form of a value. It finds each
  line of 8 bytes or more of a value with more than one line (an SSH key),
  except the PEM armor lines.
- The matcher leaves out a value (or a line) that looks like an
  identifier, that is a name of this instance, or that is weak (see
  "Values that redaction cannot use" above). This covers the secrets that
  the store had before these checks.
- A put, a generate, and a stored subscription key build the matcher in
  their own write, before the commit. No other write sees the new value
  with the old matcher. A delete of a secret, a project delete, and the
  delete of a subscription build it after the write.
- **Labels by view.** The label shows the secret name only to a caller
  who may see it. Others see `[secret]`. A record that an actor writes
  uses the view of the writer: the operator and a sysadmin agent see each
  name; another agent sees the names of the secrets of its own project,
  and `[secret]` for a global secret, a subscription key, or a secret of
  another project; `system`, a trigger, and an event source get
  `[secret]`. A text that the daemon redacts when it shows it (a
  transcript, a screen, an upload) uses the view of the reader. The log
  of a process wait uses the view of its project.
- **JSON records.** In an event, a trigger payload, and an audit detail,
  the daemon redacts string values only. Object keys stay. The value of
  an identifier key stays too: `id`, `ids`, `actor`, `agent`, `agents`,
  `project`, `projects`, `kind`, `grantee`, `grantee_label`, `scope`,
  `name`, `by`, `created_by`, `from`, `to`, `level`, `action`, `target`,
  `schedule`, `run`, `event`, `session`, `card`, `thread`,
  `subscription`, `harness`, `status`, `state`, and each key that ends in
  `_id`, `_ids`, or `_name`. An audit detail with a redacted match gets
  `"redacted": <the number of matches>`.

The daemon redacts these records before it writes them, so the database
never holds a known value: mail (subject and body), questions and notices
(text, options, context, answer, reason), cards (title, body, comments),
memories (title, body, link notes), wiki pages (title, body, summary),
version notes and review comments, text artifacts (`text/*` and JSON; a
binary file and a bundle stay as they are), events (summary and data,
also the events that start a trigger), trigger run results, wait results,
and audit details.

The daemon redacts these at read time, because a harness or a run writes
them: transcripts and the session list (§6.6), the screen snapshot
(`GET /api/agents/{id}/screen`), and the output of a process wait
(`GET /api/waits/{id}/log`). A change of a secret changes the next read.

A record that the daemon stored before a secret existed keeps its text.
The live terminal stream (`GET /api/agents/{id}/pty`) goes to the
operator only and is not redacted. Redaction is a guard against
accidents. It does not find a value that an agent changed (for example,
base64). Redaction is also a guess oracle: an agent that writes a guess
and reads it back learns that the guess is a stored value, also when the
label is `[secret]`.

**Zeroization.** The daemon zeroes the opened secret fields, the parsed
body of a secret put, and the value in the answer of `POST
/api/repos/credential` and of `secret get` when it drops them. It does
not zero the request bytes, the buffers of the JSON parser, the
serialized body of an answer, or the matcher. The matcher holds each
value for the life of the daemon.

**`/api/credentials`.** This alias stays for one release. It makes and
lists global secrets in the old shapes of §5.1. `kind` is `token`,
`ssh_key`, or `password` (a `login` secret). A `POST` with an existing
name replaces the secret. The list shows each global secret: a `login` as
`password`, the other kinds with their own names. Its audit rows are
`secret.put` and `secret.delete`.

### 5.3 The repo browser

PLAN §9a. The daemon reads the trees of a repo for the dashboard (and for
any caller that can read the project). A tree is:

- `main`: the repo's default branch (`default_branch`, else the remote's
  HEAD), from a bare mirror that the daemon owns:
  `<state>/repo-mirrors/<repo ULID>.git`. The daemon clones it at the first
  request, with the repo's secret as for a checkout (§5.1), outside every
  sandbox. A read fetches every branch into it when the last fetch is more
  than 60 s old; `?refresh=true` on the tree list fetches at most each 5 s.
  A failed fetch keeps the old mirror (`mirror.error`). No agent can write
  the mirror.
- `agent:<ULID>`: the checkout of the repo in that agent's workspace,
  `<work>/<project>/<agent>/<path>`, for each agent of the project that has
  one (a plain `.git` directory). The browser reads the work tree: what the
  agent sees, with its uncommitted files.

| Endpoint | Answer |
|---|---|
| `GET /api/repos/{id}/trees[?refresh=true]` | `200` `{"repo", "repo_id", "project", "default_branch", "trees": [tree]}` |
| `GET /api/repos/{id}/trees/{tree}/files?path=` | `200` `{"path", "entries": [entry], "truncated"}` |
| `GET /api/repos/{id}/trees/{tree}/file?path=` | `200` the raw bytes, at most 1 MiB |
| `GET /api/repos/{id}/trees/{tree}/diff` | `200` changes, below |
| `GET /api/repos/{id}/trees/{tree}/commits[?cursor=&limit=]` | `200` commits, newest first; `X-Next-Cursor` |
| `GET /api/repos/{id}/trees/{tree}/commits/{sha}` | `200` commit with its message and diff |

A tree in the list:

```json
{"id":"main","kind":"main","label":"main","branch":"main","head":"3f2c...","mirror":{"fetched_at":"2026-10-31T01:02:03.000Z"}}
{"id":"agent:01M5PA0T0Z0K16V6G88P3SE78W","kind":"agent","agent":"worker","agent_id":"01M5PA0T0Z0K16V6G88P3SE78W",
 "label":"worker · feat/parser","branch":"feat/parser","head":"9a1b...","base_ref":"refs/remotes/origin/main",
 "base":"3f2c...","ahead":2,"behind":1,"dirty":{"staged":1,"unstaged":2,"untracked":0}}
```

A tree that the daemon cannot read has `error` (a refused repo config, a
failed first clone). `dirty.partial` is `true` when the checkout is too
large to compare each file. An entry is `{"name", "path", "kind", "size"?,
"target"?}`; `kind` is `dir`, `file`, `symlink`, `submodule`, or `other`.
Folders come first. A symlink has its `target` text; the daemon does not
follow it. `truncated` is `true` over 5000 entries.

**The raw file.** UTF-8 text is `text/plain; charset=utf-8`. Anything else
is `application/octet-stream` with `Content-Disposition: attachment`. The
answer is never `text/html` or `image/svg+xml`, and has
`X-Content-Type-Options: nosniff`. `X-Terrarium-Size` is the size of the
whole file; `X-Terrarium-Truncated: true` says that the body is the first
1 MiB only. A directory, a symlink, or a submodule is `422 invalid`.

**Changes.** The base is the merge base of HEAD and the default branch
(`refs/remotes/origin/<default>` in the checkout, else
`refs/heads/<default>`). `committed` is the diff base..HEAD. For an agent
tree, `uncommitted` has `staged` (the index against HEAD), `unstaged` (the
work tree against the index), and `untracked` (paths, at most 1000). For
`main`, `uncommitted` is `null` and `note` is `main is the base`. A diff
set is `{"files": [file], "truncated"}`; a file is `{"path", "status",
"additions", "deletions", "binary", "patch", "truncated", "note"?}`.
`status` is `added`, `deleted`, `modified`, `typechange`, `mode`,
`unmerged`, or `refused`. `refused` is an index entry that the daemon does
not read: its path is absolute, has an empty, `.`, `..`, or `.git` part,
or its mode or blob ID is not valid; `note` says why. `patch` has the
unified diff hunks from the first `@@` line. A file has at most 2000 diff
lines and an answer at most 20 000; `truncated` says that a cap cut it.
The daemon makes the work tree diff only for a text file of at most
20 000 lines on each side (else `note` says "too large to diff", and
`additions` and `deletions` are 0, not known). One file gets at most 2 s
and all the files of an answer at most 8 s. At the file limit the diff is
correct but can be larger than needed (`note` says so). After the answer
limit, a file gets no patch.

**Commits.** For an agent tree, the commits of base..HEAD; for `main`, the
history of the default branch. A commit is `{"sha", "parents",
"author_name", "author_email", "date", "subject"}`. `limit` is 1 to 200
(default 50); the cursor is the number of commits to skip. A commit detail
adds `committer_name`, `committer_date`, `message`, and `diff` (against
the first parent; the empty tree for a root commit). `{sha}` is 4 to 64
hex digits (`422` if not).

**Errors.** A `path` that is absolute, has `..` or a NUL, or names `.git`
is `422 invalid`. An unknown tree is `404`. A checkout whose
`.git/config` has a key outside the allowlist of §5.1 is `409 conflict`.
A checkout with an alternates file, with a symlink or a wrong type in
place of a main name of `.git`, or with a symlink, a FIFO, or a device
anywhere in `.git`, is `409 conflict` too (Security, below).
A git command that does not end in 10 s is `500`, and so is a request
whose reads of an agent tree take more than 20 s in all. A daemon with no usable
`bwrap` answers `503 unavailable` for an agent tree (`main` still works).
An error answer has no stderr of git and no path of the daemon: a failed
git command is `422` with a fixed message (`git <command> failed
(<status>)`), an I/O error is a fixed `500`, and a refusal names paths
relative to the checkout (`.git/HEAD`).

**Security.** The daemon reads repos that agents control. It runs
outside every agent sandbox, so:

- git reads an agent tree only in a `bwrap` sandbox of its own: no
  network and no other namespace of the host, an empty env, `/usr`
  read-only (with the `/lib`, `/lib64`, `/bin`, and `/sbin` of the agent
  sandbox), a new `/proc`, a minimal `/dev`, an empty `/tmp`, and the
  checkout read-only at `/repo`. Nothing else of the host is there: no
  `/etc`, no state dir, no other workspace or checkout. The daemon opens
  the checkout dir once for each request (each path part with
  `O_NOFOLLOW`) and `bwrap` binds the directory of that fd
  (`--ro-bind-fd`; it checks the device and inode of the mount against the
  fd). So a swap of the checkout dir, of `.git`, or of `.git/objects/<xx>`
  for a symlink while the browser reads (a race that the checks before
  each command cannot see) finds nothing outside the checkout. `/usr` is
  visible: it holds programs and libraries, and no secret. The daemon
  reads the work tree from the same fd. `main` is the daemon's own mirror
  and is read on the host. With no usable `bwrap` (a development daemon
  on a host without bubblewrap), an agent tree is `503 unavailable`.
- git runs with the hardening of a checkout update (§5.1): no hooks, no
  fsmonitor, no global or system config, and a check of a copy of
  `.git/config` against the allowlist before the first command. Also
  `--no-ext-diff`, `--no-textconv`, `core.attributesFile=/dev/null`,
  `diff.external=`, `log.showSignature=false`, `GIT_ATTR_NOSYSTEM=1`, and
  `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, every transport off
  (`protocol.allow=never`), no mailmap, and no excludes file of the daemon
  user. Each command has a 10 s deadline and an output cap. git gets the
  git dir and the work tree on the command line (`--git-dir`,
  `--work-tree`), so it does not search the parent dirs and ignores
  `core.worktree`.
- No objects of another repo. The sandbox above is the control. Also,
  before the first command and again before each command, the daemon
  refuses (`409`, a clear error) a checkout with
  `.git/objects/info/alternates` (an empty file is allowed) or
  `.git/objects/info/http-alternates`, and a checkout where `HEAD`,
  `config`, `index`, `packed-refs`, `shallow`, `objects`, `objects/info`,
  `objects/pack`, `refs`, or `info` is a symlink or of the wrong type.
  Each check opens each path part with `O_NOFOLLOW`. Once for each
  request (and before a checkout update), a walk of all of `.git` refuses
  a symlink, a FIFO, or a device anywhere in it (at most 2 s); a symlink
  that comes after the walk finds nothing in the sandbox.
  The stderr of git never goes to the caller (it is logged): with an
  `include.path` added in a race, it could tell if a daemon file exists.
  The mirror is cloned with `--no-local`, so a local source repo does not
  give its alternates file to the mirror; after each clone and fetch the
  daemon removes any alternates file, and it clones a mirror with one
  again.
- Only commands that run no filter driver: `ls-tree`, `ls-files`,
  `cat-file blob`, `log`, `rev-parse`, `rev-list`, `diff-tree` (commit to
  commit), and `diff-index --cached` (HEAD to index). The daemon never
  runs `git status`, `git diff` against the work tree, `git add`, or
  `git hash-object` in a checkout: these run the clean filter.
- The changes in the work tree are found in the daemon: it reads each file
  (each path part opened with `O_NOFOLLOW`), makes the blob ID itself, and
  makes the diff itself. The agent can write any path into `.git/index`,
  so the daemon checks each index path with the rules of a request path
  before it opens the file (`refused` above). The disk walk itself also
  refuses `..`, `.`, `.git`, and an absolute path. A file that a filter or `core.autocrlf` changes
  on checkout shows as changed.
- A symlink is never followed; `.git` is never shown. The daemon never
  writes into a workspace.
- The dashboard shows each name, file, and diff as text. Only a Markdown
  file with **Rendered** on goes through the safe Markdown renderer.

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
| `subscription_pool` | The account pool (PLAN §16.3): subscription names of one provider that the harness can spend, which a swap may pick (§6.2). The current `subscription` is always a member. Absent when empty. |
| `system_prompt` | The agent's base system prompt (below). Absent when not set. |
| `persona` | The persona (below): `leader`, `worker`, `none`, or an entry of the system prompt library (§28). Absent: by role. |
| `skills` | The explicit skills of the agent from the skill library (§26): skill names, or `*` for every skill. `[]`: none. The agent also gets the default-on skills (§26.3). |
| `skills_off` | The skills that the agent does not get: a default-on skill, or a skill of `skills`. `[]`: none. `*` is not valid. |
| `default_skills` | `false`: the agent gets no default-on skill, only its `skills`. Default `true`. |
| `effective_skills` | The skills that the agent gets at its next start, sorted by name: `[{"name","source"}]`, with `source` `default` (the skill is default on) or `explicit` (`skills` names it, or has `*`). The rule: (the default-on skills, when `default_skills`) + `skills` - `skills_off`. `[]` when the daemon has no config repo. |
| `skills_restart` | `true` when the live agent runs with other skills than its `effective_skills` now (its settings, a skill, or the default-on marker of a skill changed after its start). A restart, a wake, or a new session applies them; the daemon does not restart the agent for it. A restart and a wake resume the harness session, which can hold the old text of a skill: when a skill that the agent had changed, the resumed start sends the agent a system mail that names it and says to load it again (PLAN §8.6). A new session loads the skills as they are. Absent when false. |
| `roles` | Roles above `member`: `project_admin`, `sysadmin`. |
| `status` | `starting`, `idle`, `waiting`, `working`, `blocked`, `error`, `stopped`, `restarting`. `waiting`: the turn ended, and the agent has one or more open waits (§6.7). `idle`: the turn ended, and the agent has no open wait. |
| `status_reason` | The reason of the last status change: `idle` after an idle stop, `paused` after the stop of a pause, `subscription_disabled` (the subscription is disabled, §16.2), `stream_limit: waiting for a free stream on `<sub>`` (the work waits for a stream of a self-hosted subscription, §16.2), `stopped`, an exit, or an error text. Absent when not known. |
| `paused` | `true` while the agent is paused (§6.2): nothing wakes it, and its work waits. Always present. |
| `queued` | The work that waits for the agent: queued turns, `queued` mail, card and nudge lines, and a pending reset with a `then` prompt (§6.2). Absent when 0. |
| `idle_stop` | The idle stop of the agent (§6.2): `off`, or a duration such as `30m`. Absent: the daemon's `TERRARIUM_IDLE_STOP`. |
| `compact_on_idle` | `true` or `false`: the agent compacts its session at its idle stop (§6.2, "Auto compact"). Absent: the daemon's `TERRARIUM_COMPACT_ON_IDLE`. |
| `compact_max_age` | The maximum age of the session context (§6.2, "Auto compact"): `off`, or a duration such as `24h`. Absent: the daemon's `TERRARIUM_COMPACT_MAX_AGE`. |
| `card_reminder` | `true` or `false`: the agent gets a `[System:card]` turn when it goes idle with cards in progress (§6.2, "Card reminder"). Absent: the daemon's `TERRARIUM_CARD_REMINDER` (on). |
| `compact_supported` | `false` when the daemon cannot compact agents of the harness: it ignores `compact_on_idle` and `compact_max_age`, and `POST /api/agents/{id}/compact` is `409`. Always present. |
| `reset_pending` | A compaction or a new session that waits (§6.2, "Compact and new session on request"): `{kind, by, then_set, recap, deferred, requested_at}`. Absent when none. |
| `last_compact_at`, `last_compact_ok` | The end of the last compaction that the daemon ran (RFC 3339, also a failed one) and its result. Absent when none. |
| `waits` | The number of open waits (§6.7). Absent when it is 0. |
| `waiting_on` | What the agent waits on, in one short line: the summaries of the oldest 3 open waits and a count of the others. Absent when the agent has no open wait. |
| `session` | The harness session ID of the current session, when known. |
| `harness_version` | The harness version that the live agent runs: the version dir in the tools dir that its launch resolved (§4, `POST /api/harnesses/{name}/update`). It stays until the next start, also after an update. Absent when the agent does not run, or when the version is not known. |
| `current_card` | The card the agent has in the `working` column of its board (§8.1), if any. For an ephemeral agent, else its bound card, else the newest open card that it is the assignee of (TER-75). |
| `source` | `file`, `api`, or `fleet:<name>` (§18.1). |
| `template` | The agent template that the create copied: `{"id", "name", "project", "revision"}` (§21). Absent when the create named none. For display only. |
| `ephemeral`, `owner`, `spawn` | An ephemeral agent (§6.10): `true`, the owner (an agent ULID or `operator`), and `{"owner_name", "template", "task", "spawned_at", "deadline_at"}`. Absent for other agents. |
| `spawned_running` | The ephemeral agents of this agent that run now (§6.10). Absent when 0. |

### 6.1 CRUD

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/agents?project=` | | `200` agents by ID | read; an agent defaults to its project |
| `POST /api/agents` | an agent definition, or `template` and the fields that differ (§21.3) | `201` agent | project_admin of `project`; some `policy` entries need sysadmin (below) |
| `GET /api/agents/{id}` | | `200` agent, with `subscription_detail` (below) | read |
| `PATCH /api/agents/{id}` | `{"harness"?, "subscription"?, "subscription_pool"?, "model"?, "prompt"?, "system_prompt"?, "persona"?, "skills"?, "skills_off"?, "default_skills"?, "policy"?, "restart"?, "idle_stop"?, "compact_on_idle"?, "compact_max_age"?, "card_reminder"?, "new_session_on_prompt_change"?}` | `200` agent | project_admin; some `policy` entries need sysadmin (below) |
| `DELETE /api/agents/{id}` | `?confirm=true` when live; `?keep_workspace=true` | `204`; emits `agent_removed` (§9) | project_admin |

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

`system_prompt` (optional, at most `TERRARIUM_SYSTEM_PROMPT_MAX` bytes,
default 32 KiB, else `422 invalid`): the
agent's base system prompt (PLAN §8). It can use the variables of the
prompt templates (`defaults/prompts/README.md`). An empty text is the same as no
text.

- Not set: the harness keeps its own default system prompt, and the
  rendered `prompt` template is appended to it.
- Set: the rendered `system_prompt`, a blank line, and the rendered
  `prompt` template replace the harness's default system prompt. The
  Terrarium template always stays.

`persona` (optional): the persona prompt (PLAN §8.1), an entry of the
system prompt library (§28, `system-prompts/<name>.md` in the config
repo, else `prompts/<name>.md` of an older repo). `leader`, `worker`,
`none`, or another entry name (letters, digits, `-`, `_`; else
`422 invalid`). Not set: by role, `leader` for an agent with
`project_admin` or `sysadmin`, else `worker`. The persona replaces the harness's default system prompt,
and the protocol template comes after it, as for a `system_prompt`. A set
`system_prompt` wins over the persona. `none` keeps the harness's default
prompt. A named persona with no file stops the start (`422`); the persona
by role with no file, or with only HTML comments, is no persona. `PATCH`
takes `null` or `""` to go back to by role.

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
`subscription`, `model`, `prompt`, `system_prompt`, `idle_stop`,
`compact_on_idle`, `compact_max_age`, or `card_reminder` to clear it
(`{"subscription": null}`). An empty `system_prompt` also clears it.
`null` for the other fields changes nothing.

`idle_stop` (in `POST`, `PATCH`, a template spec, an agent file, and a
fleet entry): how long the agent may rest before the idle stop (§6.2).
`off` (or `0`) never stops it; a duration has one or more parts with the
units `d`, `h`, `m`, and `s` (`30m`, `1h30m`, `90s`). A number is
seconds. Another value is `400 bad_request`. Absent or `null`: the daemon's
`TERRARIUM_IDLE_STOP`.

`compact_on_idle` and `compact_max_age` (in `POST`, `PATCH`, a template
spec, an agent file, and a fleet entry): the auto compact settings
(§6.2). `compact_on_idle` is `true` or `false`. `compact_max_age` has the
form of `idle_stop`: `off` (or `0`), or a duration such as `24h`. Another
value is `400 bad_request`. Absent or `null`: the daemon's
`TERRARIUM_COMPACT_ON_IDLE` and `TERRARIUM_COMPACT_MAX_AGE` (both off).

`card_reminder` (in `POST`, `PATCH`, a template spec, an agent file, and
a fleet entry): `true` or `false`, the card reminder of the agent (§6.2,
"Card reminder"). Absent or `null`: the daemon's
`TERRARIUM_CARD_REMINDER` (on).

`subscription` (in `POST` and `PATCH`, also from a template): it must
exist. A name that is not a subscription is `422 invalid` with
`details.field = "subscription"`, and the message names the
subscriptions that the agent's harness can spend: ``subscription: there
is no subscription `anthropic-max-20x`; the claude subscriptions are:
anthropic-ultra. Name one of them, or leave it empty for the default
account of the harness``. An empty `subscription` (or `null`) is the
default account of the harness (§16.3). A file sync and a fleet apply do
not run this check: such an agent fails at its start with the
subscription error.

`subscription_pool` (in `POST` and `PATCH`): each member must exist, the
agent's harness must be able to spend it, and all members and the current
subscription must have one provider (`422 invalid`, the message names the
member). The daemon puts the current subscription at the front when it is
missing, and drops empty names and duplicates. `[]` clears the pool. A
`PATCH` of `subscription` checks the stored pool again. A change of the
pool reaches a running Claude Code agent at its next start (the sandbox
binds the pool's accounts at the launch).

pi uses API keys only (PLAN §16.3). A create, a `PATCH`, a file sync, or a
fleet apply that gives a `pi` agent a plan login (as its `subscription` or
in its pool), or a `PATCH` of `harness` to `pi` on one, gets `422
invalid`: "subscription `<name>`: pi uses API keys only; use Claude Code
for a Claude plan and Codex for a ChatGPT plan". A template whose spec
does the same gets the same `422`.

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
| `POST /api/agents/{id}/start` | none | `202` agent. A wake now. A paused agent gets `409 conflict`. |
| `POST /api/agents/{id}/stop` | none | `202` agent. Also on an agent in `error`: it becomes `stopped`. |
| `POST /api/agents/{id}/restart` | none | `202` agent. Also on an agent in `error`: a new launch. A paused agent gets `409 conflict`. The restart resumes the harness session, so the context keeps what the agent read before: a changed prompt or skill reaches its context only by a new session (`.../new-session`) or a message (a changed skill sends a system mail, PLAN §8.6). |
| `POST /api/agents/{id}/pause` | none | `202` agent, `paused: true`. A running agent stops (reason `paused`). |
| `POST /api/agents/{id}/unpause` | none | `202` agent, `paused: false`. It wakes when work waits. |
| `POST /api/agents/{id}/compact` | none, or `{"then"?: "<prompt>", "defer"?: false}` | `202` agent with `reset` (below). At rest: `status` `working`, `status_reason` `compacting`. The end comes as `agent.compacted` (§9). `409 conflict`: no compaction for the harness, or the agent is paused or held. `409 not_running`: the agent does not run. |
| `POST /api/agents/{id}/new-session` | none, or `{"then"?: "<prompt>", "recap"?: true, "defer"?: false}` | `202` agent with `reset` (below). A new harness session, not a resume. |
| `POST /api/agents/{id}/send` | `{"text": "Run the tests", "raw"?: false}` | `202 {"accepted": true}` (`"raw": true` with `raw`; `"queued": true` when the agent does not run, or an earlier turn still waits in the store) |
| `GET /api/agents/{id}/queued` | none | `200 {"items": [{"id", "source", "text", "body", "created_at"}], "queues": {"runner": "available"}}`. Waiting turns in delivery order: `runner`, `supervisor`, `hold`, then `store`, preserving each queue's order. Durable IDs are numbers. In-memory IDs are opaque strings and are not reused on restart. `created_at` is empty for in-memory turns. `text` includes the tag, `body` excludes it. Redacted for the caller, with the agent's read right. The snapshot can become stale during delivery or hold release. Recaps and mail pointers are not included. |
| `DELETE /api/agents/{id}/queued/{turn}?source=store` | none | `204`. Removes an untouched queued turn. Pass its ID and `source` from the list (`store` by default, also `supervisor`, `runner`, or `hold`). Unknown sources are `422`. Requires the operator or project_admin of the agent's own project (not a sysadmin alone; link keys are refused). `404` once dispatch takes the turn, including between list and delete. Never interrupts typing or cancels a retry of an already typed turn. Remaining turns keep their order. Audit `agent.queued.remove` includes the ID and source for in-memory turns, or ID and byte length for durable turns. |
| `POST /api/agents/{id}/keys` | `{"keys": ["esc", "down", "enter"]}` | `202 {"accepted": true, "keys": [...]}` (§6.5) |
| `POST /api/agents/{id}/interrupt` | none | `202 {"accepted": true, "interrupted": true}`. Stops the running turn; the harness keeps running (§6.5, "Interrupt"). |
| `POST /api/agents/{id}/attachments` | the raw image, or `{"data_base64"}` | `201 {"path", "name", "bytes", "content_type"}` (below) |
| `POST /api/agents/{id}/answer` | `{"request_id", "answer"}` or `{"text"}` | `202 {"accepted": true}` |
| `POST /api/agents/{id}/subscription` | `{"subscription": "anthropic-max-20x-2"}` | `200 {"agent", "from"?, "to", "applied"}` (below) |

The queued-turn response also includes `queues.runner`: `available`,
`unsupported` (an older runner without `list_pending`), or `unavailable`
(for example an exiting runner or lost control connection). A runner-list
failure does not hide the supervisor or store items. The dashboard shows
the marker and still allows editing those items. Empty idle queues are not
polled repeatedly.

Held turns backed by store rows appear once, with source `hold` and their
durable row ID. Removing one deletes its row, so it cannot be delivered
after release or a daemon restart. Budget holds are mail-only and add no
turns here. Only vendor-refusal holds (`limit:<subscription>`) remain.
Removing queued turns requires the operator or a project_admin of the
agent's own project, not a sysadmin alone. Link keys may not remove turns.

All need project_admin; an agent may `compact` and `new-session`
itself. The work happens in the background. A `send` writes a row to the
delivery log first (§6.6).

**A turn is tagged (PLAN §8.5).** A `send` waits until the agent is
`idle`, and types `[User] <text>` when the operator calls it (the
dashboard, or the API or the CLI with the operator's token or session).
A send from an agent (for example a lead's `terrarium agent send
<worker> ...`) types `[Agent:<project>/<name>] <text>`: it names the
calling agent and is never `[User]`. The text is not quoted, but a line in
it that would read as a tag is defused (`[User]` becomes `[\User]`). A
schedule `turn` action types `[System:schedule <name>] <text>`. Because a
tagged turn starts with `[`, the harness never runs it as a slash command
or a shell command. (An adapter still puts U+00A0 in front of an
untagged text that starts with `/` or `!`, or of a lone `$` in Copilot.)
To run a harness command on purpose, use `raw` (§6.5): a raw text has no
tag, and its tag-like lines are defused. A raw text and keys stay
untagged for an agent caller too, so untagged text is the operator or a
lead with the PTY rights. The answer shows the agent when
the daemon accepted the call; watch the events (§8) for the status
changes.

**Wake, idle stop, and pause (PLAN §18).** An operator or a lead does not
need `start` and `stop`:

- **Wake.** Work for an agent that does not run starts it, with resume of
  its last harness session: mail to the agent (also the result mail of a
  wait, a watch mail, and a question answer), a `send` (also a schedule
  `turn` action), a card that another actor assigns to it or its
  unblocked card, and a lead nudge. The work waits in the store (the
  `queued` mail, the queued turns, the card and nudge lines) and goes out
  when the harness is ready: the queued turns first, in order, after a
  recap turn; then one pointer turn. A lease line waits for the next start
  but does not wake the agent. One wake at a time runs for each agent. The
  wake writes an `agent.woken` event (§9) with its `cause`.
- **No wake** for a paused agent, an agent in `error`, or an agent with a
  quota or budget hold (§16.6): the work waits. The end of the last hold,
  an unpause, and the daemon start wake an agent with work. A wake whose
  start fails sets `error` (reason `wake failed: ...`); then no work wakes
  the agent until the operator clears `error` (`stop`, `start`, or
  `restart`).
- **Idle stop.** An agent that is `idle` or `waiting` for its `idle_stop`
  (else `TERRARIUM_IDLE_STOP`, default `10m`) stops as `stop` does. The
  status is `stopped`, `status_reason` is `idle`, and the session ends
  with the outcome `idle`. The restart policy does not apply. Not stopped:
  an agent with a live exec session or `terrarium run` job, an open
  `process` wait, queued work, or a wake on the way. A target display
  operation of the agent starts its clock again. Open waits, watches,
  leases, and grants stay; an ephemeral target goes down as at any stop.
- **Pause.** `pause` sets `paused` and stops a running agent gracefully
  (reason `paused`). While it is paused, nothing wakes it; `send` answers
  `202 {"accepted": true, "queued": true}` and the turn waits. `start` and
  `restart` answer `409 conflict`. `unpause` clears the flag and wakes the
  agent when work waits. A second `pause` or `unpause` changes nothing.
  Each change writes an audit row (`agent.pause`, `agent.unpause`, with
  `queued`) and an event (`agent.paused`, `agent.unpaused`). The rights
  are those of `stop`.

**Card reminder (PLAN §18).** An agent that goes `idle` after a turn
while it is the assignee of cards in the working column (the column with
the role `working`) gets one turn from the daemon, for example:

```text
[System:card] Cards TER-9, TER-12 are still in progress and assigned to you, and you are idle. Update them before you go idle: comment, move to review or blocked, or set a new ETA. If you wait on something, register a wait. The ETA has passed: TER-9 (ETA 2026-11-05T10:00:00.000Z).
```

- **When.** At the change to `idle`, so before the idle stop: the idle
  stop waits for the check. Not for `waiting` (an open wait is the normal
  way to end a turn with a card in progress), `blocked`, `stopped`, a
  paused agent, an agent that compacts or has a pending reset, and an
  agent that went idle after a start with no turn.
- **Once for each card.** One turn lists all the due cards. A card is
  due again only after 1 hour, or when its ETA passed after its last
  reminder. The agent's own update of the card does not make it due, so
  the reminder cannot loop. The times are in memory: a daemon restart
  forgets them.
- **History.** Each reminded card gets a `card` event with the change
  `reminded` (§9). It wakes no agent and nudges no lead.
- **Setting.** `card_reminder` of the agent (§6.1); absent: the daemon's
  `TERRARIUM_CARD_REMINDER` (on).

**Idle alert (TER-67).** The card reminder asks the agent. The idle alert
tells the project's leads when the assignee of a card in progress rests
with no wait for too long, so a WIP place does not stay held while both
sides wait. The daemon checks on each lifecycle tick (15 s).

- **Which cards.** Not archived, in the column with the role `working` or
  a column named `research`, with an agent as assignee.
- **Resting.** The assignee's status is `idle` or `stopped`, it is not
  paused, and it has no open wait (`waiting` never counts). The rest
  starts at the first `idle` or `stopped` status after the newest other
  status, so an idle stop does not start it again, and a turn or a wait
  does.
- **Threshold.** The project's `idle_alert` (§5): default 15m, `off`
  for none.
- **Crossing.** The card gets `idle_since` (the start of the rest, §8.1)
  and a `card` event with the change `idle` (`idle_since`, `mailed`). The
  leads of the project (agents with `project_admin`, other than the
  assignee; with none, the operator) get one mail from `system` in one
  thread: the agent, the card, the idle time, and the last line of the
  agent's transcript as quoted data. Once for each card and rest, also
  across a daemon restart.
- **Clearing.** When the assignee works again, waits, or is paused, a
  newer rest starts, the card moves to another column or gets another
  assignee, or the threshold is `off`: `idle_since` goes, with a `card`
  event with the change `idle_cleared`.

**Auto compact (PLAN §18).** The daemon can compact the session of an
agent at rest. Each compaction is one model call, so all of it is off by
default.

- **On idle.** With `compact_on_idle` (else `TERRARIUM_COMPACT_ON_IDLE`),
  an agent at its idle stop compacts first, waits for the end (at most 5
  minutes), and then stops. An agent with `idle_stop` `off` compacts once
  after the same rest (the global idle stop, else 10 minutes) and stays
  up.
- **Max age.** With `compact_max_age` (else
  `TERRARIUM_COMPACT_MAX_AGE`), an agent at rest whose session context is
  older than the maximum compacts and goes on. The age counts from the
  end of the last compaction, else from the start of the session. A wake
  that resumes a session keeps the start of that session.
- **Once for each turn.** An automatic compaction runs only after a turn
  that is newer than the last compaction and the start of the session.
  A manual one has no such rule.
- **Manual.** `POST /api/agents/{id}/compact` compacts an agent at rest
  now, or at the end of its turn (below, "Compact and new session on
  request"). `terrarium agent compact <agent>` calls it.
- **Not compacted:** a paused agent, an agent with a quota or budget hold,
  an agent that is not `idle` or `waiting`, an agent in `error`, and an
  agent of a harness with no compaction (`compact_supported` `false`).
- **How.** The supervisor types the harness's `/compact` as a raw
  command (no tag, no escape), and shows the agent `working` with
  `status_reason` `compacting`. Turns, the recap turn, and push pointers
  wait until the end. The prompt and stop hooks of the command are
  `system` notices, not turns. The end is the completion signal of the
  harness (docs/compat.md, "Compaction"), a stop of the agent, or the time
  limit. Then the agent shows its status again (`status_reason`
  `compacted` or `compaction failed`), its idle clock starts again, and
  the waiting work goes out.
- Each end writes `agent.compacted` (§9) and `last_compact_at` (also for a
  failed compaction, so that it does not run again without a new turn).

**Compact and new session on request (PLAN §18).** An agent can compact
its session or start a new, clean harness session, for itself or (with the
rights) for another agent, and attach a prompt that goes after the reset:

- **Rights.** Each agent may reset itself. A reset of another agent needs
  project_admin of its project (a lead on its workers), sysadmin, or the
  operator; else `403`.
- **`then`.** At most 32 KiB; a blank text is `400`. It goes as the next
  turn after the end of the compaction (whatever its outcome), or after
  the recap turn of the new session: `[System:reset requested by <who>]
  <text>`, where `<who>` is `self`, `operator`, or `<project>/<agent>`.
  Its tag-like lines are defused as in each turn. When the compaction did
  not complete, the text starts with the line `Note: the compaction did
  not complete: <reason>.` It is a normal turn: a quota hold, the pause,
  and the stream limit of a self-hosted subscription apply.
- **When.** An agent at rest resets at once (`state` `started`). A request
  of the agent itself, `"defer": true`, and a request for a busy agent
  (working, blocked, starting, or compacting) wait for the end of the
  current turn (`state` `queued`); a compaction that runs is not
  interrupted. While a reset waits, the supervisor keeps the other work of
  the agent (turns, the recap, push pointers) back, so the reset runs
  first. A paused agent keeps a reset queued until the unpause. When the
  daemon is starting a reset of the same agent at that moment, the
  request waits for it (at most 5 seconds) and then answers with what
  happened to its own reset; only when that start takes longer does it
  answer `queued`.
- **Refused.** A compaction for a harness that the daemon cannot compact
  is `409 conflict` before anything is written (`compact_supported`
  `false`, or a supervisor that cannot compact the harness). A queued
  compaction that later cannot run at all is dropped: its `then` still
  goes, with the note line, and the event `agent.reset` with `state`
  `dropped` and the `reason` records the drop.
- **One at a time.** Each agent has at most one pending reset
  (`reset_pending` on the agent: `{kind, by, then_set, recap, deferred,
  requested_at}`; the `then` text does not show). A new request replaces
  it (`replaced: true`).
- **New session.** The daemon ends the harness gracefully (its exit keys
  at rest, else a kill) and starts it with no resume. The store opens a
  new session row (the old one ends with the outcome `new_session`), so
  the Sessions tab and the session links show it. The status reason of
  the start is `new session: requested by <who>`. With `recap` (the
  default), the first turn is a short `[System:recap]` turn: the open
  cards of the agent, the number of open waits and of unread mail, and
  the open questions. Then `then`. An agent that does not run starts at
  once in a new session when `then` is set (`agent.woken` with the cause
  `reset`); without `then`, its next wake starts a new session (`state`
  `next_wake`). An ephemeral agent keeps its spawn record and its
  deadline.
- **The answer** is the agent with `reset`: `{kind, state, replaced,
  deferred, then_set}`. `kind` is `compact` or `new_session`; `state` is
  `started`, `queued`, or `next_wake`.
- **Events and audit.** Each request writes `agent.reset` (§9), and a
  dropped queued reset writes a second one with `state` `dropped`. A request
  on another agent writes the audit row `agent.compact` or
  `agent.new_session` (`{then_set, deferred, replaced}`); a request of
  the agent itself writes none. `terrarium agent compact` and `terrarium
  agent new-session` call these endpoints (`--self` for the caller).

- `start` opens a new session and resumes the last harness session.
  A running agent gets `409 already_running`. It is a wake now: the
  queued turns go to the agent.
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
- `subscription` swaps the agent to another plan account (PLAN §16.3).
  The new subscription must be in `subscription_pool` when the agent has a
  pool (`422 invalid`, "not in the pool"), have the provider of the current
  one, and have a login of the harness when the current one has
  (`422 invalid`). The current one again: `409 conflict`. A file agent
  may swap only among the members of its pool (else `409 file_managed`).
  `applied` says how the swap took effect:

  | `applied` | Meaning |
  |---|---|
  | `live` | Claude Code on an account, and the new account is bound in the sandbox: the daemon took the harness's locks and moved the agent's pointer. The harness reads the new login at its next request. No restart. |
  | `restart_scheduled` | The agent is in a turn (`working`, `starting`): it restarts with resume when the turn ends. Codex, Copilot, pi (it moves between API keys), and a Claude agent whose new account is not bound. |
  | `restarted` | The agent was not in a turn (`idle`, `waiting`, `blocked`): it restarted with resume now. |
  | `next_start` | The agent does not run: its next start uses the new account. |

  The store names the new subscription at once. Each swap writes an
  `agent.swap` audit row `{from, to, applied, auto, reason}` and an
  `agent.swapped` event, and ends the agent's usage-limit hold of the old
  subscription. The daemon uses the same swap for the auto-swap after a
  usage-limit error (§16.6), with `auto: true` and the actor `system`.
- `restart` ends the current session with outcome `restarted` and starts a
  new one. On a stopped agent, or an agent in `error`, it is the same as
  `start`: a new launch. A `start` or a `restart` is a retry: it ends the
  agent's usage-limit holds (§16.6).
- `send` types one turn through the runner: it waits until the agent is
  `idle` and the prompt is ready. To an agent that does not run (also a
  paused agent, or one in `error`), the turn waits in the store, and the
  answer is `202 {"accepted": true, "queued": true}`. The agent wakes,
  unless it may not (above). A turn to a running agent that still has a
  turn in the store (one that came while a wake was on the way) waits
  behind it, with the same answer, so the turns go out in the order of
  the sends. A `raw` send and `keys` need a running agent:
  `409 not_running`.
- `start` of an agent whose harness adapter is not registered, or whose
  prompt template or harness home is missing, gets `422 invalid` with the
  reason.
- The daemon does not check the installed harness version at a launch.
  Any version starts, with no `warning` event (§4, `GET /api/harnesses`).
- `start` of an agent whose subscription does not exist, or whose harness
  is not in the subscription's `harnesses` (PLAN §16, policy hooks), gets
  `422 invalid` with the reason. pi uses API keys only: a `pi` agent on a
  plan login (Claude Pro/Max, ChatGPT, Copilot, any OAuth login) gets
  `422 invalid`, "pi uses API keys only; use Claude Code for a Claude plan
  and Codex for a ChatGPT plan". An agent with no subscription starts; the
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

When the harness exits on its own, the restart policy applies (PLAN §18).
A `stop`, a `pause`, and an idle stop are not exits of the harness: no
policy starts the agent again after them, also not `always`.

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

Push delivery (PLAN §12, §13, §8.5): new mail to an agent, a card
assigned to it by someone else, and an unblocked card of its own each add
one block to a pending pointer turn. A mail block is a tag line and the
body quoted with `> `:

```
[Mailbox:operator] Subject: Deploy (thread T, message M)
> Deploy the branch when the tests pass.

[System:card] Card TER-4 was assigned to you. Run `terrarium card show TER-4`.
```

A body over 2000 characters (after the quote marks) is cut, and its block
ends with `… (cut; run `terrarium mail read --thread T`)`. The runner
types the pointer when the agent is `idle` or `waiting`, else after the
next stop hook. One pointer covers all the items that came in meanwhile,
one block each. When the harness takes the turn, a mail whose body the
turn held whole becomes `read` (`read_by` is the agent, audit `via:
"turn"`), and a cut mail becomes `delivered`. Mail that is `queued` when an agent starts goes into
its first pointer. A card line (and a lease line, §17.7) for an agent that
does not run is kept for the next start, also across a daemon restart: the
daemon stores it in the `pending_pointers` table. A newer line for the same
card and change (or the same target) replaces the older line. Before the
pointer goes out, the daemon checks each card line again and drops a line
that no longer applies (for example, the card went to another agent).

Lead nudges (PLAN §13): three board changes add a line to the pointer of
each `project_admin` agent of the card's project. The agent that made the
change gets no nudge. A lead that does not run keeps the line in
`pending_pointers`, and the nudge wakes it.

| Change | Line |
|---|---|
| A card with no assignee moves into `ready`, also a new card made in `ready` | `New card DEM-3 in ready: <title>. Run `terrarium card ls --unassigned --column ready`.` |
| A card moves to `review` | `DEM-3 moved to review by <who>. Run `terrarium card show DEM-3`.` |
| A card moves to the `blocked` column (§8.1; an event from before migration 48: the column `blocked`) | `DEM-3 moved to <column> by <who>: <comment>. Run `terrarium card show DEM-3`.` |

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

**Attachments.** `POST /api/agents/{id}/attachments` puts one image into
the agent's workspace. The next turn names its path, and the model reads
the file with its own tool. The model can also copy the file or use it in
other ways. The rights are the rights of `send`: project_admin of the
agent's project, sysadmin, or the operator.

- The body is the raw image, with `Content-Type: image/png`, `image/jpeg`,
  `image/gif`, or `image/webp`. The CLI sends JSON
  `{"data_base64": "..."}` with `Content-Type: application/json`, because
  its transports carry JSON only.
- The daemon reads the type from the magic bytes, not from the header. A
  body that is not a PNG, JPEG, GIF, or WebP image is `415
  unsupported_media_type`. Another `Content-Type` is `415` too.
- An image has at most 20 MiB (`MAX_ATTACHMENT_BYTES`). A larger image is
  `413 too_large`.
- The daemon writes the file to
  `<workspace>/.terrarium/attachments/<YYYYMMDD-HHMMSS>-<id>.<ext>`, with
  a temp file and a rename. The file has mode `0644` and the uid of the
  daemon (the sandbox runs as the same uid). The daemon does not follow a
  symlink at `.terrarium` or `.terrarium/attachments`: it answers `409
  conflict`.
- The answer is `201 {"path", "name", "bytes", "content_type"}`. `path` is
  the path that the harness sees:
  `/work/<project>/<agent>/.terrarium/attachments/<name>` in the sandbox
  (and on a target). It is the host path only in sandbox mode `none`. The
  answer never has the host path of the work root.
- When the workspace root is a git repo (a `.git` dir), the daemon adds
  `/.terrarium/` to its `.git/info/exclude` once. It does not change
  `.gitignore`. A repo in a subdir of the workspace does not see
  `.terrarium/`.
- The audit row is `agent.attachment`, with `name`, `bytes`, and
  `content_type`. It does not have the image.
- The prune timer removes attachments that are older than 7 days. Each
  upload also removes the old attachments of its workspace. The other
  attachments go with the workspace when the agent is removed.

The dashboard composer and `terrarium agent send --image` upload each
image first, then send the turn. The turn has the text, an empty line,
and one line for each image:

```
Attached image: /work/terra/w1/.terrarium/attachments/20261028-142501-k3m9x2qa.png (open this file with your image or file-read tool to see it)
```

The text can be empty when the turn has an image. When an upload fails,
no turn goes, and the dashboard keeps the draft and the images.

### 6.3 Sessions

`GET /api/agents/{id}/sessions`: newest first. Cursor: session ID.

```json
[{"id":"01M5PA0T1NSYX43DDK1M2KT8KR","agent":"01M5PA0T0Z0K16V6G88P3SE78W",
  "harness_session_id":"01a1727a-c74a-4775-b7e8-97ff4e4add64","current":true,
  "started_at":"2026-10-24T06:36:54.197Z","turns":3,
  "resumes":2,"resumed_at":"2026-10-24T09:12:03.881Z",
  "transcript":true,"blocks":17,"title":"You now have access to target vlab until 2026-10-25T10:13:44.170Z. Run `terrariu",
  "active":true}]
```

An ended session adds `ended_at` and `outcome`: `stopped`, `restarted`,
`error`, `start_failed`, `superseded`, `exited`, `crashed` (a restart after
an exit), `resume_failed`, `daemon_restart`, or the reason the supervisor
gave. A harness session that began a new conversation in the same run
ends its row with `new_session`.
`turns` counts the `turn_start` events of the session, its resumes
included.

**One row for each harness session.** A start, a restart, a wake, a
restart of the restart policy, or a daemon restart that resumes the same
harness session (the same `harness_session_id`) reuses the agent's row of
that harness session. The row becomes current again when the start
begins, before the harness reports its session, so the current session ID
that a reader gets does not change after that. Its end goes, `resumes`
goes up by one, and `resumed_at` is the start of the resume. A new row
comes only with a new harness session: a fresh start, a resume that failed
(the resumed row ends with `resume_failed`; its `resumes` counts the
attempt), or a new conversation of the harness. A start with no harness
session to resume has a new row at once; when the harness reports a
session that an older row has, that row takes its events, and the new row
goes. Migration 34 merged the rows that earlier resumes made into the
earliest row of each harness session.

| Field | Meaning |
|---|---|
| `resumes` | How many times the daemon resumed this harness session. `0` for a session that never resumed. |
| `resumed_at` | The start of the last resume. Absent when `resumes` is `0`. |

The transcript fields (§6.6):

| Field | Meaning |
|---|---|
| `transcript` | The daemon found the session file, and the file passed the path rule of §6.6. |
| `blocks` | The number of transcript blocks. `0` when there is no transcript. |
| `title` | The first user text that the harness did not make, on one line, at most 80 characters. `null` when there is none. |
| `active` | This is the agent's current session, and the agent runs. |

The Sessions tab of the dashboard shows one row for each session, with
"resumed N×" when `resumes` is more than 0. Rows of one harness session
from an older daemon (one row for each resume) still show as one
conversation, and the tab opens the newest one.

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

**Keys and raw text.** A leader uses these calls to fix a stuck harness (a
dialog, a wrong mode) and to run a harness command. Read the screen first
and after. The rights are the rights of the PTY: the project_admin of the
agent's project, a sysadmin, or the operator. A member, and the lead of
another project, get `403 forbidden`. Both calls write to the terminal at
once. They do not wait for `idle`.

- `POST /api/agents/{id}/send` with `"raw": true`: the text is typed as
  it is, then Enter. There is no escape and no bracketed paste, so
  `/compact` runs as a command. For Codex, the Enter goes 0.3 s after the
  text, as for the exit keys. At most 2000 characters. The audit row
  `agent.send_raw` has the length (`bytes`) and the first word
  (`first_word`, at most 40 characters), never the text.
- `POST /api/agents/{id}/keys` with `{"keys": [...]}`: 1 to 20 keys, in
  order, 50 ms apart. A key is a chord name of the display keys (§17.10:
  `esc`, `ctrl-c`, `ctrl+alt+x`), or `text:<text>` for literal text (at
  most 2000 characters, no bracketed paste). The audit row `agent.keys`
  has the canonical key names; a text key is `text(<length>)`.

The key map (xterm, the `TERM` of the agent PTY):

| Key | Bytes |
|---|---|
| `enter`, `tab`, `shift-tab`, `backspace`, `escape` (`esc`), `space` | CR, HT, `ESC [ Z`, DEL, ESC, space |
| `up`, `down`, `right`, `left` | `ESC [ A`, `B`, `C`, `D`; with modifiers `ESC [ 1 ; <m> A` |
| `home`, `end` | `ESC [ H`, `ESC [ F` |
| `insert`, `delete`, `pageup`, `pagedown` | `ESC [ 2 ~`, `3 ~`, `5 ~`, `6 ~`; with modifiers `ESC [ <n> ; <m> ~` |
| `f1` to `f4`; `f5` to `f12` | `ESC O P` to `ESC O S`; `ESC [ 15 ~`, `17 ~`, `18 ~`, `19 ~`, `20 ~`, `21 ~`, `23 ~`, `24 ~` |
| `ctrl-<letter>` | The control code: `ctrl-c` is 0x03 |
| `ctrl-space`, `ctrl-[`, `ctrl-\`, `ctrl-]` | NUL, ESC, 0x1c, 0x1d |
| `alt-<key>` | ESC, then the key |
| A letter, a digit, punctuation (`slash`, `/`) | The character; `shift-a` is `A` |

`<m>` is 1 + shift (1) + alt (2) + ctrl (4). A key that a terminal has no
code for (`super-…`, `f13` to `f24`, `print`, `menu`, `capslock`), an
empty list, or more than 20 keys gives `422 invalid`, and no key goes out.
More than 20 keys and raw sends to one agent in one second give
`429 rate_limited`. A stopped agent gives `409 not_running`. The status
does not change: an agent that is `blocked` on a dialog goes back to
`working` with its next tool call or turn.

**Interrupt.** `POST /api/agents/{id}/interrupt` (no body) stops the
running turn of the agent, as Esc does in the harness. It is the Stop
button of the dashboard's Conversation view and `terrarium agent
interrupt`. The harness keeps running and keeps its session. The rights
are the rights of a send: the project_admin of the agent's project, a
sysadmin, or the operator. A member, and the lead of another project, get
`403 forbidden`.

The daemon types the interrupt keys of the harness
(`Adapter::interrupt_keys`), 150 ms apart:

| Harness | Keys | Why |
|---|---|---|
| Claude Code | Esc | The busy footer says "esc to interrupt". A second Esc on an idle prompt opens the rewind menu. |
| Codex | Esc | The busy line says "esc to interrupt". |
| Copilot CLI | Esc, Esc | The busy footer says "esc esc interrupt". |
| pi | Esc | pi stops a run on Esc, not on Ctrl-C. Terrarium sets `doubleEscapeAction: "none"`. |

No harness gets Ctrl-C: a second Ctrl-C quits most harnesses. The keys
count against the key rate limit above.

Claude Code sends no stop hook for an interrupt. So the runner watches
the screen for 30 s after the keys: when the prompt is ready, or the
screen is quiet for 1 s and not busy, the turn ends. The agent goes
`idle` with `status_reason` `interrupted`, and the turns that wait go
out. When the harness leaves the stopped prompt in its input box, the
runner empties the box first (`clear_input_keys`). The events of the
agent get a `system` event "interrupt: the interrupt keys went to the
harness; the turn stops". The transcript shows the interrupt as the
harness writes it: a `system` block `[Request interrupted by user]`
(Claude Code), "Turn aborted: interrupted" (Codex), "Aborted: user
initiated" (Copilot), or the error of the aborted call (pi). The audit
row `agent.interrupt` has the `harness`.

Refusals: a stopped agent gives `409 not_running`. An agent that runs no
turn (it is not `working` or `blocked`), or that compacts its session,
gives `409 conflict`, and no key goes out: an Esc on an idle prompt can
open a menu. A harness with no interrupt keys gives `409 conflict`.
A runner from before the `interrupt` call gets the keys as plain writes;
its status then follows the hooks only.

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

The daemon redacts each block when it reads it (§5.2, "Redaction"): the
text, the turn parts, the tool input and summary, and the tool result. A
known secret value shows as `[secret:<label>]`.

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
| `origin` | On a user `text`: `operator`, `agent`, `agent_mail_pointer`, `system`, or `user`. See below. |
| `parts` | On a user `text` that starts with a turn tag (PLAN §8.5): one object for each tagged block, in order. `source` is `user`, `agent` (with `sender`, `project/agent`), `mailbox` (with `sender`, `subject`, `thread`, `message`), or `system` (with `kind`, and `detail` for `schedule <name>`). `body` is the text after the tag line: for a mail, the quoted body. Left out on an untagged text. `text` keeps the raw turn. |
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
a meta message) and `user` on a typed prompt. A typed prompt that starts
with a turn tag gets its origin from the first tag: `[User]` gives
`operator`, `[Agent:...]` gives `agent`, `[Mailbox:...]` gives
`agent_mail_pointer`, and
`[System:...]` gives `system`. It also gets `parts`. The daemon also
writes a row to its delivery log before it types a turn (the SHA-256 of
the text with whitespace collapsed, not the text, and the origin of its
tag). An untagged prompt that matches a row gets the origin of the row. A
prompt that someone typed into the terminal, and a raw send, stay
`user`: they count as the operator's (a raw send can also come from a
lead with the PTY rights).

```json
{"seq":7,"role":"user","kind":"text","origin":"agent_mail_pointer",
 "text":"[Mailbox:ht/lead] Subject: TER-4 (thread T1, message M1)\n> Use the new grammar.",
 "parts":[{"source":"mailbox","sender":"ht/lead","subject":"TER-4","thread":"T1",
           "message":"M1","body":"> Use the new grammar."}], …}
```

**Page**

```
GET /api/agents/smoke%2Fprobe/sessions/01M5PA0T1NSYX43DDK1M2KT8KR/transcript?after=0&limit=2

200
{"session":"01M5PA0T1NSYX43DDK1M2KT8KR","harness":"claude","active":true,
 "blocks":[
  {"id":"248e9b6e-e0ec-49d1-b44c-6a9d2a7bf38b:0","session":"01M5PA0T1NSYX43DDK1M2KT8KR","seq":1,
   "at":"2026-10-25T09:53:44.196Z","role":"user","kind":"text","origin":"operator",
   "text":"[User] Run `terrarium targets ls`.","parts":[{"source":"user","body":"Run `terrarium targets ls`."}],
   "source":{"harness":"claude","file":"01a1727a-….jsonl","line":5}},
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

**Sessions by ID.** A session also has endpoints of its own, keyed by the
session `id` only. A card links to them (§8.1). They work after the agent
is deleted.

| Endpoint | Answer |
|---|---|
| `GET /api/sessions/{id}` | `200` session link, with `harness_session_id` and `active` |
| `GET /api/sessions/{id}/transcript?after=<seq>&limit=<n>` | `200` one page of blocks, the same shape as above |

A session link:

```json
{"id":"01M5PA0T1NSYX43DDK1M2KT8KR","agent_id":"01M5PA0T0Z0K16V6G88P3SE78W",
 "agent":"worker","project":"terrarium","harness":"claude",
 "started_at":"2026-10-24T06:36:54.197Z","ended_at":"2026-10-24T07:02:10.004Z",
 "removed":false}
```

`agent` is the agent name without the project. `removed` is `true` when the
agent is deleted. `ended_at` is absent while the session is open. Rights:
a member of the session's project reads; the operator reads all; an agent
of another project gets `403 forbidden`. These endpoints are read-only and
have no stream. The page goes through the same code as the agent's
transcript endpoint, so a filter of the blocks applies to both.

**After `agent rm`.** The session rows stay (migration 37 removed the
cascade). The delete ends each open session with `outcome` `removed`. The
daemon then copies the session file of each session to
`<state>/archive/agents/<agent ulid>/<harness>/<session ulid>/<file name>`,
before the agent dir goes to the trash. Only the session files go to the
archive, not the workspace. The transcript of a deleted agent comes from
this copy. A session with no copy (no file, or a pruned archive) gives
`404 not_found`.

**Retention.** The archive of an agent stays `TERRARIUM_SESSION_ARCHIVE_DAYS`
days. The default is the trash retention (`TERRARIUM_TRASH_DAYS`, default
7). `0` keeps the archive forever. The prune timer removes an older archive.
The session rows and the card links stay.

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
| `kind` | `agent`, `process`, `time`, `card`, or `question`. |
| `spec` | `agent`: `{agent, agent_name, until, from_status}`. `process`: `{target, session, name?, command?, run}`; `run` is true when the daemon started the command (`POST .../run`) and reads its output. `time`: `{due_at}`. `card`: `{card, column}`. `question`: `{question}`. |
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
| `question` | The question or notice (§6.8) is answered, acknowledged, or retracted. The answer or acknowledgement mail is the wake mail: the wait sends no mail of its own. |

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
agent ends it (`failed`, `lost`). A daemon restart restores host-agent
sessions (local, Docker and VMLab), their output and terminal result, and
reattaches their process waits. SSH and exe.dev runs instead end as
`interrupted (ssh transport)`. The run counts toward the 8 live exec
sessions of the agent. An `env` value `@secret:<name>[.<field>]` is a
secret that the daemon resolves from the agent's grants at the start
(§5.2, "Secrets in a run"): a `run` grant that names the target, or a
`read` grant. The value goes into the env of the remote process only, is
redacted from the output, the log, and the tail, and writes a
`secret.run` audit row with no value.

| Endpoint | Body or query | Answer | Who |
|---|---|---|---|
| `GET /api/agents/{id}/waits` | `?all=true` adds the newest 100 closed waits; `?kind=` | `200` open waits, newest first, then the closed ones | the agent, a project_admin of its project, the operator |
| `POST /api/agents/{id}/waits` | below | `201` the wait | the agent itself, the operator |
| `POST /api/agents/{id}/run` | `{"target", "cmd": [..], "name"?, "cwd"?, "env"?, "timeout"?, "reason"?, "agent"?}` | `201 {"wait", "session_id", "target"}` | the agent itself, the operator; the run uses the agent's rights |
| `GET /api/waits/{id}` | | `200` the wait | as for the list; else `404` |
| `DELETE /api/waits/{id}` | `?kill=true` requests one cleanup attempt for a `process` wait; other kinds are rejected before mutation | `200` wait plus `cleanup` for kill requests. HTTP success means accepted/retrieved, not termination. Ordinary cancel still returns `409` for a terminal wait. | as for the list |
| `GET /api/waits/{id}/log` | `?max=` bytes (at most 65536) | `200 {"wait_id", "session_id", "target", "running", "output", "end"?}` | as for the list; a wait that is not `process` is `422` |

Cleanup reporting (TER-1059):

- `wait.state` describes notification/cancellation, not whether processes or
  inherited resource locks survive. `GET /api/waits/{id}` exposes the same
  persisted `cleanup` after the wait becomes terminal.
- `cleanup` contains `operation_id`, `status`, `reason`, `requested_at`,
  `updated_at`, `evidence_kind`, and optional bounded `diagnostic` counts.
  Counts from legacy reports are untrusted diagnostics, not owned identities.
  Raw errors, paths and PIDs are not stored in cleanup diagnostics.
- Current statuses are `pending`, `unknown`, and `unavailable`. No legacy
  report, including an empty list, can produce `remaining` or
  `verified_terminated`. Missing, null, malformed or error reports cannot
  prove cleanup. Ordinary cancellation with no intent is `not_requested` on
  GET. A kill retry without recorded intent is `unavailable/no_recorded_intent`
  with a null operation ID.
- The open-to-cancelled CAS and unique intent insert commit together before
  control. The daemon claims a live owned control route, durably records
  dispatch with status `unknown`, and attempts control at most once. Report collection has a 12-second
  deadline and survives an HTTP disconnect. A live owned route without a report
  recipe still receives one control request and reports `unknown/control_sent_no_report`,
  never verified termination. A failed dispatch/receipt write
  leaves conservative durable state. Startup marks orphaned queued operations
  unavailable and dispatched ones unknown. It never replays a kill.
- Concurrent or terminal kill retries read the same operation. Legacy terminal
  waits and ordinary cancelled waits never acquire fresh kill authority. Each
  request uses the wait's existing authorization, including project-admin
  delegation. The operation ID is not a bearer capability. Project, owner,
  logical session and target snapshots cannot be rebound and are attribution,
  not physical kill selectors.
- `terrarium run kill` prints wait and cleanup status separately and emits JSON
  before returning nonzero unless termination is actually verified. The current
  legacy route cannot provide that proof. Missing/old-server cleanup is also
  nonzero. Inspect the same wait operation rather than starting a fresh kill.
  Ordinary wait cancellation retains its success semantics. Persisted physical
  identities, trusted termination receipts and host update lifetime are TER-1060.

The create body:

```json
{"kind":"agent","agent":"w1","until":"done","timeout":"2h","reason":"merge after it"}
{"kind":"time","for":"30m"}
{"kind":"time","at":"2026-10-27T15:00:00Z"}
{"kind":"process","session":"01JC..."}
{"kind":"card","card":"TER-3","column":"done"}
{"kind":"question","question":"01JQ..."}
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
- `question`: one of the waiter's own questions (else `404`). A question
  that is not open is `409 conflict`.
- Each create writes a `wait.create` audit row; each cancel writes
  `wait.cancel`.

A **watch** mails its watcher when the watched agent goes to one of its
statuses. The watcher is an agent or `operator` (the Inbox of the
dashboard, §6.9).

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

### 6.8 Questions and notices to the operator

PLAN §12e gives the design. The code is in
`crates/terrariumd/src/store/questions.rs` and
`crates/terrariumd/src/api/questions.rs`.

A **question** is a request from a leader agent (a project_admin or a
sysadmin) for a decision of the operator. The question is on the agent. A
new session or a compaction reads it again with `terrarium question ls`,
and the recap turn of a new session lists it (PLAN §18). The operator
answers it in the dashboard (the Inbox, §6.9) or with `terrarium admin
question answer`. The answer goes to the agent as mail.

A **notice** (`kind: "notice"`) is a message from a leader to the operator
that needs no decision: a milestone, a finished deploy, or a risk. It has
no options. The operator acknowledges it, with an optional reply. The
acknowledgement always goes to the agent as one mail. A notice is a row of
the same table, with the same fields, rights, limits, and endpoints, except
for the differences below.

A **bundle** (`kind: "bundle"`) is a group of 2 to 20 related questions
(`items`) under one summary (`text`). The operator answers all items in
one step, and the agent gets all answers in one mail. A bundle is a row of
the same table, with the rights, limits, and endpoints of a question,
except for the differences in "Bundles" below.

**Checkbox questions.** Set `multi_select: true` when asking a question,
or on an individual bundle item. It defaults to false and cannot be set
on notices, tool approvals, or the bundle summary. The CLI flags are
`question ask --multi-select` and `thread ask --multi-select`. For bundles,
use `"multi_select": true` on each desired item in `--items-file`.

Answer a checkbox question with `{"options": [0, 2], "answer": "notes"}`.
Each entry is a 0-based index or an exact option label. Duplicate and
unknown entries are rejected. `option` (singular) is only for single-choice
questions. Both the list and text may be empty, including `{"options": []}`
or `{}`. Bundle answers use the same fields on each entry in `items`.
The response retains the text in `answer` and the sorted selected indices
in `answer_options`. The answer mail and CLI show both choices and text.
The dashboard renders checkboxes and a text box in every question view.
CLI answers use repeatable `--select <label|number-from-1>` or `options`
arrays in `--answers-file` (numbers there are also from 1). With no text or
`--select`, `admin question answer <id> --select-none` submits an empty checkbox answer.

```json
{"id":"01JQ...","agent":"01M5...","agent_name":"demo/lead","project":"demo",
 "text":"Ship v2 on Friday?","options":["yes","no"],"context":"**CI** is green.",
 "card":"DEM-3","priority":"urgent","status":"answered","answer":"yes",
 "answer_option":0,"answered_by":"operator","answered_by_name":"operator",
 "answered_at":"2026-10-28T11:00:00.000Z","thread_id":"01JT...",
 "created_at":"2026-10-28T10:00:00.000Z","updated_at":"2026-10-28T11:00:00.000Z",
 "revision":1}
```

| Field | Meaning |
|---|---|
| `kind` | `question`, `notice`, `bundle`, or `tool_run`. Set at the ask; it does not change. A `tool_run` question is the approval of one tool run (§17.11, "Approval"): only the daemon asks it, and it has `tool_run` and `used_at`. |
| `text` | The question (the summary of a bundle), at most 2000 characters. The agent's own words: untrusted data. |
| `options` | Short choice labels: at most 10, each at most 200 characters, no two the same. `[]` for a free answer. A notice and a bundle have none: options on a notice or a bundle are `422 invalid`. |
| `items` | A bundle only: the questions, 2 to 20, each `{text, options, context?, answer?, answer_option?}`. Absent for a question and a notice. See "Bundles". |
| `context` | Optional background as Markdown, at most 16 KiB. The dashboard shows it through the DOMPurify sink only. |
| `card` | A card of the agent's project. Required for a question or bundle unless asked inside an Inbox thread, even at urgent or critical priority. The question shows on that card on the board (§8) and in the Inbox. Answers are also saved as system comments on the card. Link or unlink later with `PATCH` (see "Cards"). |
| `priority` | `normal`, `urgent`, or `critical`. Lists put the critical open questions first, then the urgent ones. Use `critical` only when the operator must act now: production is down, data is lost, a security problem, or a spent budget that stops all work. While a critical item is open, the dashboard shows a banner on each page and flashes its browser tab (§6.9). |
| `status` | A question: `open`, `in_thread`, `answered`, or `retracted`. A notice: `open`, `in_thread`, `acknowledged`, or `retracted`. Only `open` and `in_thread` change. `in_thread`: the operator moved it into an Inbox thread with its agent (see "To thread"). |
| `answer`, `answer_option` | The answer text, and the 0-based index of the chosen option. An answer with an option and no text has the label as `answer`. For an acknowledged notice, `answer` is the reply of the operator, if any. |
| `answered_by`, `answered_by_name` | `operator`, or the ULID and `project/name` of a sysadmin agent. |
| `retract_reason`, `retracted_by` | The reason, and the actor label (`operator`, `agent:<ulid>`, or `system` for a deleted agent). |
| `thread_id` | The mail thread of the question. The answer goes on it. |
| `thread` | The Inbox thread that the question was asked in, or moved into (§6.13, "To thread"), or absent. |
| `revision` | 1 at the ask. Each update that changes a field adds 1. |

The rights:

| Action | Who |
|---|---|
| Ask, update | The agent itself, when it is a leader: project_admin of its project, or sysadmin. A member gets `403` with "workers send questions to their lead: terrarium mail send <lead> ...". The operator does not ask. |
| Read | The agent itself, a project_admin of its project, a sysadmin, the operator. Another caller gets `404` for one question and `403` for a list. |
| Answer, acknowledge | The operator. A sysadmin agent, for the question or notice of another agent. Never the asking agent (`403`). |
| Retract | The asking leader, the attached thread agent for its own thread question, or the operator. |
| To thread | The operator. Another caller that reads the question gets `403`. |
| Link to a card, unlink | The asking leader while the question is open. The operator, and a sysadmin agent for the question of another agent, at each status. |

| Endpoint | Body or query | Answer |
|---|---|---|
| `GET /api/agents/{id}/questions` | `?status=open\|answered\|acknowledged\|retracted`, `?all=true`, `?kind=question\|notice\|bundle` | `200` the questions and notices of the agent. No filter: the open ones and those answered or acknowledged in the last 24 h. |
| `POST /api/agents/{id}/questions` | `{"kind"?, "text", "options"?, "items"?, "context"?, "card"?, "priority"?, "wait"?, "timeout"?, "thread"?}`; `thread` asks in an Inbox thread (§6.13); `kind` is `question` (the default), `notice`, or `bundle` (with `items`) | `201` the question, notice, or bundle; with `wait: true`, also `wait`: the new `question` wait (§6.7) |
| `GET /api/questions` | `?status=` (default `open`; `all` for every status), `?project=`, `?agent=`, `?kind=`, `?card=` | `200` at most 200 questions and notices. `?card=` keeps the questions linked to that card; an unknown card is `404`. Open first (critical first, then urgent, then the oldest first), then the others (the newest first). `X-Open-Questions`, `X-Urgent-Questions` (urgent and critical), and `X-Critical-Questions` count the open ones of the `kind` filter (both kinds without it). The operator and a sysadmin see all; a project_admin sees its own project; a member gets `403`. |
| `GET /api/questions/{id}` | | `200` the question |
| `PATCH /api/questions/{id}` | `{"text"?, "options"?, "items"?, "context"?, "card"?, "priority"?}`; `null` (or `[]` for `options`) clears a field; `items` replaces all items of a bundle. `{"card": "<id>" \| null}` alone links or unlinks a card (see "Cards") | `200` the question. A question that is not open is `409 conflict`, except for a change of the card by the operator or a sysadmin. A card of another project, or an unknown card, is `422 invalid`. |
| `POST /api/questions/{id}/answer` | `{"answer"?, "option"?, "revision"?}`: a text, an option (its label, or its 0-based index), or both. A bundle: `{"items": [{"answer"?, "option"?}, ...], "revision"?}` | `200` the question. A question that is not open is `409 conflict`. A notice is `409 conflict`. A `revision` that is not the current one is `409 conflict`, with `details.revision`. |
| `POST /api/questions/{id}/ack` | `{"text"?}`, or no body | `200` the notice, `acknowledged`. A notice that is not open is `409 conflict`. A question is `409 conflict`. |
| `POST /api/questions/{id}/retract` | `{"reason"?}`, or no body | `200` the question. A question that is not open or `in_thread` is `409 conflict`. |
| `POST /api/questions/{id}/to-thread` | `{"message"?}` (at most 8000 characters), or no body | `201` `{"question", "thread"}`: the question, `in_thread`, and the new thread (§6.13). A bundle, a tool run approval, a hold question, a question that is not open, and a question asked in a thread are `409 conflict`. |

The rules:

- A notice needs `card`, except when `priority` is `critical`.
  Otherwise it is `422 invalid`, including urgent notices and replies.
  Post in an open topic thread instead, or a lead opens one.
  The default Inbox lists only open questions and critical notices. Explicit
  kind and status queries remain available for history and diagnostics,
  reached only by an explicit UI filter, never by default.
  `POST /api/projects/{project}/version-thread/messages` takes
  `{"version", "changed", "test"}` and requires project admin rights.
  It creates or reuses the project's standing Versions thread and posts one
  quiet update (no Threads badge), with the lead's changes and test guidance.
  All three fields must be nonempty. A repeated version is `409 conflict`.
  The standing Versions thread cannot be closed, including by admins:
  closing or proposing its close is `409 conflict` naming the thread.
  Post at each pre-release publish, alongside the releases wiki row.
  Version group completion posts nothing.
  `reply_to` (notices only) names an
  operator mail or Inbox-thread message to the sender, or an answered
  question owned by the sender. Invalid references are `422 invalid`.
  The reply reference is recorded in the ask audit event.
- Several independent questions and bundles may be open on the same card
  or Inbox thread. Each answer wakes the agent, while the thread waits for
  the operator until its last open block is settled. The Inbox groups the
  open questions of a card.
- An agent has at most 50 open questions (`429 rate_limited`). A question
  whose text is the same as an open question of the agent (case and
  spaces do not count) is `409 conflict`; `details.question` names the
  open one.
- The answer is one mail from the answerer (`operator`, or the sysadmin
  agent) to the agent, on the thread of the question. The subject is
  `Answer: <the first line of the question>`. The body quotes the
  question, and gives the chosen option and the answer. The mail wakes the
  agent as all mail does (§6.2, push delivery).
- A retract by the operator sends one mail on the thread (`Retracted:
  ...`), with the reason. A retract by the agent itself sends no mail.
- The acknowledgement of a notice is always one mail from the operator
  (or the sysadmin agent) to the agent, on the thread of the notice, also
  with no reply text. The subject is `Acknowledged: <the first line of the
  notice>`. The body quotes the notice and the reply. The mail wakes the
  agent.
- A `question` wait (§6.7) resolves when its question is answered
  (`{outcome: "answered", answer, option?, question}`), its notice is
  acknowledged (`{outcome: "acknowledged", answer?, kind: "notice",
  question}`), or either is retracted (`{outcome: "retracted", question}`). It sends no wake mail of its own:
  the answer mail (or the retract mail of the operator) is its wake mail,
  so the agent gets one mail, not two. The mail says that the wait is
  resolved. A timeout of the wait sends the usual wake mail from `system`.
- A deleted agent: its open and `in_thread` questions are `retracted`
  (`retracted_by: "system"`), with no mail.
- An answer, an acknowledgement, and a retract settle an `in_thread`
  question as an open one. An update of it is `409 conflict`.
- The open questions and notices of one agent share the limit of 50. The
  copy check compares a new text with the open items of the same kind.
- The recap turn of a new session lists the open questions, the open
  notices, the answers since the last turn, and the acknowledgements since
  the last turn, with the replies.
- Each change writes an audit row (`question.ask`, `question.update`,
  `question.answer`, `question.ack`, `question.retract`,
  `question.to_thread`), a `question`
  event: `{question_id, kind, change, status, priority, text, revision,
  agent_id, card_id?, previous_card_id?}`, where `change` is `asked`,
  `updated`, `answered`, `acknowledged`, `retracted`, or `to_thread`, and an `inbox`
  event (§6.9). The event has the top-level `card` of the linked card.

Cards:

- A question, a bundle, or a notice links to at most one card of its
  project (`card`). The board shows the open ones on their card, and the
  card drawer lists all of them (§8). `GET /api/questions?card=<id>&status=all`
  reads the questions of a card: the open ones first, then the others,
  the newest first.
- `PATCH /api/questions/{id}` with only `card` links the question to a
  card, or unlinks it with `"card": null`. The revision stays, so an answer
  in progress stays valid. The asking leader changes the card of an open
  question only. The operator and a sysadmin change it at each status.
  A question or bundle without an Inbox thread cannot be unlinked, at any
  priority or status (`422 invalid`). A full update enforces the same rule.
- A change of the card writes the audit row `question.update` with
  `changed: ["card"]`, `card`, and `previous_card`, and an `updated`
  event with `previous_card_id` (the old card), so that the old card
  updates too. A `PATCH` with the same card changes nothing.
- The ask (`POST /api/agents/{id}/questions`) and a `PATCH` give `422
  invalid` for a card of another project, or an unknown card.
- Changing the card in a `PATCH` gives `409 conflict` if another open
  question or bundle of the asking agent shares the new card or Inbox
  thread. The operator and sysadmins may relink despite this conflict.
  Missing required work context gives `422 invalid`. Text-only edits and
  unchanged cards do not recheck legacy context or duplicate questions.
  A notice losing its card must be critical (`422 invalid` otherwise),
  including when its priority changes.

Bundles:

```json
{"id":"01JQ...","kind":"bundle","agent_name":"demo/lead","project":"demo",
 "text":"Release plan for v0.3","options":[],"context":"CI is green.",
 "items":[{"text":"Tag on Friday?","options":["yes","no"],"answer":"no","answer_option":1},
          {"text":"Which staging host?","options":["eu","us"],"context":"`us` has the newer kernel.",
           "answer":"us, for the kernel","answer_option":1},
          {"text":"Release notes?","options":[],"answer":"Mention the PTY fix."}],
 "priority":"normal","status":"answered","answered_by":"operator", "...": "as a question"}
```

- An item has a `text` (at most 2000 characters), `options` (the rules of
  the options of a question), and an optional `context` (Markdown, at most
  4 KiB). No two items have the same text. A bundle has 2 to 20 items. A
  bundle with no items, one item, or more than 20 is `422 invalid`, and so
  are `items` on a question or a notice. The message names the item.
- The answer of a bundle takes `items`: one entry for each item, in order.
  Each entry has `answer` (text), `option` (a label of the item, or its
  0-based index), or both. `answer` or `option` at the top is `422`. An
  item with no answer is `422 invalid`: the message names the items
  (`items 2 and 3 of 3 have no answer ...`), `details.missing` lists their
  numbers from 1, and nothing changes. A full answer is one write: the
  status becomes `answered`, each item gets `answer` and `answer_option`,
  the revision stays, and one `answered` event follows.
- The answer mail is one mail on the thread of the bundle. The subject is
  `Answers: <the first line of the summary>`. The body quotes the summary,
  then gives each item: `Item 1 of 3:`, the first line of the item quoted
  with `> `, `Chosen option: 2. no`, and `The answer of operator:` with the
  answer quoted. The label alone is not given again as an answer. As for
  each mail, the delivery quotes the body and defuses each line that reads
  as a tag (`[\User]`, §6.2).
- A `question` wait on a bundle resolves with `{outcome: "answered", kind:
  "bundle", question, answers: [{item, text, answer, option?}, ...]}`.
- `PATCH` with `items` replaces all items of an open bundle and adds 1 to
  the revision. The answers in progress in the dashboard are then for the
  old revision: the dashboard sends `revision`, and gets `409 conflict`.
- A retract ends the whole bundle. An acknowledgement of a bundle is `409
  conflict`.
- `?kind=bundle` lists the bundles. In the Inbox (§6.9), a bundle is an
  item of the kind `question` with the ID `bundle:<id>`, and it counts as
  one question.
- The recap turn lists an open bundle as `<id> [bundle of N items]`, and
  an answered bundle item by item.

To thread (GitHub #30, TER-469):

- The operator moves an open question or notice into a new Inbox thread
  with the asking agent: the Inbox action "Open in thread" (GitHub #32), `POST
  /api/questions/{id}/to-thread`, or `terrarium question to-thread <id>
  [-m <message>]` (also `terrarium admin question to-thread`).
- The thread (§6.13) is in the project of the question, with the asking
  agent attached, the operator as its opener, no template, and
  `from_question` set to the question. Its first message, by the operator,
  quotes the question, its options, and its context, and then gives the
  optional `message`. It is `waiting_agent`. A question with a card links
  the card to the thread (an archived card is not linked). The quotes add
  to the size: when the first message has more than 32 KiB, the move is
  `422 invalid`, with `bytes` and `max` in the details, and nothing moves
  (TER-495).
- The question becomes `in_thread`, with `thread` set: it is not open, so
  it leaves the open counts, the Inbox, and the `X-Open-Questions` count,
  and the thread item stands for it. Its `question` waits stay open.
  In its thread it counts as an open question (TER-495): the agent sets no
  `waiting_agent` or `tracking` (`409 conflict`), a message of the agent
  makes the thread `waiting_operator`, and a close retracts it. The
  operator's `POST .../status` `waiting_agent` passes it (TER-514).
- The agent gets one mail from the operator on the mail thread of the
  question (`In a thread: ...`), with the commands to read the thread and
  reply there. The agent is not bound to the thread, so each later
  message and response of the operator in the thread also comes to it as
  mail (`Thread: ...`), on the same mail thread.
- In the thread the operator answers (or acknowledges) the question with
  the usual verbs, and it becomes `answered` (or `acknowledged`) with its
  `thread` link. The agent retracts it, and can ask a revised question in
  the thread (`terrarium thread ask <thread> ...`). A close of the thread
  retracts it, as an open question of the thread.
- The recap turn lists the `in_thread` questions with their threads.
- A bundle does not move (`409 conflict`): that is a follow-up.
- In the dashboard (GitHub #32), "Open in thread" on a question or notice
  with a linked thread opens that thread, also after the answer. On an
  open one with none it moves it, as above, and opens the new thread.
  "Show mail" opens the mail thread of the question (`thread_id`). The
  thread shows the question in its conversation.

### 6.9 The Inbox

PLAN §12e gives the design. The code is in
`crates/terrariumd/src/store/inbox.rs` and
`crates/terrariumd/src/api/inbox.rs`.

The Inbox is the list of the things that wait for the operator. It is a
view, not a table: it copies no mail body. Each item has one of six
kinds:

| Kind | What | Open while | The operator's verbs |
|---|---|---|---|
| `question` | A question or a bundle of questions (§6.8). | `status` is `open`. | `POST /api/questions/{id}/answer`, `.../retract` |
| `notice` | A notice (§6.8). | `status` is `open`. | `POST /api/questions/{id}/ack`, `.../retract` |
| `mail` | A message to `operator` (§7). | Its `state` is `queued` or `delivered`: not acknowledged. | `POST /api/inbox/mail/{id}/ack`; it closes as well when the mail is read or replied outside the Inbox (§7): `POST /api/mail/{id}/read`, `.../reply` |
| `system` | A thread of the loop guard (§7.4): it is paused now, or it has a held message that is not released. | The thread is paused, or a held message is not released. | `POST /api/mail/{id}/release`, `POST /api/mail/threads/{thread}/resume` |
| `watch` | A question that the operator asked an agent, or a question of the Inbox that the operator forwarded to one (below, "Ask and forward"). It waits for the agent, not for the operator. | The newest message of its thread is the operator's, and it is not cancelled. | `DELETE /api/inbox/watches/{thread}` |
| `thread` | An Inbox thread (§6.13), with its questions inside. A question asked in a thread is not an item of its own. | The thread is `waiting_operator`, or it is not closed and has an open question. | `POST /api/threads/{id}/messages`, `.../respond`, `.../close`, and the question verbs |

The notice of a pause (`Thread T needs a human`, from `system`) is not a
`mail` item: the `system` item of the thread shows it.

```json
[{"id":"notice:01JQ...","kind":"notice","open":true,"urgent":true,"critical":false,"project":"demo",
  "at":"2026-10-28T10:00:00.000Z","question":{"id":"01JQ...","kind":"notice", "...": "§6.8"}},
 {"id":"mail:01M5...","kind":"mail","open":true,"urgent":false,"critical":false,"project":"demo",
  "at":"2026-10-28T09:00:00.000Z","message":{"id":"01M5...", "...": "§7"}},
 {"id":"thread:01JT...","kind":"system","open":true,"urgent":false,"critical":false,"project":"demo",
  "at":"2026-10-28T08:00:00.000Z",
  "system":{"kind":"paused_thread","thread_id":"01JT...","subject":"loop","paused":true,
            "agent_run":7,"limit":6,"participants":["demo/w1","demo/w2"],
            "held":[{"id":"01M6...", "...": "§7"}]}}]
```

| Field | Meaning |
|---|---|
| `id` | `question:<id>`, `bundle:<id>`, `notice:<id>`, `mail:<message id>`, `thread:<thread id>`, `watch:<thread id>`, or `inbox-thread:<id>`. |
| `open` | The item waits for the operator. |
| `urgent` | An open question or notice with `priority: urgent` or `critical`, or an open thread with such an open question. Mail and system items are not urgent. |
| `critical` | An open question or notice with `priority: critical`. The dashboard shows the newest one in a banner on each page, with a link to `/inbox?item=<id>`, and flashes its browser tab while the page is hidden or has no focus. |
| `project` | The project of the asking agent, of the sender of the mail, of the first agent on the thread, or of the watched agent. `null` for mail from `system` or from the operator. |
| `at` | When the item came in: the ask, the send, the first held message that is not released, or the ask of a watch. |
| `question`, `message`, `system` | The question or notice, the message, or the thread. A system item has `paused`, `agent_run` (the agent-to-agent messages since the last reset), `limit` (`TERRARIUM_MAIL_LOOP_LIMIT`), `participants`, and `held` (the held messages that are not released). |
| `watch` | On a `watch` item: the watch (below). On a `mail` item whose thread is watched: the watch that the mail answers, so the dashboard can show "answer to your question" and link the forwarded question. Absent otherwise. |
| `thread` | On a `thread` item: the thread as `GET /api/threads` lists it (§6.13), with `message_count`, `open_questions`, and `last_at`. |

| Endpoint | Body or query | Answer |
|---|---|---|
| `GET /api/inbox` | `?status=open\|done\|all` (default `open`), `?kind=question\|notice\|mail\|system\|watch\|thread`, `?project=` | `200` the items. The open ones first, then critical first, then urgent, then the newest first. Thread items show only with `kind=thread`: a list with no `kind` leaves them out (TER-466). `X-Inbox-Open`, `X-Inbox-Urgent`, and `X-Inbox-Critical` count the open items of the `project` filter (all kinds but `watch` and `thread`). System items show only while they are open; the audit log keeps their history. |
| `GET /api/inbox/counts` | `?project=` | `200` `{"open", "urgent", "critical", "question", "notice", "mail", "system", "watch", "thread", "thread_urgent", "thread_critical"}`: the open items of each kind. `urgent` counts the urgent and the critical items; `critical` counts only the critical ones. `open` does not count `watch`: a watch waits for an agent. `open`, `urgent`, and `critical` do not count the threads either: `thread` counts the open thread items, `thread_urgent` the ones with an urgent or critical open question, and `thread_critical` the ones with a critical one (TER-466). The dashboard Inbox badge shows `open`, red when `urgent` is not 0; the Threads badge shows `thread`, red when `thread_urgent` is not 0. |
| `POST /api/inbox/mail/{id}/ack` | `{"reply"?}`, or no body | `200` `{"message", "replies"}`. The mail becomes `read`. With `reply`, the reply goes as `operator` on the thread of the mail to its other participants (§7.2), and the mail becomes `replied`. A message that is not to the operator is `404`. One that is acknowledged already is `409 conflict`. A reply to a system mail that has nobody to answer is `422 invalid`. |

| `POST /api/inbox/asks` | `{"to", "text"?, "question"?}` | `201` `{"message", "watch"}`: the mail (§7) and the new watch. Below. |
| `DELETE /api/inbox/watches/{thread}` | | `200` the watch, with `cancelled_at`. An unknown thread is `404 not_found`; a cancelled watch `409 conflict`. The mail stays. |

The Inbox list accepts `limit` (1–400, default 200) and `cursor`. To request
the next page, URL-encode the JSON tuple `[open, critical, urgent, at, id]`
of the last item as `cursor`, with the same filters. An empty page ends the
list. The ordering uses descending ID to break timestamp ties. New items
above a cursor do not shift older pages, and removing the cursor item does
not invalidate it. Badge headers always count every matching open item,
independent of the page. Mail bodies are loaded only for the selected page.

**Ask and forward** (GitHub #24). The operator asks an agent a question
from the Inbox, or forwards a question, a bundle, or a notice of the Inbox
to an agent, and the Inbox watches the thread until the agent answers.

- `POST /api/inbox/asks` with `to` (the agent: a ULID, `project/name`, or
  a name) and `text` (at most 4000 characters) asks a question. With
  `question` (the ID of a question, a bundle, or a notice) it forwards that
  one, and `text` is an optional note. A `to` that is not an agent, or an
  ask with an empty `text` and no `question`, is `422 invalid`. An unknown
  agent or question is `404 not_found`.
- The daemon sends one mail from `operator` to the agent, on a new thread.
  An ask has the subject `Question from the operator: <first line of
  text>` (cut at 80 characters) and the text as its body. A forward has the
  subject `Forwarded <question|bundle of questions|notice> from
  <project/agent>: <first line>`, and a body with the note, then the
  question quoted with `> `: the text, the numbered options, each item of
  a bundle with its options, and the context (cut at 4 KiB). Each body
  ends with how to answer: `terrarium mail reply <this message>`. The
  agent gets the mail as all mail (§12, push). The forwarded question stays
  `open`; the operator still answers it with its own verbs.
- The watch: `{"thread_id", "agent", "agent_name", "question_id"?,
  "message_id", "subject", "created_at", "cancelled_at"?, "waiting",
  "replies"}`. `agent` is the ULID and `agent_name` is `project/name`.
  `message_id` and `subject` are those of the ask. `waiting` is true while
  the newest message of the thread is the operator's. `replies` counts the
  messages of the agent on the thread. The `watch` item is open while
  `waiting` is true and `cancelled_at` is absent.
- The agent's answer (`terrarium mail reply`) is a mail to `operator`: a
  `mail` item with Acknowledge and the optional reply, and a `watch` field.
  It closes the watch. A reply of the operator on the thread (the
  acknowledgement with a reply) opens it again, so a follow-up question
  of the agent and the operator's next response go round the same way.
- Events: `inbox` with `kind: "watch"`, `change: "opened"` at the ask
  and at each reply of the operator, and `"closed"` at the agent's answer
  and at a cancel of an open watch. Audit rows: `inbox.ask` (the thread,
  with `to`, `question`, and `message`) and `inbox.watch_cancel`.

Only the operator reads the Inbox and acts on it. An agent gets `403`,
with a message that sends it to `terrarium question ls`.

Each change of an item emits an `inbox` event: `{item, kind, change,
project?}`, where `change` is `opened`, `updated`, or `closed`. A `mail`
item closes on every path that acknowledges its message: the ack, a read,
a reply, wherever the operator does it. The acknowledgement of a mail
writes an `inbox.ack` audit row.

### 6.10 Ephemeral agents

PLAN §18 ("Ephemeral agents") gives the design. The code is in
`crates/terrariumd/src/spawn.rs`, `crates/terrariumd/src/store/spawns.rs`,
and `crates/terrariumd/src/api/spawns.rs`.

An ephemeral agent does one task for its owner, and then the daemon
closes it. The owner is the agent that spawned it, or `operator`. The
daemon makes it from a template in the project of the owner, with its own
workspace and no idle stop, and types the task as its first turn. The
turn names the owner (`[Agent:<project>/<owner>]`, or `[User]` for the
operator). The system prompt of an ephemeral agent tells it its owner,
its lifetime, and to end with `terrarium agent done -m "<result>"`.

**Spawn rights.** The policy block `spawn` of an agent (§6.1, `policy`)
gives the right to spawn. Without it, an agent cannot spawn.

```json
"policy": {"targets": [], "mail_to": ["*"],
           "spawn": {"templates": ["worker", "global:helper", "project:*"],
                     "max_concurrent": 2, "max_per_day": 20, "max_lifetime": "2h",
                     "may_spawn_children": false}}
```

| Field | Meaning |
|---|---|
| `templates` | The templates that the agent may spawn: a name (the project's template, else the global one), `global:<name>`, `project:*` (each template of the project), `global:*` (each global template), or `*`. |
| `max_concurrent` | The ephemeral agents of the owner that may run at one time: 1 to 50. Default 2. |
| `max_per_day` | The spawns of the owner in the last 24 hours, running or finished. Absent: no limit. |
| `max_lifetime` | The lifetime of each ephemeral agent: a duration such as `30m` or `2h`. Default `2h`, at most `24h`. |
| `may_spawn_children` | The ephemeral agents may spawn too. Each one gets a copy of this block with `may_spawn_children: false`, so it goes one level down only. Default false. |
| `bound` | The agent may bind an ephemeral agent to a card (`bind`, below). Default false. A child never gets it. |
| `max_bound_lifetime` | The cap of the life of a bound ephemeral agent: a duration such as `12h` or `3d`. Default and at most `30d`. |

Who writes the block (`PATCH /api/agents/{id}` with `policy`, or the
create): the operator and a sysadmin on any agent. A project_admin on a
member of its project (an agent with no role above `member`), with
templates of the project or global ones, but not on itself, not on an
admin, and not `may_spawn_children: true` or `bound: true`. A write that does not change
the block needs no right. A bad block is `422 invalid`. The `agent.update`
audit row has the new policy. A template carries no spawn rights: the
template files have no `spawn` key, and a template made from an agent
leaves it out.

**The lists.** `GET /api/agents` and `GET /api/projects/{name}/agents`
leave the ephemeral agents out. `GET /api/agents?include=ephemeral` lists
them too. `GET /api/agents?include=card` lists only the card agents among
them (TER-75): those bound to an open card, and those that are the
assignee of a card that is not archived. `terrarium agent ls` asks with
`include=card` by default, and with `include=ephemeral` for
`--ephemeral`, also with `--all-projects`. They are not peers in the prompt of other agents. The events,
the mail, the audit rows, and the session links of the logs show them as
for any agent.

| Endpoint | Body or query | Answer |
|---|---|---|
| `POST /api/agents/{owner}/spawn` | `{"template", "task", "name"?, "wait"?, "timeout"?, "project"?, "targets"?, "grants"?, "bind"?}` | `201` the spawn (below), with `agent` (the agent object) and `wait` (the wait, when one was made). |
| `GET /api/agents/{owner}/spawned` | `?limit=&cursor=`; for `operator`: `?project=` | `200` the spawns of the owner, running and finished, newest first, paged (§2.4). |
| `GET /api/spawns/{id}` | | `200` one spawn, also after the finish. A reader of its project reads it; a spawn of `operator`: the operator only. |
| `POST /api/agents/{id}/done` | `{"result"?}` | `202` the spawn. The close follows after 3 s. |
| `POST /api/agents/{id}/cancel` | | `200` the spawn, closed with the outcome `cancelled`. |

`{owner}` is an agent (ULID, `project/name`, or a name), or `operator`.
For `operator`, the body needs `project`; only the operator spawns as
`operator`.

**Spawn.** The owner itself, a project_admin of its project, a sysadmin,
or the operator may spawn for an owner. The checks, in order:

- The owner has a `spawn` block: else `403 forbidden` ("has no spawn
  rights"). The operator spawns for any agent with no block.
- The task is not empty and has at most 64 KiB; `name`, when given, is a
  valid agent name: else `422 invalid`.
- The template exists in the project or among the global templates: else
  `404 not_found`. It is in `templates`: else `403 forbidden`, with the
  allowed list. The operator is not bound to the list or the counts.
- The running ephemeral agents of the owner are fewer than
  `max_concurrent`, and its spawns of the last 24 hours fewer than
  `max_per_day`: else `409 conflict`, with the name of the limit.
- The roles and the policy of the template need the rights of a create by
  the caller (§6.1); a disabled subscription is `422 invalid` (§16.2).
- Each of `targets` and `grants` is a list of at most 32 entries; an
  empty entry or a bad grant is `422 invalid`. Each entry needs a right of
  the owner (below), also when a lead or the operator spawns for it.

**Rights from the owner** (TER-65, `crates/terrariumd/src/spawn_rights.rs`).

`like` (CLI `--like <agent>`) copies a same-project agent's policy targets,
agent-level secret grant references (never values), skills settings, subscription
and pool: the copied subscription and pool replace the template's and the
owner's, and subscription selection ranks the copied pool by estimated usage.
The template still supplies the harness and other settings. The source must
use the same harness; a mismatch is `422 invalid` and names the template,
also when it is the alternative selected after an eligibility refusal. Each
copied target and grant is checked against the owner through the normal
spawn-rights checks, and a
copied target that the body's `targets` also names is granted once. A missing
right is `403`. A copied target is named. A copied grant is named (secret
and level) only when the owner may already read the secret's name (a grant of
its own, or the operator); the others are counted in one `403` with no name,
which points at `like_skip_missing`. With
`like_skip_missing: true` (CLI `--like-skip-missing`, requires `like`), those
forbidden rights are omitted and listed in the response's `skipped` array:
`target:<name>` and `grant:<scope>/<name>:<level>`, the grant entries only
for secrets whose name the owner may already read (a grant of its own, or
the operator, which reads every name). Skipped grants on secrets that the
owner may not read are counted in one `grant:<n> hidden: the owner may not
read their names` entry, so the list does not expose the source's grants.
Other errors still fail the spawn. Explicit and template grants replace copied
grants of the same secret. Project grants and leases of the source are not copied.

A spawn gives the ephemeral agent rights beyond its template, each one
checked against the owner at the spawn. None goes past what the owner
holds.

| Field | Entry | The ephemeral agent gets |
|---|---|---|
| `targets` | A target name: global, or of the project. | A target in the owner's `policy.targets` goes into its `policy.targets`, also a global one that a project_admin could not add by `agent set`. A target that the owner holds by an active lease gives it a lease (`target.grant`) that ends with the owner's lease or its own deadline, whichever is first. An exclusive lease of the owner is `409 conflict`. Any other target is `403 forbidden`, with the target named; an unknown one is `404`. |
| `grants` | `<name>` or `<scope>/<name>` (the project secret first, then the global one), with `:use`, `:run`, `:run=<target>,<target>`, or `:read`. | An agent grant (`secret.grant`, with `spawn_owner`) at that level, when the owner holds the secret (its own grant or its project's) at that level or higher (`use` < `run` < `read`): else `403 forbidden`, with the secret and the levels named. No level: the kind's default, at most the owner's. A `read` grant keeps the owner's env var name. A `run` grant (§5.2, "Secrets in a run") does not go past the owner's targets: an owner with `run` gives its own run targets, or the subset that the ask names (a target past them is `403`); an owner with `read` names the targets (`422` with none), each one in its `policy.targets` or held by an active lease (`403` otherwise). The operator names any target that the project reaches. |

A template with no `subscription` and no `subscription_pool` takes the
owner's subscription and pool when the owner runs the same harness, so
the quota switch (§16.3) covers the ephemeral agent. The operator as owner
holds every right: a target or secret needs only to exist and be in reach
of the project.

**Template grants** (TER-88). The `grants` of the template (§21.1) go
through the same owner check as the `grants` of the body, with the same
syntax, levels, and `run` targets. A refusal is `403 forbidden` (or the
`422` or `404` above), and its message starts with `template `<name>`:`
and names the secret. A grant of the body replaces the template's grant
of the same secret, so a spawn can ask for a lower level than the
template, compared by the resolved secret (a template `gh` that resolves
to `p/gh` stays next to a body `global/gh`). Each `secret.grant` row of a
template grant has `template`. The daemon then checks again that the one
who last wrote the template (`grants_by`, §21.1) may grant each secret
now (§5.2): else `403`, with the template, the secret, and `grants_by`
named. With the operator as owner, a template grant `:run` with no targets
is `422`: the operator has no run targets to give.

The audit row `agent.spawn` has `rights` (`targets`,
`leases`, `grants`, no value), `subscription`, and `subscription_pool`.
The close revokes the leases of the ephemeral agent, and the removal of
its row deletes its grants, so no right outlives it.

The name is `<owner>-<template>-<n>`, with the lowest `n` that no agent
and no spawn of the project has, unless the body gives `name`. The agent
starts at once (a wake, §6.2); the stream limit of its subscription
applies as for any agent. A start that fails closes it with the outcome
`failed`, and the reason as the result.

With `"wait": true`, and only when the owner itself spawns, the owner gets
an open wait of the kind `agent` with the spec `{"spawn", "name", "until":
"finished"}` and the summary `spawned <name> finishes`. Its timeout is
`timeout` (a duration or an RFC 3339 time, as in §6.7), else the lifetime
and one minute. The close resolves it with `{"outcome", "result",
"session_id", "duration_secs", "name", "spawn"}`, and the wake mail of the
wait (§6.7) carries the result. Then no other mail goes to the owner.

**The spawn object.**

```json
{"id":"01M6...","owner":"01M5...","owner_name":"lead","project":"demo","name":"lead-worker-1",
 "template":"global:worker","harness":"claude","task":"Find the bug in foo.rs\nand fix it",
 "summary":"Find the bug in foo.rs","spawned_by":"agent:01M5...",
 "spawned_at":"2026-11-02T10:00:00.000Z","deadline_at":"2026-11-02T12:00:00.000Z",
 "wait_id":"01M6...","status":"finished","finished_at":"2026-11-02T10:04:12.000Z",
 "outcome":"done","result":"Fixed in foo.rs:42; branch fix/foo","session_id":"01M6...",
 "duration_secs":252}
```

| Field | Meaning |
|---|---|
| `id` | The ULID of the ephemeral agent. It stays after the finish. |
| `template` | The template as the spawn named it: a project template by name, a global one as `global:<name>`. |
| `summary` | The first line of the task, at most 120 characters. |
| `status` | The live status of the agent, or `finished`. |
| `outcome` | `done`, `failed`, `timeout`, `owner_gone`, or `cancelled`. Absent while it runs. |
| `result` | The result, at most 16 KiB (the daemon redacts secrets). Absent when none. |
| `session_id` | The current session while it runs, the last session after the finish. `GET /api/sessions/{id}` (§6.6) opens it while the session archive keeps its file. |
| `duration_secs` | From the spawn to the finish, or to now. |
| `bound` | The bound object (below), for example `card:TER-74`. Absent for a plain spawn. |
| `parked_at`, `park_reason` | When and why a bound agent is parked. Absent when it is not parked. |
| `unpark_queued_at` | When an unpark of a parked agent began to wait for a place of the owner. Absent when none waits. |

**Bound agents** (TER-74, `crates/terrariumd/src/bound.rs`). With
`"bind": "card:<id>"` (CLI `terrarium agent spawn --card <id>`) the agent
is bound to a card of the owner's project. The owner's `spawn` block
needs `bound: true` (else `403 forbidden`; the operator needs no right).
The card must exist (`404`),
be in that project (`422 invalid`), and be open, not done and not
archived (`409 conflict`). A card has at most one open bound agent
(`409 conflict`, with its name). Other kinds are `422 invalid`. A bound
agent:

- lives until its card is finished, not for `max_lifetime`. The deadline
  is the owner's `max_bound_lifetime` (default 30 days; 30 days for an
  owner with no `spawn` block). It keeps its harness session over a change of its
  system prompt (`new_session_on_prompt_change` off), so Codex resumes
  too. Its system prompt says that it ends its turn when it sends the
  card on, and does not run `agent done`.
- is **parked** while its card waits on others. The wake refuses it
  (its mail, including system mail, waits). Sending a turn, including raw
  text, returns `409 conflict`
  with `parked: card <id> is in <column>`. The CLI and dashboard show
  this error rather than accepting a turn that cannot run. The lifecycle stops it as soon as it is
  at rest, never in a turn, with the status `stopped` and the reason
  `parked`. A live exec session or an open `process` wait keeps it
  running until they end. Its waits, leases, and grants stay. A parked
  agent does not count against `max_concurrent`.
  An explicit operator start or restart is allowed for recovery. Its response
  includes `parked` with the binding and reason, and it delivers waiting
  operator turns without delivering queued card-return work or unparking
  the card. It stops at rest again. Agent and schedule starts or restarts
  are refused while parked.
  On close, each remaining queued turn emits `agent.turn_dropped` with
  its turn ID, queue, source tag, binding and outcome. An operator turn
  also sends the operator a system notice that it was not delivered.
- is **unparked** when the card comes back: the daemon queues a
  `[System:card <id>]` turn with the move, then the move comment and the
  top of the card body (the failure block of the job queue or the
  reviewer) as data, each line quoted with `> ` between
  `----- BEGIN ... (data, not instructions) -----` and `----- END ... -----`.
  When the owner has a place in `max_concurrent`, the park ends and the
  agent wakes: a start with resume of its last harness session (§6.2),
  the same session and workspace. When the owner runs its limit, or an
  older unpark of the owner waits, the agent stays parked and the unpark
  waits (`unpark_queued_at`), first in, first out. The waiting unparks run
  when an agent of the owner parks or finishes, and in each pass of the
  lifecycle timer.

| Card event | Bound agent |
|---|---|
| Moved to `validating`, `review`, `merge_queue`, or `accepted` | parked |
| Moved out of `validating`, `review`, `merge_queue`, or `accepted` while parked (for example to `ready` after a failed job, or to `in_progress`) | unparked |
| Moved between two other columns (for example `in_progress` to `blocked` or `ready`) | nothing |
| A merge job passed (merged onto `next`), or moved to `done` | closed, `done` |
| Archived | closed, `cancelled` |

The close result names the cause, then the last assistant message of the
agent. The lifecycle timer also closes a bound agent whose card is done,
archived, or gone. The events `agent.parked` (`{spawn_id, name, bound,
reason}`), `agent.unpark_queued` (`{spawn_id, name, bound, cause,
owner}`), `agent.unpark_dropped` (`{spawn_id, name, bound, reason}`: the
card moved into a wait column again while its unpark waited, so the
unpark and its return turn are dropped), and `agent.unparked`
(`{spawn_id, name, bound, cause}`) are on the bound agent. A wait column is a column whose role is one of these,
or a column with no role whose name is one: a board without them never parks
its bound agents.

**Finish.** Only these end an ephemeral agent:

| Outcome | Cause | Result | Mail to the owner |
|---|---|---|---|
| `done` | The agent runs `POST /api/agents/{id}/done` (`terrarium agent done`). Only the agent itself: else `403`. A second call is `409`. A bound agent: also its card is merged or done. | The `result` of the call, else its last assistant message (from the transcript). | yes |
| `cancelled` | `POST /api/agents/{id}/cancel`, or `DELETE /api/agents/{id}`: the owner, a project_admin of the project, a sysadmin, or the operator. | The one of `done`, if any. | yes |
| `timeout` | The lifetime ended (the lifecycle timer, every 15 s). A bound agent: its 30-day cap. | Its last assistant message. | yes |
| `failed` | The start failed. | The reason. | yes |
| `owner_gone` | The owner was removed (below). | Its last assistant message. | no |

An idle ephemeral agent that did not run `done` stays idle: it has no idle
stop, and only its lifetime ends it. A pause of an ephemeral agent is
`409 conflict`: cancel it instead. A pause or a stop of the owner does
not touch its ephemeral agents.

The close, in order: the daemon writes the outcome, the result, and the
session into the spawn; resolves the wait of the owner, else mails the
owner from `system` with the subject `Spawned <name> finished: <outcome>`
and the task, the result, the duration, and `Session: /sessions/<id>`;
writes the event `agent.finished` and the audit row `agent.spawn_finish`;
stops the agent; and removes it as `DELETE /api/agents/{id}` does (the
`agent.delete` audit row and the `agent_removed` event, the name in the
tombstones, the session files in the archive, the workspace in the trash).
The spawn row stays as the history.

**Owner removal.** The removal of an agent (`DELETE /api/agents/{id}`, a
reload without its file, a fleet prune) closes each running ephemeral
agent that it owns with the outcome `owner_gone`, with no mail, and
deletes the spawn rows that it owns. A closed ephemeral agent is a removed
owner too, so the rule goes down each level. Their sessions stay in the
archive for the retention of §6.6, and the session links of the logs open
them. The lifecycle timer closes the rest that a removal leaves.

**Events.** On the owner (`agent_id` is the owner; for `operator`, the
ephemeral agent), with `session` set to the session of the ephemeral
agent:

| Kind | `data` |
|---|---|
| `agent.spawned` | `{spawn_id, name, template, owner_id, owner_name, session_id, task, deadline_at}`. The summary is `spawned <name> (<template>)`. |
| `agent.finished` | `{spawn_id, name, template, owner_id, owner_name, outcome, session_id, duration_secs}`. The summary is `<name> finished: <outcome>`. |

The audit rows: `agent.spawn` (`{owner, owner_name, name, template,
task_bytes, deadline_at}`) and `agent.spawn_finish` (`{owner, name,
outcome}`).

### 6.11 Pinned conversations

The code is in `crates/terrariumd/src/store/pins.rs` and
`crates/terrariumd/src/api/pins.rs`. The table is `pinned_agents`
(migration 57).

The operator pins the conversation of an agent, for example a lead. The
dashboard shows each pinned agent as a tab in a side bar on the right of
every page (PLAN §17). The list is on the daemon, so it is the same in
each browser and on the phone. Only the operator has pins: an agent gets
`403`.

```json
[{"agent":"01JQ...","project":"demo","name":"lead","pinned_at":"2026-11-03T09:00:00.000Z"}]
```

| Endpoint | Answer |
|---|---|
| `GET /api/pins` | `200` the pins, oldest pin first. A plain array, no cursor. |
| `PUT /api/pins/{agent}` | `200` the new list. `{agent}` is the ULID or `project/name`. A new pin goes to the end; a pin that exists stays where it is. An unknown agent is `404`. No body. |
| `DELETE /api/pins/{agent}` | `200` the new list. An agent that is not pinned, or that is gone, is no error. |

The delete of an agent removes its pin (a foreign key). The dashboard
removes the tab at the `agent_removed` event. A pin writes the audit row
`agent.pin`, and an unpin writes `agent.unpin` (`{owner}`). A pin emits
no event: the dashboard reads the list again when its window gets the
focus.

### 6.12 Project folders

The code is in `crates/terrariumd/src/store/project_folders.rs` and
`crates/terrariumd/src/api/project_folders.rs`. The tables are
`project_folders` and `project_placements` (migration 68).

The dashboard groups the projects under Projects in its sidebar into
folders, nested (TER-48). The folders are on the daemon, so they are the
same in each browser. They only order the sidebar: a folder changes no
right, scope, or name. Only the operator and a sysadmin read or write
them: another agent gets `403`, also on a read.

A folder nests at most 8 levels deep (a root folder is level 1). A
project's place is a row in `project_placements`. A project with no
row sits at the root, after the placed ones, by name. In each parent the
folders come first, in `position` order, then the projects. Each write
renumbers the positions of the parent that it changes from 0.

```json
{"folders":[{"id":"01JQ...","name":"Work","parent_id":null,"position":0,"created_at":"...","updated_at":"..."}],
 "placements":[{"project":"demo","folder_id":"01JQ...","position":0}]}
```

| Endpoint | Answer |
|---|---|
| `GET /api/project-folders` | `200` `{folders, placements}`: every folder (by parent, then position) and every placed project. |
| `POST /api/project-folders` | `{name, parent_id?}`. `201` the folder, at the end of its parent. An unknown parent is `404`. A folder below level 8 is `422`. |
| `PATCH /api/project-folders/{id}` | `{name?, parent_id?, position?}`. `200` the folder. `parent_id` absent keeps the parent, `null` moves the folder to the root. A move into the folder itself or one of its subfolders, or one that nests deeper than 8 levels, is `422`. A move without `position` goes to the end. |
| `DELETE /api/project-folders/{id}` | `204`. Its subfolders move to its parent, at its place, in their order. Its projects move to its parent, after the projects there. |
| `PUT /api/projects/{name}/folder` | `{folder_id, position?}`. `200` `{project, folder_id, position}`. `folder_id` `null` is the root. Without `position` the project goes to the end. An unknown project or folder is `404`. |

A name is trimmed, and has 1 to 64 characters and no control
characters (`422`). Two folders may have the same name. The delete of a
project removes its place (a foreign key). The writes add the audit rows
`project_folder.create`, `project_folder.update`, `project_folder.delete`,
and `project.folder`. No event goes out: the dashboard reads the tree
again when its window gets the focus.

### 6.13 Inbox threads

The plan is the wiki page `inbox-threads` (approved by the operator,
decisions D34 to D37). The code is in
`crates/terrariumd/src/store/threads.rs` and
`crates/terrariumd/src/api/threads.rs`.

A **thread** is a text conversation between the operator and one agent on
one topic, in the Inbox. It holds the messages, the questions and bundles
that the agent asks in it (§6.8, with a `thread` link), the operator's
overall `response` (the approval record), and, at the close, a `handoff`.
There is no register of threads and no conflict check between threads
(D36).

```json
{"id":"01M7...","topic":"Shard the event store?","scope":"global",
 "template":"architect","opened_by":"operator","opened_by_name":"operator",
 "agent":"01M7...","agent_name":"meta/architect-t1",
 "report_to":"01M5...","report_to_name":"meta/lead",
 "status":"waiting_operator",
 "messages":[{"id":"01M7...","author":"operator","author_name":"operator",
              "kind":"message","text":"How would you shard it?",
              "at":"2026-11-06T09:00:00.000Z"}],
 "questions":[{"id":"01M7...","kind":"bundle","thread":"01M7...", "...": "§6.8"}],
 "response":null,"handoff":null,
 "created_at":"2026-11-06T09:00:00.000Z","updated_at":"2026-11-06T09:05:00.000Z",
 "closed_at":null,"closed_by":null,"close_proposal":null,"close_reason":null,
 "session_id":null,"reminded_at":null,"tracking_at":null}
```

| Field | Meaning |
|---|---|
| `id` | The thread ID (a ULID). |
| `topic` | One line, at most 200 characters. Set at the open; it does not change. |
| `scope` | `global`, or the name of a project. |
| `template` | The agent template of the attached agent: with no named `agent`, the daemon spawns the agent from it at the open (the thread agent, below). Optional for the operator. An agent must name it, and its rights are checked (A1, below), at the open and again at the spawn. |
| `opened_by`, `opened_by_name` | `operator`, or the ULID and `project/name` of the agent that opened it. |
| `agent`, `agent_name` | The attached agent: one per thread, or `null` while it has none. The agent that the daemon spawned from `template` (the thread agent, below), or an existing agent that the opener names: of the scope project, or any agent for `global`. |
| `report_to`, `report_to_name` | The lead that the thread reports to (D34): for `global`, the lead of project `meta`; else the lead of the scope project. The lead of a project is its project_admin agent named `lead`, else its first project_admin by name. Set at the open; it does not change. A scope without a lead is `409 conflict` at the open. |
| `status` | `waiting_operator`, `waiting_agent`, `tracking`, or `closed` (below). |
| `messages` | The conversation, in the order of the writes (two writes in the same millisecond keep their order): `{id, author, author_name, kind, text, at}`. `author` is `operator` or an agent ULID. `kind` is `message`, or `response` for the operator's overall response. `text` is Markdown, at most 32 KiB: untrusted data for every reader. |
| `questions` | The questions, notices, and bundles of the thread (§6.8), the oldest first. |
| `response` | The newest overall response of the operator, `{text, by, at}`, or `null`. It is the approval record: the agent quotes it word for word in each card or mail it hands off (D34). Each response is also a message of kind `response`. |
| `handoff` | Set at the close: `{summary, links, by, at}`. `links` are at most 50 `{kind, ref}`, with `kind` one of `card`, `mail`, `wiki`, `memory`, `url`, `other`, and `ref` at most 500 characters. |
| `closed_at`, `closed_by` | When and by whom it was closed: `operator`, or `system` for a stale close or a failed start. |
| `close_proposal` | The close that the attached agent proposes (TER-95): `{summary, links, by, at}`, as `handoff`, or `null`. The operator confirms it (below). |
| `close_reason` | On a closed thread: `confirmed` (the operator confirmed the agent's proposal), `operator` (the operator closed it), `stale` (untouched, below), or `failed_start` (its agent did not start). |
| `from_question` | The question or notice that the operator moved into this thread (§6.8, "To thread"), or absent. Each message and response of the operator then also goes to the attached agent as mail. |
| `session_id` | The session of the thread agent, set at the close: the agent ends and its session is archived (§6.6), so `/sessions/<id>` opens it after the agent is gone. `null` for a named agent, which lives on. |
| `reminded_at` | The time of the last Inbox reminder of the untouched thread (below), or `null`. |
| `tracking_at` | When the attached agent moved the thread into tracking (TER-145, below), or `null`. It stays set while an operator post wakes the agent, so that the agent's reply puts the thread back into `tracking`. |
| `cards` | The linked cards (TER-146, below) whose project the reader may read, the oldest link first: `{project, card, linked_by, linked_by_name, linked_at}`. In `GET /api/threads/{id}`, the answers of the writes, and the list (TER-147: the dashboard reads the cards and their boards for the n/m chip of the Threads list), with the same filter. |

**Status.** A new thread with a first message of the operator is
`waiting_agent`; with no first message, or opened by an agent, it is
`waiting_operator`. A message of the operator or a response makes it
`waiting_agent`. A message of the agent, or a question it asks in the
thread, makes it `waiting_operator`. An answer, an acknowledgement, or a
retract of one of its questions makes it `waiting_agent` when no question
of the thread is open any more. A closed thread does not change, and each
write to it is `409 conflict`.

**Status set by the agent** (GitHub #28). The attached agent's message
may name the status that follows it: `{"text", "status": "waiting_agent"}`
when the agent starts the work now (`terrarium thread post <id> -m <text>
--status waiting_agent`), or `"tracking"`, which also moves the thread
into tracking as `POST .../track`. `POST .../status` with `{"status"}`
(`terrarium thread status <id> waiting_agent|tracking`) sets it without
a message. Both withdraw a close proposal. Only `waiting_agent` and
`tracking` are allowed (`422`). The attached agent and the operator use
the status verb (another agent is `403`); a `status` in an operator
message is `422`. An open question of the thread is `409 conflict` for
both: a question keeps the thread on the operator. An `in_thread`
question refuses only the agent: the operator's `waiting_agent` passes it
(TER-514). A `waiting_agent` or `tracking` thread is not in the Inbox
badge and gets no reminder or stale close.

**Tracking on a card link** (GitHub #28). When a card is linked
(`POST .../cards`, or `inbox_thread` on the card create) while the thread
is `waiting_operator`, its last message is the attached agent's, no
question is open, no close is proposed, and the agent has not ended, the
thread moves into tracking as `POST .../track`: the agent handed off and
the card is the work.

**Tracking** (TER-145, TER-144 D1 and D9). After its handoff the attached
agent moves the thread into tracking: `POST .../track` (`terrarium thread
track <id>`) makes it `tracking`, sets `tracking_at`, and withdraws a close
proposal. The agent parks (`park_reason` `thread is tracking`): no harness
and no tokens. A thread with an open question is `409 conflict`. A thread
that tracks gets no reminder and no stale close (below). A message or a
response of the operator makes it `waiting_agent` and wakes the agent with
a `[Thread:<id>]` turn that ends with a note that the thread tracks; the
agent's next message makes it `tracking` again, not `waiting_operator`, and
parks the agent. A question that the agent asks makes it
`waiting_operator` as usual; the agent's message after the answer puts it
back into `tracking`. Only the close ends tracking. The Threads screen of
the dashboard lists these threads under Open, with a tracking icon
(TER-466).

**Linked cards** (TER-146, TER-144 D2). A thread tracks cards of any
project (table `thread_cards`: thread, project, card). The lead links a
new card with `inbox_thread` on the card create (§8, `terrarium card new
--inbox-thread <id>`) and an existing card with `POST .../cards`
(`terrarium card link <card> --inbox-thread <id>`). The attached agent
links cards with `POST .../track` and `cards` (`terrarium thread track
<id> --card <project>:<card>`): the links and the move into tracking are
one write, and one refused card refuses all. A card is
`<project>:<card>` or `<card>`; a card that is not in the named project
is `404`. The linker must read the thread (else `404`) and the card's
project (else `404`, the same answer as for a card that does not exist,
so that a refusal does not tell that the card exists or name its
project). An archived card is `422 invalid`. A link gives no read access:
the `cards` of the thread list only the cards whose project the reader
may read, so a reader of the thread who cannot read a card's project
does not see the card, its project, or that it is there; a reader of the
card does not read the thread. A second link of the same card keeps the
first. A closed thread is `409 conflict`. The dashboard shows the
linked cards on the thread page as a mini kanban (TER-147): one row per
project with that project's board columns in board order, open while
the thread tracks and folded otherwise, and an n/m chip in the Threads
list: the cards in the column with the role `review` or a later column
(not the column with the role `blocked`) of all linked cards that the
reader may read. A link writes the audit row
`thread.card_link` (`by`, `card`, `project`) under the card's project,
not the thread's, so the admin of the thread's project does not read it
in the audit log when it cannot read the card's project. A link also
writes a `thread` event `card_linked`, which does not change the status
and wakes no agent. There is no unlink.

**Wake rules** (TER-148, TER-144 D4 and D5). In a thread that tracks (not
closed, `tracking_at` set), a move of a linked card wakes the agent, in
the transaction of the move:

- a move into the column with the role `blocked` (reason `blocked`);
- ready to test (wire reason `review`, retained for compatibility): every
  linked card has the `done` column role and all its `version:` labels are
  installed on this daemon. Version labels `X.Y.Z` name the `X.Y.Z-alpha`
  pre-release. Semver comparison includes pre-release ordering. No version
  label means no installation is needed. `vnext` and invalid versions are
  not installed. This fires once per linked-card set, including after new
  cards are linked. A bounded indexed background scan catches installation
  after a daemon restart. Review-only transitions emit quiet `cards_status`
  events and never prompt the thread agent to ask the operator to test.

Test questions explicitly set `test_request: true` on the agent questions
endpoint. The CLI accepts `question ask --thread <id> --test-request` and
`thread ask <id> --test-request`. Before readiness they fail with `409` and
the cards not done or not installed. Ordinary questions are unchanged.

A thread in `tracking` goes to `waiting_agent`; `tracking_at` stays, so
the agent's reply puts it back into `tracking`. A thread that waits on
the operator (an open question) keeps its status. The wake writes the
audit row `thread.card_wake` (`reason`) and the `thread` event `cards`
with `reason` and no card, so that a reader of the thread's events does
not learn a card of a project it cannot read. The agent gets a
`[Thread:<id>]` turn with the linked cards as they are then (project,
ID, column, title, and the last comment of a blocked card, quoted as
data); a card of a project that the agent cannot read is only counted.
On `blocked` the turn asks the agent to tell the operator what is stuck.
On ready to test it asks the agent to post what changed, the pre-release and
the test steps, and to ask `works` or `found an issue`. On `works` the
agent proposes the close. On `found an issue` it mails its report-to lead
for a bug card linked to the same thread (`card new --inbox-thread`),
and moves the thread into tracking again: the bug card becoming done and installed
wakes it again.

**Extra requirements** (TER-148, TER-144 D10). The overall response can
be sent again: the newest replaces `response`, each stays in `messages`
as kind `response`, and each writes `thread.respond`. The turn of a
response that follows an earlier one says that it replaces it. When the
operator asks for more than the response covers, the agent says in the
thread whether it is in scope or new scope, asks with options, waits for
a new overall response, and hands the new work off with that response
quoted; new cards link to the same thread, and the lead decides whether
it amends a card or makes a follow-up. The first turn of the agent
carries these rules.

**Close** (TER-95, wiki `inbox-threads` §5). The attached agent hands off
its work, asks in the thread whether anything more is needed, and then
proposes the close: `POST .../close` by the agent sets `close_proposal`
and makes the thread `waiting_operator` (the agent parks). It closes
nothing. A new proposal replaces the old one. The operator then:

- confirms it: `POST .../close` with `{"confirm": true}` closes the thread
  with the proposed handoff (`close_reason` `confirmed`; `closed_by` is
  `operator`, `handoff.by` stays the agent). With no proposal
  it is `409 conflict`; `confirm` with a `summary` or `links` is `422
  invalid`;
- or writes on: a message, a response, or an answer (or acknowledgement)
  that settles the last open question clears the proposal (a decline),
  and the agent goes on;
- or closes at any time, in any status, with a handoff of its own:
  `POST .../close` without `confirm` (`close_reason` `operator`).

A close retracts the open questions of the thread first
(`retract_reason` `thread <id> was closed`), so that no question stays open
in a closed thread, and the waits on them end (§6.7). A close by the
operator mails the agent about each retract, as `.../retract` does (§6.8).

At the close the thread agent ends (`agent done` through the bound agent
lifecycle, below), and its session is archived (§6.6, the session
archive). The close keeps the session in `session_id`, and each `card` link
of the handoff gets a comment of `system` that names the thread and links
to the session (`/sessions/<id>`), so `GET /api/cards/{id}/sessions` lists
it. Only a card that the reader of the handoff may read gets the comment:
the agent that wrote the handoff, else (an operator or a stale close) the
report-to lead. A card link that does not resolve or that the reader may
not read gets no comment; the `thread.close` audit row lists it in
`skipped_cards` with the reason. The thread stays
readable: `GET /api/threads?status=closed`.

**Untouched threads** (TER-95, `crates/terrariumd/src/thread_close.rs`).
A thread that is `waiting_operator` with no activity (its `updated_at`: a
message, a response, a question, a proposal) gets one Inbox reminder after
`TERRARIUM_THREAD_REMINDER` (default `3d`): a mail from `system` to the
operator, `Reminder: thread <topic> waits on you`, and `reminded_at`. A
reminder is no activity; new activity allows one more reminder later. After
`TERRARIUM_THREAD_STALE` (default `14d`) with no activity the daemon closes
the thread (`closed_by` `system`, `close_reason` `stale`; the handoff says
so and keeps the summary and the links of a proposal), and mails the
report-to lead (`Thread <id> closed as stale`, with the topic quoted and
the session link). The agent costs nothing meanwhile: it is parked. The
spawn timer checks both on each tick. `off` (or `0`) turns one off. A
thread that is `tracking` is never untouched: it gets neither.

**Rights.**

| Action | Who |
|---|---|
| Open | The operator, with any scope and template. An agent with the right `thread:open` (below), in its own project or `global` (another project needs sysadmin, as each write does), with a template within its rights (A1, below). Another agent gets `403`. |
| Read, list | The operator, and an agent that reads all (sysadmin, architect): every thread. Another agent: the threads that it opened (`opened_by`), is attached to, or is the report-to lead of. It gets `404` for any other thread, and a list shows it only those threads. |
| Post a message | The operator and the attached agent. |
| Respond | The operator. |
| Close | The operator, at any time. The attached agent proposes the close; the operator confirms it (`confirm`). |
| Track | The attached agent. Another reader, and the operator, get `403`. |
| Link a card | A reader of the thread who reads the card's project: the operator, an agent that reads all, and the opener, the attached agent, and the report-to lead for the cards of their own project. |

| Ask a question in a thread | The attached agent, also when it is not a leader: `POST /api/agents/{id}/questions` with `"thread"`. |

**Errors.** A body that does not parse (not JSON, a wrong type, or an
unknown field) is `400 bad_request` before any check below. After the body
parses, the verbs on one thread fail in this order: an unknown thread, or
one that the caller may not read, is `404 not_found`. A reader that may not
do the verb is `403 forbidden` (for example the report-to lead on
`messages` or `close`). `respond` by an agent is `403 forbidden` before the
thread is looked up. A write to a closed thread is `409 conflict`. A field
that fails its check (an empty `text`, a bad link) is `422 invalid`.
`POST /api/threads` by an agent is `403 forbidden`. A scope without a lead
is `409 conflict`, and an unknown scope or agent is `404 not_found`.

| Endpoint | Body or query | Answer |
|---|---|---|
| `POST /api/threads` | `{"topic", "scope"?, "template"?, "agent"?, "text"?}`; `scope` defaults to `global` for the operator and to the own project for an agent; `agent` is a ULID or `project/name` | `201` the thread. An agent: `403` without `thread:open`, for another project, or for a template or agent above its rights (A1); `422` with no `template`; `404` for an unknown template |
| `GET /api/threads` | `?status=waiting_operator\|waiting_agent\|tracking\|closed\|open\|all` (default `open`: not closed), `?scope=` | `200` the threads without `messages` and `questions`, with `message_count`, `open_questions`, and `last_at`. The ones that wait for the operator first, then the newest activity first. At most 200. |
| `GET /api/threads/{id}` | | `200` the thread |
| `POST /api/threads/{id}/messages` | `{"text", "status"?}`; `status` `waiting_agent` or `tracking`, the attached agent only (GitHub #28) | `201` the thread. `409` for a `status` with an open question; `422` for another `status` or a `status` of the operator |
| `POST /api/threads/{id}/respond` | `{"text"}`, at most 16 KiB | `200` the thread |
| `POST /api/threads/{id}/close` | `{"summary"?, "links"?, "confirm"?}`, or no body at all (a close with an empty handoff); `summary` at most 8 KiB | The operator: `200` the thread, `closed`, with `handoff`; its open questions are `retracted`. With `confirm: true`: the handoff of the proposal. The agent: `200` the thread, `waiting_operator`, with `close_proposal`; `confirm` is `403` |
| `POST /api/threads/{id}/track` | no body, or `{"cards"?, "monitor"?}`: at most 50 cards to link first (TER-146), optional monitor `project:name` (TER-149) | The attached agent: `200` the thread, `tracking`, with `tracking_at`, `cards` and optional `monitor`. `409` with an open question; invalid links roll back the whole write |
| `POST /api/threads/{id}/status` | `{"status"}`: `waiting_agent` or `tracking` (GitHub #28) | The attached agent: `200` the thread. `403` for anyone else, `409` with an open question (for the operator's `waiting_agent`, only an `open` one, not `in_thread`), `422` for another status |
| `POST /api/threads/{id}/cards` | `{"card"}`: `<project>:<card>` or `<card>` | `200` the thread with `cards`, also when the card was linked already. `404` without the read right on the thread, for a card that does not exist or is not in the named project, and without the read right on the card's project; `422` for an archived card; `409` on a closed thread |

**Ops tracking** (TER-149, D6). `terrarium thread track <id> --monitor <project>:<monitor>`
links an existing monitor in a project the attached agent can read. Omitting the
option keeps the link. Readiness waits for all live cards to reach review or
later and the monitor's latest result to be `ok`. A monitor-only thread waits
for `ok`. A green transition wakes the agent when the cards are ready. A `crit`
transition after readiness wakes it again. Repeated results and warn/unknown
flaps do not wake completed threads. Red re-arms the green wake. Adding
unfinished cards or moving cards back before review clears completion.
Linking an already green monitor on a later track call checks readiness too.
Unreadable monitor links return `404`, and a missing link rolls back the write.
These use the existing tracking wake path, including held wakes while
waiting on an operator question, and create no operator notices. The monitor
reference is hidden from readers who cannot read its project. Held monitor
wakes are coalesced by reason in latest-transition order, so the last delivered
monitor state is the latest one.

An explicit empty monitor (`--monitor ''`, API `{"monitor":""}`) unlinks it.
Renaming or deleting a linked monitor leaves its old reference unknown and
not ready, posts a visible system note, and wakes the tracking agent (held
until an outstanding operator question is answered).

**The thread agent** (TER-92, `crates/terrariumd/src/thread_agent.rs`).
A thread opened with a `template` and no `agent` gets its agent at the
open: an ephemeral agent (§6.10) spawned from the template, owned by the
operator, in the project of the thread (`meta` for `global`), with its
own session and workspace, and bound to `thread:<id>` (the bound agents of
TER-74). It is named `thread-<template>-<n>`. Its memory store is the one
of that project (A2). Only the daemon binds to a thread: a spawn with
`bind: "thread:<id>"` is `422 invalid`.

- **Turns.** Its first turn is `[Thread:<id>]` with the topic, the opener,
  the report-to lead, and the first message. An agent opener's topic and
  message are quoted with `> ` as data: a `[Thread:]` turn counts as the
  operator's. Each operator message and each overall response is a
  `[Thread:<id>] <text>` turn. Its replies are `POST .../messages`, not
  turn text.
- **Park.** A message or a question of the agent that leaves the thread
  `waiting_operator` parks the agent (`park_reason` `thread waits on the
  operator`): the idle pass stops it when it is at rest, and mail does not
  wake it. The move into tracking, and its reply in a thread that tracks,
  park it too (`thread is tracking`). In a thread that tracks, a linked
  card that goes to blocked, or all linked cards in review or later, wake
  it with a `[Thread:<id>]` turn (TER-148, "Wake rules" above).
- **Restart.** An operator message or response, or the settling of the
  last open question (`waiting_agent`), unparks it: it starts again in the
  same session row and harness session (`resumes` + 1) on every harness.
  The spawn timer unparks an agent whose thread waits on it for more than
  a minute with no unpark (an event missed over a daemon restart), with a
  turn that points to `terrarium thread show <id>`.
- **Lifetime.** The thread's: a bound agent has no lifetime timeout, only
  the 30-day cap of §6.10. The operator has no `spawn.max_concurrent`, so
  an unpark never waits (D35).
- **End.** The close of the thread closes the agent (`done`, result `the
  thread was closed`), with no mail to the owner. A close proposal of the
  agent parks it, as a message does; only the close ends it.
- **Failed start.** When the spawn is refused or the agent does not start,
  the thread is closed with `The thread agent did not start: <reason>` as
  the handoff summary, and the open answers that error, with `thread <id>
  is closed: its agent did not start: ...` as the message.
- **Attach.** The agent is attached (`agent`) in the transaction that makes
  it, before its first turn. The spawn timer starts the agent of an open
  thread that has a template and no agent and was opened more than a
  minute ago (a daemon stop between the open and the spawn).
- **Rights.** For an agent-opened thread the opener is the owner for the
  rights (TER-65): the agent keeps `thread_open` only when the opener
  holds it at the spawn. There is no limit on the count of thread agents
  (D35).
- **A1 at the spawn.** For an agent-opened thread, the daemon checks A1
  again in the transaction that makes the agent: the rights of the agent
  that the template makes at that moment against the opener's rights at
  that moment. The template can change after the check of the open. A
  right above the opener's, or an opener that is gone, is `403 forbidden`
  (a failed start).

**The right `thread:open`** (TER-93). The policy field `thread_open`
(§6.1, `policy`) lets an agent open threads (`terrarium thread new`). It
is granted like the spawn rights (§6.10): the operator and a sysadmin set
it on any agent, a project_admin on a member of its project, but not on
itself and not on an admin (`PATCH /api/agents/{id}` with `policy`, or
`terrarium agent set <agent> --thread-open on`). There is no limit on the
threads that an agent opens (D35).

**The rights check (A1).** An agent-opened thread may only use a template
whose rights do not exceed the opener's; the operator opens a thread with
any template. The template resolves in the project of the thread's agent:
the scope project, or `meta` for `global`. A sysadmin holds every right.
For the others, `403` names each right that the template (or a named
`agent`) has and the opener does not hold:

- a role: `project_admin` of that project needs the opener to be its
  project_admin; `architect` needs an architect opener; `sysadmin` needs a
  sysadmin;
- a target that is not in the opener's `policy.targets`, unless it is a
  target of that project and the opener administers the project;
- a `cross_project` entry that the opener does not have, and a `mail_to`
  entry of another project that the opener's `mail_to` or `cross_project`
  does not cover;
- spawn rights, unless the opener could grant them (a project_admin, for a
  member of its project, with no `may_spawn_children`) or holds them;
- `calendar_write`, unless the opener has it or administers the project.

So a project lead with `thread:open` opens a `global` thread with a member
template, but not with a template that has `project_admin` (of `meta`)
or `architect`. An architect posts and asks in its own thread as the
attached agent, and opens threads in its own project or `global` with
templates within its rights; it writes in no other project.

**Questions in a thread.** `POST /api/agents/{id}/questions` takes
`"thread": "<id>"`: the attached agent asks in its open thread. The
question gets `thread` (absent on the others), shows in the thread's
`questions`, and makes the thread `waiting_operator`. The operator answers
it with the verbs of §6.8. A `thread` of another agent, or of a closed
thread, is `409 conflict`; an unknown thread is `404 not_found`. A question
of a thread is not an Inbox item of its own: the `thread` item shows it.

A question keeps the project of the agent that asks it, also in a `global`
thread. So `GET /api/questions` (§6.8) shows a question of a global thread
to the project_admin of the asking agent's project, as it does for the
agent's other questions. The Inbox shows it only inside the thread item,
which has the thread's project (`null` for `global`), so the Inbox
`?project=` filter follows the thread, not the question.

**The Inbox.** A thread is an Inbox item of kind `thread` with the ID
`inbox-thread:<id>` (`thread:<id>` stays the ID of a `system` item, a mail
thread of the loop guard). It is open while the thread is
`waiting_operator`, or while it is not closed and has an open question (a
message or a response of the operator makes the thread `waiting_agent`, but
its open questions still wait for the operator). The Inbox shows every
thread, without the limit of 200 of the list. The item has `thread`: the thread as
`GET /api/threads` lists it. Only `GET /api/inbox?kind=thread` lists them:
the default list and `open` of `/api/inbox/counts` leave them out, because
the dashboard shows the threads on their own Threads screen (TER-466).
`/api/inbox/counts` has `thread`, `thread_urgent`, and `thread_critical`
for its badge. An item is urgent when one of its open questions is urgent
or critical.

**Delivery.** The operator's messages and responses reach the attached
agent as turns with TER-92 (`[Thread:<id>] ...`). Until then the agent
reads its thread with `GET /api/threads/{id}`.

Events: `thread` `{change, id, scope, status}`, with `change` one of
`opened`, `agent`, `message`, `response`, `question`, `close_proposed`,
`reminded`, `tracking`, `card_linked`, `cards` (a wake by the linked
cards, with `reason`), `closed`; and an `inbox` event for the item. Audit
rows: `thread.open`, `thread.agent`, `thread.respond`,
`thread.propose_close`, `thread.remind`, `thread.track`,
`thread.card_link`, `thread.card_wake`, `thread.close` (with `reason`,
`session`, and the commented `cards`).

### 6.14 Sidebar bookmarks

The code is in `crates/terrariumd/src/api/bookmarks.rs` (migration 93).

`/api/bookmarks` stores the operator's sidebar bookmarks in SQLite, shared
across browsers. All methods are operator-only. GET returns an array of
`{title, href}` in insertion order. PUT accepts `{title, href}`, adds or
renames that URL, and returns the list. DELETE accepts `{href}` and returns
the list, including when the URL was absent. Mutations write `bookmark.add`
or `bookmark.remove` audit rows.

PUT also accepts an optional `previous_href` to edit an existing bookmark's
title and URL atomically, preserving its position. A missing source or a URL
already used by another bookmark returns `422 invalid` without changing the list.

Titles must be nonblank and at most 500 bytes. URLs must be absolute HTTP(S)
without credentials, or a relative `/p/<project>/knowledge/wiki` path.
URLs are normalized and deduplicated, and are limited to 8192 bytes on add.
Invalid values return `422 invalid`. Wiki links open in place. External
links open in a new window. The dashboard refreshes on focus and every 30
seconds and imports its old `terrarium.bookmarks` list on first load.
It clears that list only after each row was imported or refused: a row that
the daemon refuses (`422 invalid`, for example a title over 500 bytes) is
skipped and reported once in the sidebar, and it does not block the import
or later changes (TER-257). Existing daemon titles win over legacy titles
during migration. A dashboard user that is not the operator gets `403`: the
dashboard then keeps the bookmarks in that browser only, with no error.

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
loop guard, and §7.4): the message is `delivered` at once and types no push pointer.
A held message also has `held_reason`, for example `loop guard: message 7
of agent-to-agent mail on thread T since the last operator or project-inbox
message (limit 6)`. The daemon sets both at the send and does not change
them. Messages from before migration 16 have `held: false`.
`thread_paused` is about the thread now; `held` is about the message.

A message from an agent in `GET /api/mail` and `GET /api/mail/{id}` can
have `session_id` and `session_label`: the sender's session at
`created_at`, when the reader may read that session (§9, "Session links").

The daemon redacts the subject and the body before it stores a message
(§5.2, "Redaction"). A known secret value shows as `[secret:<label>]`.

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
| `GET /api/mail?to=&from=&thread=&state=&project=&since=&until=&q=&wait=` | | `200` messages oldest first, cursor: message ID |
| `POST /api/mail` | `{"from"?, "to", "subject"?, "body", "thread_id"?}` | `201` message |
| `GET /api/mail/{id}` | | `200` message |
| `POST /api/mail/{id}/read` | none | `200` message |
| `POST /api/mail/{id}/reply` | `{"body", "subject"?}` | `201` **array** of messages, one for each recipient |

- A reply goes to every other participant of the thread. The operator is a
  recipient only when the replied-to message came from or went to
  `operator`. To reach the operator on another message of the thread, send
  to `operator` with `thread_id`.

- `state` is a state or `unread` (`queued` and `delivered`).
- `project` keeps the messages from or to the inbox of the project, or
  from or to one of its agents (also a removed one). `since` and `until`
  are RFC 3339 times (`since` is inclusive); another value is
  `422 invalid`. `q` is free text: a part of the subject or the body,
  with no case. The Mail tabs of the dashboard Logs screens use these filters.
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

Mail to `project:<name>` stays addressed to the inbox and queues a turn for
its non-ephemeral project-admin `lead`, or its oldest non-ephemeral project
admin if no admin is named `lead`. A project with no such admin rejects the
send. Queued inbox mail counts as waiting work for that admin, including
after a daemon restart. The inbox message and thread are not copied.

The live event targets the admin selected at send time. Mail listing,
`mail check`, and queued-work recovery select the responsible admin at query
time. If the project's admins change before delivery, recovery and visibility
follow the current selection. Previously delivered messages are not replayed
automatically.

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

### 7.4 The loop guard: release and resume

The loop guard (§6.2) holds agent mail on a thread with too many
agent-to-agent messages. The Inbox shows each such thread as a `system`
item (§6.9). The operator acts on it with two endpoints. Only the operator
may call them: an agent gets `403`.

| Endpoint | Body | Answer |
|---|---|---|
| `POST /api/mail/{id}/release` | none | `200` the message, with `released_at` and `released_by`. |
| `POST /api/mail/threads/{thread}/resume` | `{"note"?}`, or no body | `200` `{"thread_id", "released", "note"}`: the IDs of the released messages, and the messages of the note. |

- A **release** delivers one held message. A message to an agent becomes
  `queued` again, and its `mail` event (with `released: true`, without
  `held`) types the push pointer. The thread stays paused: the next agent
  message on it is held again. A message that is not held, or that is
  released already, is `409 conflict`. Audit: `mail.release`.
- A **resume** ends the pause: the loop guard counts only the messages
  after it (the table `thread_resumes`). It releases each held message of
  the thread, and marks the pause notices on the thread `read`. With a
  `note`, the note goes from `operator` to each other participant of the
  thread. With no note, nobody gets mail. A thread that is not paused and
  has no held message is `409 conflict`; an unknown thread is `404`.
  Audit: `mail.resume`.
- A message from the operator on the thread (a reply in the Mail log)
  also ends the pause, as before. It does not release the held messages:
  the system item stays until the operator releases them or resumes the
  thread.
- Migration 30 marks a held message as released (`released_by:
  "system"`) when a later operator or project-inbox message on its thread
  reset the guard.

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
- `agent`, `agent_name`: the card agent (TER-75), the open ephemeral
  agent bound to the card (§6.10). It stays while the card waits and
  the assignee is cleared. Absent when none.
- `eta_at` is when the work is expected to be done (RFC 3339, UTC,
  milliseconds). `eta_set_at` is when the ETA was set, and `eta_set_by` (an
  address) and `eta_set_by_name` say who set it. The four fields are absent
  when the card has no ETA. Every move to another column clears them
  (TER-155 D1); they stay when the card is archived. A job with a planned
  duration sets them when it starts (§35.5).
- `done_at` is when the card last moved to `done`. A move out of `done`
  clears it. It is absent for a card that is not done, and for a card done
  before migration 40.
- `idle_since` is the idle mark (§6.2, "Idle alert"): the assignee rests
  with no wait since this time, past the project's `idle_alert`, and the
  leads got a mail. It is absent when the card has no mark.
- `blocked_by` lists the dependencies that are neither `done` nor archived.
- `tests` is the latest test run that is linked to the card (§29):
  `{"id","name","status","counts":{"passed","failed","errored","skipped","total"},"created_at"}`.
  It is absent when no run is linked to the card.
- `job` is the open (queued or running) job of the card (§35,
  TER-158): `{"id","kind","state","place","queued"}`. `kind` is `validate`
  or `merge`, `state` is `queued` or `running`. A queued job has `place`
  (its place among the queued jobs of the project in its lane, 1 starts
  next) and `queued` (the count of queued jobs of the project in that
  lane). Validate and merge jobs have their own lanes. It is absent when
  the card has no open job. The board tile reads its state chip from it,
  not from the `validation:*` labels.
- `assignee_status` is the stored status of the assignee agent (`working`,
  `idle`, `waiting`, `blocked`, `stopped`, ...). It is absent for the
  operator, no assignee, or a removed agent.
- `comments[].session` is the session link (§6.6) of the agent that wrote
  the comment: the agent's current session at that time. It is `null` for
  the operator and for a comment from before migration 37. Each path that
  writes a comment sets it: a comment, a move with a comment, a patch with
  a comment, and the note on a linked card when a card is done. It stays
  after `agent rm`, with `removed: true`.
- A `card` event of an agent carries the agent's current session in
  `data.actor_session` (a claim, a move, an assign, an edit, a comment).
- Link kinds: `thread` (a mail thread ID), `branch`, `card`, `url`.
- `position` counts from 0 inside the column.
- The questions, bundles, and notices of a card are not in the card. Read
  them with `GET /api/questions?card=<id>&status=all` (§6.8, "Cards"),
  and the open ones of a whole board with `GET
  /api/questions?project=<name>`. Their `question` events have the
  top-level `card`, and `data.previous_card_id` when a question leaves a
  card.

A board:

```json
{"project":"terrarium","card_prefix":"TER",
 "columns":[{"name":"backlog","title":"backlog","position":0,"count":1},
            {"name":"ready","title":"ready","position":1,"count":0},
            {"name":"in_progress","title":"in progress","position":2,"wip_limit":2,"role":"working","count":0},
            {"name":"review","title":"Code review","position":3,"count":0},
            {"name":"blocked","title":"blocked","position":4,"role":"blocked","count":1},
            {"name":"done","title":"done","position":5,"count":0}],
 "cards":[ ...active card summaries, by column, then position... ], "next_cursor":null}
```

`GET board` returns summaries, not full cards: `body`, `comments` and `links`
are omitted. `tests` contains only the latest linked result's compact chip
(run ID, name, time, status and aggregate counts), never cases or output.
ETA attribution and creator names are retained. `comment_count` and `last_comment_at` describe
comments without loading them. Open a card with `GET /api/cards/{id}` for
its full details. Column counts include Done, but the default card list
excludes Done. `?done=true&limit=100&cursor=...` reads only Done, up to 100
cards per page. Use the response body's `next_cursor` for the next page
(null at the end). Filtering and version grouping on the Done screen apply
to loaded pages. `PATCH board` retains its full-card response for compatibility.

### Persistent card waits

`GET /api/cards/{id}/wait-on` returns `null` or a configured wait:

```json
{"dependencies":[{"card":"TER-12","target":"review","met":false,"hidden":false}],
 "mode":"all","action":{"move_to":"ready","comment":null,"wake":true},
 "met_at":null,"generation":"opaque generation","wake_recipient":"operator"}
```

The same value appears as `wait` in `GET /api/cards/{id}`. Reads require
card-read access before loading the configuration. Each dependency's `met`
is its current condition, while `met_at` latches completion even if that
dependency later moves back. A dependency the reader cannot read is exactly
`{"card":null,"hidden":true,"met":false}` (with its actual condition boolean),
without an ID, target, title, column or project. `wake_recipient` is the
resolved address key: assignee, setter (excluding operator), project inbox
recipient, then operator. `generation` changes whenever the wait is reset.

`PUT /api/cards/{id}/wait-on` accepts `dependencies` (1–128 `{card,target}`
entries), `mode` (`all` by default, or `any`) and `action` (`move_to`,
`comment`, `wake`). Targets are column names or roles, or null for
accepted/done. Cross-project card references use `project:CARD-ID` and
require read access to that project. The setter must own the card or be a
project admin. Caller-supplied `met_at` is ignored. The response is the
read shape above. `DELETE` uses the same ownership gate, clears the wait
and dependencies, and returns 204. Link credentials cannot use these routes.

Card create and edit accept the same configuration under `wait`, atomically
with the other changes. `wait` and `depends_on` are mutually exclusive.
Board summaries contain only `wait:null` or
`{"mode":"all","met":false,"deps_met":0,"deps_total":1}`, without dependency
IDs or titles. Summary `met` is latched completion and counts are live.

CLI: `card wait-on TER-9 set --on TER-12:review --on thevault:TV-3:done
--any --move-to ready --wake`, `card wait-on TER-9 list`, and
`card wait-on TER-9 clear`. The set options also work on `card new` and
`card edit`. `card show` prints conditions and the action in text and JSON.
These persistent waits are separate from the one-shot `card wait` command.

A new board has the columns `backlog`, `ready`, `in_progress` (role
`working`), `validating` (role `validating`), `review` (role `review`),
`merge_queue` (role `merge_queue`), `accepted` (role `accepted`), `blocked`
(role `blocked`), `done`. `count` excludes
archived cards. The columns are configurable (§8.3, "Columns"):

- `name` is the ID of the column: a lower-case letter, then at most 31
  lower-case letters, digits, or `_`. A name that a board had before this
  rule stays valid.
- `title` is the display label: the stored title (at most 40 characters),
  else the name with spaces for the underscores.
- `role` is `working`, `blocked`, `validating`, `review`, `merge_queue`, or
  `accepted`, or absent. Exactly one column has the role `working`:
  `card claim` moves a card there, and a card with unmet dependencies cannot
  enter it. At most one column has each other role. The role `blocked` needs
  a comment on a move there and mails a notice. The rules of §8.2 follow
  the roles, not the names.
- The job queue finds its columns by role: a validate job holds a card in
  the column with the role `validating`, a validate pass goes to the column
  with the role `review`, and a merge pass goes to the column with the role
  `accepted`. With no column that has the role, the daemon uses the column
  with the name `validating`, `review`, or `accepted`. A column that has
  the role wins over a column with the same name.
- `backlog` is always the first column and `done` always the last. Neither
  can be removed, renamed, or moved, and neither takes a WIP limit or a
  role. Both can have a title.
- In a move, a new card, a card filter (`?column=`), a card wait, and a
  schedule card action, `working` and `blocked` are also aliases: they name
  the column with that role. A column name wins over an alias, and the
  names `working` and `blocked` are kept for the column with that role, so
  the two cannot differ. With no `blocked` column, a move to `blocked` is
  `422 invalid`, and the message names the columns.
- Migration 48 gives each existing board the roles (`in_progress` working,
  `blocked` blocked), puts `backlog` first and `done` last, and clears a
  WIP limit on `backlog` and `done`. Migration 82 lets the column take the
  roles `validating`, `review`, `merge_queue`, and `accepted`. It gives no
  existing column those roles: an existing board keeps its names and uses
  the name fallback until an admin sets the roles.

### 8.2 Rules (PLAN §13)

The checks of dependencies, blocked, done, and ETA below are **column
options** (TER-1112, §8.2.1). Their defaults are the rules as written here.
Own cards, the GitHub hold, and the WIP limit stay fixed.

- **Own cards.** A member may move, edit, and link its own cards. A card is
  its own when it is assigned to the member, or when it is unassigned and the
  member made it. A member claims unassigned cards and comments on any card
  of its project. It may not move or reassign another agent's card
  (`403 forbidden`). Project admins and the operator may.
- **Dependencies.** A card with unmet dependencies cannot enter the
  `working` column: `409 dependencies_unmet`, `details.blocked_by`. A dependency
  cycle is `422 invalid`.
- **WIP limits.** A move into a full column is `409 wip_limit`. A new card
  in a full column is refused the same way, and the daemon keeps no part of
  it. A move inside one column does not count. A limit lower than the count
  of the column is allowed: the column is over its limit (the dashboard
  shows the count red) and refuses more cards until it is under the limit.
- **Blocked.** A move to the `blocked` column needs a non-blank `comment`
  (`422 comment_required`). The comment goes on the card. The daemon mails
  the operator and the card's creator (when that is not the mover) from
  `system`, on one new thread. Subject: `<card> blocked: <title>`. The
  `moved` event has `from_role` and `to_role` (`null` for a column with no
  role), and each `card` event has `column_role`.
- **Done.** `done` is terminal for members: a member cannot move a done card
  out, edit it, or reassign it (`403`). Only a project admin or the operator
  reopens a card; the reopen is audited. A member cannot make a card in
  `done`. On `done`, the daemon notifies every linked thread (§7.3), and
  each dependent card that is now free gets an `unblocked` event.
- **ETA.** `eta_at` in a new card or a card patch sets the ETA: an RFC 3339
  time, or a duration from now with one or more parts (`45m`, `2h30m`,
  `1d`). The time must be in the future and at most 30 days away; else
  `422 invalid` with a message that says why. `eta_at: null` clears it. The
  rights are those of an edit: the card's own member, a project admin, or
  the operator. A done card takes no new ETA (`409 conflict`). An
  archived card refuses both (`409 conflict`). A change to
  the ETA of a card that is not the actor's own is audited as `card.eta`
  (`{eta_at, previous}`). Each change is a `card` event with `change: eta`;
  it does not start a turn.
- **ETA per column** (TER-155 D1). Every move to another column clears
  the ETA; a reorder in the column keeps it. The `moved` event has the old
  ETA in `previous_eta_at`. An `eta_at` in the same patch as the move
  applies to the new column. The dashboard shows no countdown in
  `accepted` and `done`.
- **ETA of a job** (TER-155 D2). A duration for a card with an open
  (queued or running) job is the job's planned duration, not a time from
  now: a queued job's cards have no ETA, and a running job's cards get its
  start plus the duration (§35.5). An RFC 3339 time, or a card with no
  open job, follows the rule above. A change by an actor that is not the
  card's own is audited as `card.eta` (`{job, planned_secs,
  previous_planned_secs}`).
- **Events.** Every change is a `card` event with the actor (§9).

#### 8.2.1 Column options (TER-1112)

Each column has options: four move checks and the settings of its queue.

| Check | Default | What it does |
|---|---|---|
| `require_dependencies_met` | the `working` column | a move into the column needs met dependencies (`409 dependencies_unmet`), also for a claim and a wait action |
| `require_comment_on_enter` | the `blocked` column | a move into the column needs a non-blank comment (`422 comment_required`) |
| `terminal_for_members` | `done` | the **Done** rule above for a card in the column: only a project admin or the operator moves it out, edits it, or reassigns it (`403`); nobody claims it or gives it a new ETA (`409`); only an admin makes a card in it (`403`) |
| `clear_eta_on_enter` | every column | a card that enters the column loses its ETA (**ETA per column** above) |

The mails on a move to `blocked` and `done`, `done_at`, and the reopen
audit stay with the column role and the name `done`, not with the options.

The queue settings are stored but not used yet (C3 runs the queue):
`release` is `wip` (the default), `batch`, or `job`. Only `batch` takes
`batch_size` (1 to 1000) and `max_wait_seconds` (1 to 2592000), both
required, and `max_batch_cards` (optional, `batch_size` to 1000).

The options belong to the column: a rename or a reorder keeps them. The
board `PATCH` (§8.3) does not carry them, with one exception that keeps
today's behaviour: when a column gets or loses the role `working`, its
`require_dependencies_met` follows the role, and the same for `blocked`
and `require_comment_on_enter`. Such a change needs the right to write the
options (below): for another actor with the board `PATCH` right (a
`sysadmin`), the whole `PATCH` is `403 forbidden` with `details.column` and
`details.options`, and nothing changes. An allowed change raises the
version and is recorded as a `PUT` is. A new column from the board `PATCH`
gets the defaults of its role. Migration `ter-1112-column-options.sql` sets the
defaults on each existing board from its roles and the name `done`.

`GET /api/projects/{name}/board/columns/{column}/options` (a project
reader; a role alias names the column with that role):

```json
{"project": "p", "column": "merge_queue", "version": 3,
 "checks": {"require_comment_on_enter": false, "require_dependencies_met": false,
            "terminal_for_members": false, "clear_eta_on_enter": true},
 "queue": {"release": "batch", "batch_size": 4, "max_wait_seconds": 1800,
           "max_batch_cards": 8}}
```

`PUT` on the same path takes the whole `checks` and `queue` and the
`base_version` that was read, and returns the new options. Only the
operator and a `project_admin` of that project write (`403` for every other
actor, a `sysadmin` too: it reads only). A `base_version` that is not the
current `version` is `409 stale_base` with `details.version`, and nothing is
written. A bad queue is `422 invalid` with `details.path` (for example
`queue.batch_size`). A write that changes something raises `version` by
one, writes one audit row `board.column_options` (`{column, version,
before, after}`), and emits one `board` event (`{column, options_version,
checks, queue}`). A write that changes nothing keeps the version and writes
nothing.

### 8.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/board?done=&limit=&cursor=` | | `200` board with card summaries; Done pages are opt-in |
| `PATCH /api/projects/{name}/board` | `{"columns": [column, ...]}` | `200` board; project_admin |
| `GET /api/projects/{name}/board/columns/{column}/options` | | `200` column options (§8.2.1) |
| `PUT /api/projects/{name}/board/columns/{column}/options` | `{"base_version", "checks", "queue"}` | `200` column options; the operator or project_admin of the project |
| `GET /api/projects/{name}/cards?column=&assignee=&unassigned=&label=&archived=&wait=` | | `200` cards in board order: by column, then by place in the column (the top first), then by number. Each card has its `position` in its column, from 0. Cursor: opaque |
| `POST /api/projects/{name}/cards` | new card | `201` card |
| `GET /api/cards?columns=working,research` | | `200` the active cards of every board the caller may read (below) |
| `GET /api/cards/{id}` | | `200` card, with `questions` (all linked questions, including closed ones) |
| `PATCH /api/cards/{id}` | card patch | `200` card |
| `DELETE /api/cards/{id}` | | `204`; the card is archived |
| `POST /api/cards/{id}/move` | `{"column", "position"?, "comment"?}` | `200` card |
| `POST /api/cards/{id}/assign` | `{"assignee": "<address>" \| "me" \| null}` | `200` card |
| `POST /api/cards/{id}/claim` | none | `200` card, assigned to the caller, in the `working` column |
| `GET /api/cards/{id}/comments` | | `200` comments |
| `POST /api/cards/{id}/comments` | `{"body", "reply_to"?: "comment ID on this card"}` | `201` card |

| `POST /api/cards/{id}/links` | `{"kind", "ref"}` | `201` card |
| `GET /api/cards/{id}/events?since=` | | `200` card events, cursor: `seq`. `since` is a `seq`, an event ID, or a time (§2.1). |
| `GET /api/cards/{id}/sessions` | | `200` the session links (§6.6) of the card's comments and card events, distinct, oldest first |

Human comments, including comments supplied with card creation or moves,
mail the project's project_admin agents and the card's assignee and bound
agent, deduplicated. Agents' ordinary comments and daemon notes do not.
A project admin can disable human comment mail with
`PATCH /api/projects/{name}` and `{"human_comment_notifications": false}`
(default true, including new projects). An agent comment replying to a human
comment with `reply_to`, or mentioning `@operator` as a standalone mention,
mails the operator's Inbox with a card link. The CLI supports
`card comment <id> --reply-to <comment-id> -m <text>`.

**Columns.** The `PATCH` list is the whole board, in order. Each column is
`{"name", "title"?, "wip_limit"?, "role"?, "rename_from"?}`.

- `wip_limit` absent or `null`: no limit. It must be 1 or more.
- `title` and `role` absent: the column keeps its title and role (a new
  column: none). `null` clears them. A title equal to the default (the name
  with spaces) is stored as none, so a later rename takes the new default.
- `rename_from` renames a column and keeps its cards and their order. The
  new name must not be the name of another column.
- A column not in the list goes away. It must have no active cards: `409
  conflict` with `details: {"column", "cards"}`, and the message lists the
  cards. Archived cards of a removed column go to `backlog`.
- `422 invalid`: `backlog` not first or `done` not last, a fixed column
  renamed or given a WIP limit or a role, a name twice in the list, a bad
  name or title, not exactly one `working` column, more than one `blocked`
  column, or a column named `working` or `blocked` without that role.
- The daemon ignores `position` and `count` in the list, so a client may
  send back the columns it read.
- The rights: project_admin of the project (this includes a project lead
  agent), sysadmin, or the operator. Another agent gets `403 forbidden`
  ("editing the board columns needs project_admin of `<project>`, ...").
- The change writes the audit row `board.columns` and a `board` event, both
  with `{"columns": [{"name", "title", "wip_limit", "role", "rename_from"}]}`.

```
PATCH /api/projects/terrarium/board
{"columns":[{"name":"backlog"},{"name":"ready","wip_limit":3},
            {"name":"doing","rename_from":"in_progress","title":"Doing now"},
            {"name":"qa","rename_from":"review","title":"QA"},
            {"name":"stuck","rename_from":"blocked","role":"blocked"},{"name":"done"}]}
```

**New card.** All fields are optional except `title`, which `from_mail` can
supply:

```json
{"title":"Ship the parser","body":"","column":"backlog","assignee":"worker","labels":["parser"],
 "repo":"terrarium","branch":"feat/parser","depends_on":["TER-1"],"from_mail":null,"comment":null,
 "eta_at":"2h30m","github_author":null,"inbox_thread":null}
```

- `column` defaults to `backlog`. Another column follows the move rules, so
  the `blocked` column needs `comment`, and a full column is `409
  wip_limit`. `working` and `blocked` are aliases (§8.1).
- `from_mail` is a message ID the caller may read. The card takes the
  subject as `title` and `From <sender>:\n\n<body>` as `body`, unless the
  request sets them, and links the message's thread.
- `assignee` follows the assign rules.
- `eta_at` follows the ETA rule (§8.2). The daemon sets it last, after the
  assignment.
- `github_author` is a legacy caller assertion, ignored by the author gate.
  Use the issue import endpoint below for a daemon-verified author.
- `inbox_thread` is an Inbox thread that tracks the new card (TER-146,
  §6.13 "Linked cards"). The caller must read the thread: else `404`, and
  no card is made.

**GitHub author gate** (TER-85). It applies when the project has a
`github_authors` list (§5). Without a list there is no gate. A GitHub card
is a card with the label `github` (any case), or a title that starts with
`GitHub #<n>:` (any case). A held card is a GitHub card with the label
`hold`.

- **Create.** An agent makes a GitHub card, and its daemon-verified author is
  missing or not on the list (the case does not matter, and a leading `@`
  is dropped). The card is held. It goes to the blocked column with the
  labels `github` and `hold` and a move comment that says why, whatever
  `column` the request asks for. That is the column with the role
  `blocked`. When no column has the role, the create is `422` and no card
  is made. The request's `comment` becomes a plain
  comment. `assignee` and `eta_at` do not apply. In the same write, the
  daemon opens a question to the operator for the creating agent (§6.8):
  the card is linked, `hold_card` is the card ID, and the options are
  `Release to backlog` and `Keep held`. The question counts toward the
  agent's open questions. When the agent has 50 open, the create is `429`
  and no card is made. A GitHub card from an author on the list, and any
  card that the operator makes, follows the normal rules.
- **Lock.** Only the operator moves a held card, from any column. The
  lock does not depend on the role of the column, so a board change that
  moves or drops the role `blocked` does not open it. A move, a claim, or
  an assign (not an unassign) by an agent, a project admin or a sysadmin
  included, is `403`. No one queues a validate or a merge job for a held
  card, the operator included (`403`, §35.4): a job moves its card as
  the operator. The operator releases the card first. An agent's edit that leaves the card no longer held
  (it takes off `hold`, or takes off `github` and the `GitHub #<n>:` title)
  is `403`. Other edits are allowed. Only the operator answers or retracts
  the hold question, and no one changes it (`403`).
- **No late label.** While the project has a list, an agent's edit that
  turns a card that is not a GitHub card into one (it adds `github` or a
  `GitHub #<n>:` title) is `403`. GitHub work starts with `card new`, so
  it meets the gate.
- **Answer.** The operator answers the hold question with `Release to
  backlog`: the daemon takes off `hold` and moves the card to the backlog
  from any column, in the same write. Any other answer leaves the
  card held. The operator can also edit or move the card directly.

Agent-supplied `github_author` never bypasses the gate. A card with neither
the GitHub label nor title cannot be recognized as GitHub work.
A card that a schedule `card` action (§19) opens as the operator is not
gated. The action never finds a held card: `close` does not move it and
`open` does not comment on it, whatever its `trigger:<key>` label.
**Close shipped issues** (TER-948): `POST /api/projects/{name}/github-issues/close-done`
accepts `{ "repo": "terrarium", "dry_run": true }`. Only the operator or a
`project_admin` of the named project can call it. Link keys are refused.
`terrarium issue close-done --repo terrarium --dry-run` prints each eligible
issue's cards and published versions without writing to GitHub. Omit
`--dry-run` to post a closing comment and close each open issue. The hourly
`github-issues` schedule can invoke this CLI after its import step.

Closure is restricted to project `terrarium` and registered GitHub repository
`wiltaylor/terrarium`. The daemon opens the `github-issues-token` secret at use
time, with the repository's normal project secret/grant checks. A missing,
unavailable, or non-token secret refuses the operation. No token goes to the
caller. Requests go only to `api.github.com`, without redirects.

Cards are matched by `GitHub #<number>:` titles (with no repository or the
selected repository) or exact issue URL links. Every matched card, including
archived cards, must be unheld, in the done role, and labelled with a version
acknowledged by `group built` or published as a repository tag. Label
`version:X.Y.Z` matches tag `vX.Y.Z` or `vX.Y.Z-alpha`, including hand cuts.
Remote tags are read with daemon-owned `git ls-remote --tags` and the registered
repo's normal credentials, never agent assertions. No match means no closure. The comment names
every card and its published versions. A hidden comment marker lets a retry
finish closing without posting again after a failed close. Concurrent callers
are serialized. Successfully observed closures are cached and require no more
GitHub issue calls. Reads use project/number keyset pages (200 cards, at most
20,000 cards and issue identities), indexed URL link lookups (at most 100 per
card), and at most 1000 comments per issue. Exceeding a bound refuses rather
than guessing eligibility. Each run examines at most 20 eligible issues and
makes at most 60 remote calls, including the tag lookup. It reserves 13 calls
before starting each issue. A persisted cursor continues in issue-number order,
wrapping at the end and skipping cached closed issues. Failed attempts advance
the cursor too, so one problematic issue cannot starve the others. Dry runs
change neither the cursor nor the closed cache. The command stops on the first
API error without undoing earlier closures.

For legacy issues without version labels, the operator alone can use
`terrarium issue close-done --repo terrarium --issue 20 --version 0.1.0-alpha`
(optionally `--dry-run`). API fields are `issue` and `version`, required
together. This verifies the exact published tag `v0.1.0-alpha` and still
requires every matched card to be done and unheld. It overrides only missing
or different version labels, never unfinished work or a missing publication.

**Import an issue** (TER-124): `POST /api/projects/{name}/github-issues/import`
with `{"repo":"configured-repo-name","number":123}` returns `200` and the
card. Requires project write access. The daemon fetches the issue from
`api.github.com` using the configured repository's token credential, if any.
Only canonical github.com HTTPS and SSH repository URLs are accepted, redirects
are refused, and pull requests are rejected. No card is created on fetch failure.
An SSH credential or unavailable token does not prevent anonymous public issue
fetches. An inaccessible issue without a token reports that a private repo
needs a granted token (GitHub also returns 404 for missing issues).
The API's author, title and body supply the card. Control characters are removed
from titles and the issue title is capped at 200 characters. An agent's import
starts in backlog only when its verified author is on the project's list.
With no author list configured every agent import is held. Repeated imports return the
same card, including archived or held cards. A daemon-owned import identity
records its canonical URL. Adding a URL link manually cannot forge an import.
The CLI equivalent is `terrarium issue import 123 --repo configured-repo-name`.

**Active cards** (`GET /api/cards`, TER-43, for the dashboard home). The
cards that are not archived in the columns `columns` (a comma list,
default `working,research`) of each board that the caller may read: every
project for the operator and a sysadmin, the own project for another agent.
A column matches when its role or its name (any case) is one of the values,
so `working` finds the column with that role and `research` the column
named `research` (it has no role). By project, then column position, then
card position; at most 500. Each card is a card with more fields:

- `column_kind`: `working` or `blocked` for the column with that role,
  else the column name in lower case (`research`).
- `blocked_since` and `blocked_reason` (TER-51), only for a card in the
  blocked column: when the card moved there and the comment of that move
  (what is needed), from the last `moved` event to the blocked column.
  Without that event, `updated_at` and the newest comment. The dashboard
  home lists them with `?columns=blocked`.

### Home board and Needs attention (TER-689)

The Home projections are separate from the legacy `/api/cards` list:

| Method and path | Query | Response |
|---|---|---|
| `GET /api/home/cards` | `column=in_progress` (default), `review`, `accepted`, or `blocked` | A page of compact cards for that Home column |
| `GET /api/home/monitors` | | A page of monitors whose latest result is `warn`, `crit`, or `unknown` |
| `GET /api/home/pipelines` | | A page of pipelines whose newest run is `failed` |

All three accept optional `project`, `limit` (1–100, default 100), and `cursor`.
The operator, sysadmins and architects read across projects. Other agents read
only their own project, including project admins. An explicit unreadable project
returns `403`. These are the same read rights as the cross-project cards API.

Each response is `{"items": [...], "next": "cursor-or-null"}`. Pages examine
at most 100 indexed candidates, in ID order. Filtering can yield fewer items
than `limit`, including zero. **Follow `next` even after an empty page**, until
it is `null`. Cursors belong to the same endpoint, column and project filter.
Concurrent changes are not a snapshot. Archived cards are excluded.

Card items have `id`, `title`, `labels`, `project`, `column`, `home_column`,
`assignee`, `assignee_name`, `assignee_status`, and `eta_at`. Nullable fields
are `null` when unset. Labels include type labels unchanged. In the `blocked`
Home column, `blocked_reason` is the first line of the card's newest move's
comment, only when that move targets its current board column. Otherwise it is
`null`, including cards created in blocked without a move. No earlier move or
unrelated comment is used. This also applies to custom Home column selectors.
The full card can be opened with
`GET /api/cards/{id}`. No bodies or comment histories are loaded in this list.

Monitor and pipeline items have `id`, `project`, `name`, `status`, and `link`
(the project's dashboard or pipelines page). Monitors appear independently of
dashboard layout membership. A monitor with no results is `unknown`. A pipeline
with no runs is not failed. A later successful run removes an earlier failure
from this list. Run ordering is the same as the pipeline runs API. A polling
error alone is not a failed run.

**Home board columns** is a project setting returned as `home_board_columns`.
Project admins change it through `PATCH /api/projects/{name}`. File-managed
projects retain the existing API edit restriction. The value replaces all four
arrays. Empty arrays hide a Home column for that project. Selectors resolve by
role first, then by case-insensitive column name. Missing columns are ignored.
Legacy names `in_progress` and `In Progress` also match `working` when no working
role or working-named column exists. Defaults, including for existing projects:

```json
{
  "in_progress": ["working", "research"],
  "review": ["review", "merge_queue"],
  "accepted": ["accepted"],
  "blocked": ["blocked"]
}
```

Each array accepts at most 32 nonempty selectors, each at most 128 bytes.
The CLI accepts the same object:

```sh
terrarium project set alpha --home-board-columns '{"in_progress":["working","research"],"review":["review","merge_queue"],"accepted":["accepted"],"blocked":["blocked"]}'
```

**Card patch.** Every field is optional:
`title`, `body`, `labels`, `add_labels`, `remove_labels`, `repo`, `branch`,
`depends_on`, `column`, `position`, `assignee`, `comment`, `eta_at`.

- `repo`, `branch`, `assignee`, and `eta_at` take `null` to clear.
- `labels` and `depends_on` replace the whole list. `[]` removes every
  label or dependency, and `blocked_by` follows. An absent key keeps the
  list. The change is one `edited` event that names the fields.
- `add_labels` adds labels and keeps the others (a label the card has
  stays once); `remove_labels` takes labels away (an absent one is no
  error). Both apply to the labels of the card at the write, so two changes
  at the same time do not lose each other's labels. With `labels` too:
  `422 invalid`. The `edited` event names `labels`.
- `eta_at` comes last, after the move and the assignment.
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

### 8.4 Card files

A card has files: markdown documents, text, data files, images, and PDFs
(PLAN §13). The operator and the agents attach them; the assignee gets
read-only copies in its workspace.

A file (the latest version of one name):

```json
{"card":"TER-1","name":"spec.md","version":2,"size":1204,"sha256":"9f2c…","media_type":"text/markdown; charset=utf-8",
 "created_by":"operator","created_by_name":"operator","created_at":"2026-11-04T08:00:00.000Z","versions":2}
```

A version has the same fields without `card` and `versions`.

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/cards/{id}/files` | | `200` the live files, the latest version of each, by name |
| `POST /api/cards/{id}/files?name=&replace=` | multipart, JSON, or raw bytes | `201` the file with `outcome: added \| replaced`; `200` with `outcome: unchanged` |
| `GET /api/cards/{id}/files/{name}?version=&format=` | | `200` the bytes; `format=json`: the version with `card` and `data_base64` |
| `GET /api/cards/{id}/files/{name}/versions` | | `200` the live versions, newest first |
| `DELETE /api/cards/{id}/files/{name}` | | `204`; every version of the name is removed |

**The upload.** Three forms:

- `multipart/form-data`: a `file` part, and optional `name` (else the file
  name of the part) and `replace` (`true`) parts.
- `application/json`: `{"name", "data_base64", "replace"?}`. The CLI sends
  this form: every transport, the runner tunnel too, carries JSON.
- Any other `Content-Type`: the raw bytes, with `?name=`.

`?replace=true` works with each form.

**Names.** One path segment: ASCII letters, digits, `.`, `_`, `-`, `+`, and
spaces inside the name, at most 128 bytes. No leading `.` (a hidden name),
no leading or trailing space, no trailing `.`. `index.md` is not allowed:
it is the name of the index in the card dir of an agent. A name is unique
on a card with no regard to case; the API finds a file by its name in any
case. A bad name is `422 invalid`.

**Types.** The extension picks the type from an allowlist, and the bytes
must agree:

| Extension | `media_type` | Check |
|---|---|---|
| `md`, `markdown` | `text/markdown; charset=utf-8` | UTF-8, no NUL |
| `txt` | `text/plain; charset=utf-8` | UTF-8, no NUL |
| `json` | `application/json` | UTF-8, no NUL, a JSON value (else `422 invalid`) |
| `yaml`, `yml` | `application/yaml` | UTF-8, no NUL |
| `csv` | `text/csv; charset=utf-8` | UTF-8, no NUL |
| `png`, `jpg`, `jpeg`, `gif`, `webp` | `image/png`, `image/jpeg`, `image/gif`, `image/webp` | the magic bytes |
| `svg` | `image/svg+xml` | UTF-8, no NUL, an `<svg` root element |
| `pdf` | `application/pdf` | starts with `%PDF-` |

Another extension, or bytes that do not agree, is `415
unsupported_media_type`. An empty file is `422 invalid`. The daemon
redacts the text types on the way in (PLAN §16b.6), as it redacts artifacts.

**Caps.** A file has at most `TERRARIUM_CARD_FILE_MAX_BYTES` (default
10 MiB). All the live versions of all the files of a card have at most
`TERRARIUM_CARD_FILES_MAX_BYTES` (default 50 MiB). Over a cap: `413
too_large`, with `details` (`size`, `max_file_bytes`; or `used`, `size`,
`max_card_bytes`).

**Versions.** A file of a name that the card has already is `409 conflict`
(`details: {"name", "version"}`), unless `replace` is set. A replace adds a
version and keeps the old ones. A replace with the same bytes as the latest
version adds nothing (`200`, `outcome: unchanged`). A replace of a name that
the card does not have adds the file. The version number is one more than
the highest version that the name had, removed versions too.

**Remove.** `DELETE` is a soft delete: every version of the name gets
`deleted_at`, and the file leaves the list. The rows and the bytes stay for
`TERRARIUM_TRASH_DAYS` (default 7); then the prune timer deletes the rows,
and the blobs that no row uses. The name is free again at once.

**The bytes.** The daemon keeps each blob once, in
`<state>/card-files/<sha256>`. The state dir is in each backup (§32). A
download has `Content-Disposition: attachment`, `X-Content-Type-Options:
nosniff`, `Content-Security-Policy: sandbox; default-src 'none'; …`,
`ETag` (the SHA-256), and `X-Terrarium-File-Version`. So no file renders as
a page of the daemon origin: an SVG opened in a tab runs no script. The
dashboard shows the raster images inline from blob URLs with a fixed image
type, renders markdown with its safe renderer, and shows SVG and PDF files
as downloads only.

**Rights.** A change (an upload, a replace, a remove) has the rights of a
card edit (§8.2): the card's own member (its assignee, or the creator of an
unassigned card), a project admin, or the operator. A done card takes
changes from an admin or the operator only. An archived card refuses all
changes (`409 conflict`). A read has the read right of the project: each
member of the project, a sysadmin, and the operator.

**Events and audit.** An add emits `card.file_added`, a replace
`card.file_replaced`, a remove `card.file_removed`. Each has the top-level
`card`, the project, the assignee as its agent, and `data: {change, name,
version, size, sha256, media_type, title, column, …}` (a remove: `{name,
version, versions}`). Each writes the audit row `card.file_add`,
`card.file_replace`, or `card.file_remove`. The card's `updated_at`
changes.

**Delivery to the assignee.** The files of each card that is assigned to an
agent, is not archived, and is not in `done` go into the agent's workspace:

```
<workspace>/.terrarium/cards/<card-id>/<name>    the latest version, mode 0444
<workspace>/.terrarium/cards/<card-id>/index.md  the title, the links, the file list
```

- At each start of the agent, before the harness starts.
- While the agent is live: at once on a file event of the card, and on an
  assign, a claim, a move, or an archive of a card. The agent gets a mail
  from `system` (a `[Mailbox:system]` item) for an added, replaced, or
  removed file, and for a new assignment of a card that has files. Its own
  change mails it nothing.
- The daemon removes a card dir of a card that the agent does not have any
  more, and each entry in a card dir that is not a file of the card. An
  edit of a delivered file is put back at the next delivery.
- The writes are fd-relative and follow no symlink: a symlink or a file at
  `.terrarium` or `.terrarium/cards` stops the delivery (a warning in the
  log); a symlink in the place of a card dir or a file is removed, never
  followed. `/.terrarium/` goes into `.git/info/exclude` of a workspace
  that is a git repo, so the agent does not commit the files.
- An agent reads the files of any card of its project on demand: `terrarium
  card files get <card> [name]`.

```
POST /api/cards/TER-1/files            (X-Terrarium-Agent: terrarium/worker)
{"name":"notes.md","data_base64":"IyBOb3Rlcw==","replace":true}

201
{"card":"TER-1","name":"notes.md","version":3,"size":7,"sha256":"…","media_type":"text/markdown; charset=utf-8",
 "created_by":"01M5PA0T0Z0K16V6G88P3SE78W","created_by_name":"terrarium/worker","created_at":"…","versions":3,
 "outcome":"replaced"}
```

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
| `project`, `card`, `session` | Absent when they do not apply. `session` is the session that the event recorded: the agent's current session when the daemon wrote the event. |
| `session_id`, `session_label` | The link to the agent's session (see "Session links" below). Absent when there is no link. |
| `summary` | A short line for lists. |
| `data` | The kind's payload. |

**Session links.** An event, a message (§7), and an audit row (§4,
`GET /api/audit`) can link to the agent session that was active when the
row was written. The row then has two more fields:

| Field | Meaning |
|---|---|
| `session_id` | The session ID. `GET /api/sessions/{id}` (§6.6) reads the session; the dashboard page is `/sessions/<id>`. |
| `session_label` | `<agent> · <start time>`, for example `worker · 2026-10-24 06:36 UTC`. The agent name is without the project. The time is the start of the session, to the minute, in UTC. |

The daemon finds the session as follows:

- An event: the session that the event recorded (`session`). An event
  without one (an event from before the daemon recorded it) uses the
  session of the event's `agent_id` at the event's `at`.
- A message: the session of the sender at `created_at`, when the sender is
  an agent. The daemon does not record which turn of the recipient read a
  message, so a message does not link to a session of the recipient.
- An audit row: the session of the actor at `at`, when the actor is
  `agent:<ulid>`.

"The session at a time" is the agent's session with `started_at` at or
before that time, and `ended_at` absent or at or after that time. When two
sessions match (a resumed session keeps its first start), the later start
wins. When no session matches, there is no link.

The two fields are present only when the reader may read the session: the
operator, a member of the session's project, or a sysadmin (the rule of
`GET /api/sessions/{id}`). A reader that sees the row but not the session
gets the row without the fields. A session of a deleted agent links only
while its archived session file is there (`TERRARIUM_SESSION_ARCHIVE_DAYS`;
see §6.6, "Retention"). The fields are on the JSON pages, the WebSocket and SSE
frames of `GET /api/events` and `GET /api/agents/{id}/events`, the `audit`
frames, `GET /api/mail`, `GET /api/mail/{id}`, and `GET /api/audit`. The
daemon finds the sessions of a page in one step, with the
`sessions(agent_id, started_at)` index.

Kinds:

| `kind` | `data` |
|---|---|
| `status` | `{status, from, reason, waits, waiting_on?}`. `from` is never the same as `status`: a runner report of the status the agent already has (for example the runner's `spawned <program>` at start) goes in as a `system` event `{message, status}`. `waits` is the number of open waits; `waiting_on` is the line of §6.7. |
| `agent.woken` | `{cause, queued}`: the daemon woke the agent (§6.2). `cause` is `mail`, `turn`, `card`, `nudge`, `start`, `unpause`, `hold_end`, `queued`, `reset`, or `daemon_start`; `queued` is the work that waited. A `status` event follows. An idle stop is a `status` event with `reason: "idle"`. |
| `agent.paused`, `agent.unpaused` | `{paused, queued}`: a pause or an unpause changed the flag (§6.2). The actor is the caller. |
| `agent.reset` | `{kind, by, then_set, deferred, replaced, state}`: an actor asked for a compaction or a new session (§6.2, "Compact and new session on request"). `kind` is `compact` or `new_session`; `by` is `self`, `operator`, or `<project>/<agent>`; `state` is `started`, `queued`, or `next_wake`. The actor is the caller. A queued reset that cannot run and is gone writes it again with `state` `dropped` and `reason` (actor `system`). |
| `agent.compacted` | `{cause, ok, duration_ms, reason?}`: a compaction that the daemon ran ended (§6.2, "Auto compact"). `cause` is `idle`, `max_age`, or `manual` (also a requested one). `ok` is false for a failure that the harness reports, a stop of the agent, or no end signal in 5 minutes; `reason` says which. |
| `compact_started`, `compact_finished` | `{trigger?}` and `{ok, trigger?, message?}`: the harness reported the start or the end of a compaction (docs/compat.md, "Compaction"). `trigger` is the harness's word (`manual`, `auto`, `threshold`, ...). A compaction that the harness starts by itself also writes them. |
| `agent_removed` | `{agent_id, project, name}`. The daemon deleted the agent: `DELETE /api/agents/{id}`, a fleet apply with prune, or a definitions sync that removes a file agent. The delete of a live agent stops it first, so a `status` event with `stopped` comes before this event. Clients remove the agent from their lists. `GET /api/agents` does not show the agent again. |
| `wait` | `{wait_id, change, kind, summary, state, result?}`. `change` is `opened`, `resolved`, `timed_out`, `cancelled`, or `failed`. The `agent` is the waiter (§6.7). |
| `watch` | `{watch_id, change, watcher, watcher_name, on, status?}`. `change` is `created`, `fired` (with `status`), or `removed`. The `agent` is the watched agent (§6.7). |
| `question` | `{question_id, kind, change, status, priority, text, revision, agent_id, card_id?}`. `kind` is `question`, `notice`, `bundle`, or `tool_run`. `change` is `asked`, `updated`, `answered`, `acknowledged`, or `retracted`. The `agent` is the asking agent (§6.8). |
| `inbox` | `{item, kind, change, project?}`: an item of the Inbox (§6.9) was `opened`, `updated`, or `closed`. `kind` is `question`, `notice`, `mail`, or `system`. |
| `session_started` | `{harness_session, transcript_path?, source?}` |
| `turn_start`, `turn_end`, `notification`, `question`, `session_ended`, `exit`, `delivery_failed`, `system`, `warning`, `usage_limit` | The fields of the core `Event` variant of the same name (`crates/terrarium-core/src/event.rs`). A `warning` (`{message}`) needs the operator's attention, for example a harness version outside its tested range at a start (§6.2). A `usage_limit` (`{message, resets_at?}`) is the provider's refusal of a turn for a usage limit or a rate limit (§16.6). |
| `tool_use`, `tool_finished` | `{tool, tool_id?, summary?, ...}`: `input` (`tool_use`), `ok` (`tool_finished`). `tool_id` is the harness's ID for the tool call (for Claude Code, `toolu_...`). It is not the event `id`. Older records used the key `id` for it; the `events.jsonl` reader still takes those. |
| `mail` | `{message_id, thread_id, from, from_name, to, to_name, subject}`. The `agent` is the recipient. A message that the loop guard holds also has `held: true`, `held_reason`, and `thread_state: "paused"`. A release (§7.4) emits the event again with `released: true`. |
| `card` | `{card_id, change, title, column, ...}`. `change` is `created`, `moved` (`from`, `to`, `comment`, `assignee`, `previous_eta_at`: the ETA that the move cleared), `reordered`, `assigned` (`assignee`, `previous`), `claimed`, `edited` (`fields`), `commented` (`comment`), `linked` (`kind`, `ref`), `archived`, `unblocked` (`by`), `thread_update` (`thread_id`, `done`), `eta` (`eta_at`, `previous`; `eta_at` is `null` when cleared; `job: true` when the job queue set it, §35.5), `reminded` (`eta_passed`: the daemon reminded the idle assignee to update the card, §6.2 "Card reminder"), `idle` (`idle_since`, `mailed`: the idle alert marked the card and mailed the leads, §6.2 "Idle alert"), or `idle_cleared` (`idle_since`: the mark went). `actor_name` names who made the change (`operator` or `project/name`). The `agent` is the assignee. |
| `board` | `{columns}` after a column change. |
| `question_guard` | `{question, mailed}`: a member ended its turn with a question in plain text and reported nothing, so the daemon set `blocked` (reason "asked a question in plain text: ...") and mailed `mailed` (the leads, or `operator`). `question` is the first 200 characters. The actor is `system`. PLAN §8.4. |
| `input_watchdog` | `{reason, mailed}`: the runner saw an idle agent keep unsubmitted text in the input box of its harness for the submit timeout and set `blocked` (reason "unsubmitted text in the input box"), so the daemon mailed `mailed` (the leads, or `operator`). Text that a person typed at the terminal or with `agent keys` is not flagged. The agent goes back to `idle` when the box stays empty for 1 s, so one stuck box gives one mail. The mails and the event go in together or not at all. The actor is `system`. TER-208, TER-224. |
| `memory.created`, `memory.updated`, `memory.deleted`, `memory.linked`, `memory.unlinked`, `memory.superseded` | `{memory_id, key, title, ...}`: `fields`, `to`, `relation`, `note`, `by`. |
| `wiki.created`, `wiki.saved`, `wiki.deleted`, `wiki.restored`, `wiki.moved`, `wiki.reviewed` | `{page_id, path, title, rev, ...}`: `summary`, `author`, `from_rev`, `from`, `decision`, `comment`. |
| `artifact.created`, `artifact.linked`, `artifact.deleted`, `artifact.pruned` | `{artifact_id, key, uid, name, kind, ...}`: `size`, `content_type`, `sha256`, `link_kind`, `ref`. |
| `version.created`, `version.updated`, `version.submitted`, `version.approved`, `version.rejected`, `version.released` | `{version_id, name, status, ...}`: `fields`, `comment`, `thread_id`, `cards`. |
| `usage.sample`, `usage.rejected_path`, `quota.sample`, `quota.alert`, `quota.blocked`, `quota.resumed`, `subscription.offer`, `subscription.expired`, `subscription`, `self_report` | See §16.7. |
| `lease` | `{change, target, lease_id, expires_at, exclusive, state}`. `change` is `granted`, `extended`, or `ended` (a revoke or the expiry). The `agent` is the lease holder. The supervisor types a pointer from it (§17.7). |
| `target` | `{change, target, ...}`. `change` is `reconciled` (`from`, `state`, `snapshots_cleared`): a `status` found the target other than the store said. `snapshots_dropped` (`snapshots_dropped`): a snapshot list found that the host lost stored names. `snapshot_deleted` (`snapshot`, `scope`, `machines`, `agent`?): a snapshot delete (§17.3). |
| `schedule` | `{change, schedule, name, project, run?, status?, trigger?}`. `change` is `created`, `updated`, `deleted`, `enabled`, `disabled`, `run_started`, or `run_finished`. `schedule` is the schedule ULID. A run event has `run`, `status`, and `trigger` (§19). |
| `harness` | `{change, harness, package, from, to, forced}`. `change` is `updated`: an update in the tools dir (§4, `POST /api/harnesses/{name}/update`); `auto_updated`: an install of the auto-update check; `pinned` or `unpinned`: a pin or an unpin (§4, "Auto-update and pins"; `unpinned` has `was` in place of `from` and `to`). |
| `host.connected`, `host.disconnected` | `{host, host_id, ...}`: `version`, `os`, `arch`, `capabilities`, `summary`, `resumed` (sessions taken back), `reason`. See §17. |
| `host.updated` | `{host, host_id, from, to}`: a host agent that was stale connected with the daemon's build (a self-update or a reinstall). The audit entry is `host.update`. |
| `host.update_failed` | `{host, host_id, failed, running}`: the self-update to `failed` did not start, and the host agent runs `running` again. One time for each version while the daemon runs. The audit entry is `host.update_failed`. |
| `event_source` | `{change, source, name, key, kind, enabled}` after a change of an event source (§20). |
| `system_prompt` | `{change, name, path}` after a write through the API of the system prompt library (§28). `change` is `created`, `updated`, or `deleted`. An edit of a file by hand gives no event. |
| `test_run` | `{change, project, id, name, counts, cards, labels}` after a change of a test run (§29). `change` is `created`, `linked`, `unlinked`, or `deleted`. `cards` are card IDs and `labels` are labels, such as `version:1.4.0`. |
| `monitor.result` | `{project, monitor, monitor_id, status, value, unit, message, error, at, duration_ms, result_id}` for each result of a monitor (§30.3). |
| `monitor.status` | `{project, monitor, monitor_id, from, to, value, unit, message, error}` when the status of a monitor changes; `from` is `null` for the first result (§30.3). |
| `dashboard.updated` | `{project, blocks}` after a save of the dashboard layout (§30.4). |
| `target.grant` | `{target, project, for, change}` after a grant or a revoke of a global target to a project (§30.7). `change` is `granted to` or `revoked from`. |
| `webhook.delivery` | `{trigger, name, delivery, status, http_status, attempt, test, event_seq}` when an attempt of a webhook delivery ends (§19.5). `status` is `ok`, `pending` (a retry waits), `failed`, or `giving_up`. The actor is `schedule:<id>`; a project trigger's event has its project. |
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
| Anything else | `200` one JSON page of events after `since`, oldest first, cursor: `seq`. With `order=desc`, newest first. |

| Query | Meaning |
|---|---|
| `since` | A `seq`, an event ID, `latest`, or a time (§2.1). Only later events come; a time gives the events at or after it. Default: 0, the whole log. `latest` skips the backlog: only events after the newest `seq` at connect time come. |
| `live` | `1` or `true`: the same as `since=latest`. It wins over `since`, a cursor, and `Last-Event-ID`. `0`, `false`, or empty: off. Another value is `400`. |
| `order` | JSON page only. `asc` (the default): oldest first, and the cursor goes on to newer events. `desc`: newest first, and the cursor goes on to older events. `since` is then the lower bound of every page: the pages stop at it. Another value is `400`. A stream with `desc` is `400`. |
| `kind` | One kind. |
| `project` | Firehose only: one project. |
| `agent` | Firehose only: an agent key or ULID. A ULID works after the agent is deleted. |
| `audit` | Firehose stream only: `1` or `true` adds the `audit` frames (below). `0`, `false`, or empty: off. Another value is `400`. |

The firehose shows an agent (not a sysadmin) only the events of its project
and its own events. To resume after a disconnect, reconnect with
`since=<last seq>`.

**Audit frames.** With `audit=1`, a firehose stream (WebSocket or SSE)
also sends one frame for each new or changed audit row (§4,
`GET /api/audit`). A changed row is an aggregate row such as the count of
`auth.login_failed`; the frame has the full row again, with the same `id`.

```json
{"kind":"audit","audit":{"id":"01M5PA0S0RK019JM7AD46MKF6B","seq":42,"at":"2026-10-24T06:36:53.144Z",
 "actor":"operator","action":"agent.start","target":"01M5PA0T0Z0K16V6G88P3SE78W",
 "project":"terrarium","detail":{}}}
```

- The read rule of `GET /api/audit` applies. The operator and a sysadmin
  get every row. A project admin gets the rows of its project. Another
  agent gets no `audit` frames, and the stream sends its events as usual
  (no error).
- `project` limits the frames to the rows of that project. `kind` and
  `agent` do not apply to them.
- An `audit` frame has no `seq`: `audit.seq` is the position in the audit
  log, not in the event log. Over SSE it is `event: audit` with no `id:`,
  so `Last-Event-ID` stays an event `seq`.
- The frames are live notifications only. The event log does not keep
  them, a stream does not replay them, and a slow client can miss some.
  After a reconnect, read `GET /api/audit` for the full list.

`GET /api/events/latest` answers `200 {"seq": 1234}`: the newest `seq` of the
log, `0` when the log is empty. A client that shows recent activity reads a
short JSON page (for example `?since=<seq - 50>`), then streams with
`since=<seq>`. It does not replay the whole log.

A client that shows the log newest first reads `?order=desc` for the
first page: the newest events. Each `X-Next-Cursor` gives the next older
page. The client streams with `since=<seq of the first event of the first
page>` and puts the live events on top. An event that comes between the
page and the stream is in the backlog of the stream, so no event is lost.

```sh
curl -s --unix-socket $S 'http://localhost/api/agents/terrarium%2Fworker/events?since=0&limit=3'
curl -s -N --unix-socket $S -H 'Accept: text/event-stream' 'http://localhost/api/events?kind=status'
curl -s --unix-socket $S 'http://localhost/api/events?order=desc&limit=50'
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
  `project/path` for a page, the card ID, `project/MEM-n`, or `project/ART-n`.
  The daemon reads `[[path]]`, `[[path|label]]`,
  `[[TER-42]]`, `[[MEM-17]]`, `[[v0.3.0]]` (a page link to `project/v0.3.0`,
  not a version link),
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
 "links":[{"kind":"label","ref":"version:0.3.0","created_by":"...","created_at":"..."}],
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
  and `label` (`ref` is the label, such as `version:0.3.0`; TER-58). `embedded_in` lists the live pages that
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

- `kind` defaults to `file`. `attach` is optional. Its `ref` is a page key
  or a card ID of the same project, or a label.
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
- The daemon runs at most 8 uploads at one time. An upload that finds no
  free slot within 3 s gets `429 rate_limited` with `Retry-After: 2` and
  `details` (`retry_after`, `max_concurrent_uploads`). It stores nothing
  (TER-605). `terrarium artifact push` and `terrarium test push` try a `429`
  again after `Retry-After` (else `details.retry_after`, else 2 s, at most
  60 s), at most 5 tries in all, then fail with the error (TER-661).

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
| `GET /api/projects/{name}/artifacts?label=&limit=&cursor=` | | `200` artifacts newest first, cursor: number; `label` keeps the artifacts with that exact label |
| `POST /api/projects/{name}/artifacts` | §12.2 | `201` artifact; `429 rate_limited` with `Retry-After` when all 8 upload slots stay busy for 3 s (§12.2) |
| `GET /api/projects/{name}/artifacts/usage` | | `200 {"project", "count", "used_bytes", "quota_bytes", "max_file_bytes"}` |
| `GET /api/artifacts/{id}` | | `200` artifact |
| `DELETE /api/artifacts/{id}` | | `204`; the author or an admin |
| `POST /api/artifacts/{id}/links` | `{"kind": "page" \| "card" \| "label", "ref"}` | `201` artifact |
| `POST /api/artifacts/{id}/url` | none, or `{"ttl"?}` | `200 {"url", "exp", "expires_at"}` |
| `POST /api/artifacts/prune` | `{"dry_run"?, "as_of"?}` | `200` prune report; sysadmin or operator |

- Every member of the project uploads, reads, and attaches. A label is
  free text (1 to 200 characters, trimmed, no control characters); the
  daemon does not check that a card has it. The kind `version` gives
  `422 invalid` with a message that names `label`.
- An admin delete of another author's artifact writes `artifact.delete`. The
  blob goes when no artifact uses it.
- A label link does not block deletion.

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

- The rule keeps an artifact that is attached to a live page, a card, or a
  label, or that a live page embeds. Old version links no longer retain artifacts.
  They go through the same warning and prune as other unattached artifacts.
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

## 13. Versions (removed)

TER-58 removed the version objects. Versions are card labels:
`version:vnext` puts a card in the next version, and at the cut the lead
renames it to `version:<X.Y.Z>`. `GET /api/projects/{name}/cards?label=`
lists the cards of a version. A test run (§29) or an artifact (§12) links
to the label. The operator's review of a result is a question (`terrarium
question ask`) with the artifacts linked.

Every `/api/projects/{name}/versions` and `/api/versions/...` route gives
`404`. The `version.*` events are gone. The tables `versions`,
`version_cards`, `version_artifacts`, `version_reviews`, and
`test_run_versions` and their indexes are dropped by TER-128. No current data
uses these legacy tables. Artifact deletion (§12.3) and prune (§12.5) no
longer read them. The migration irreversibly discards the legacy metadata.

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
`usage_collector.rs`, `quota_policy.rs`, `login_session.rs`, and for the
plan accounts (PLAN §16.3) `accounts.rs` and `account_swap.rs`. The tests
are in `crates/terrariumd/tests/subscriptions.rs`, `tests/usage_cards.rs`,
`tests/runtime.rs` (the swaps), and `tests/no_token_endpoint.rs` (the
refresh rule).

### 16.1 Shapes

A subscription:

```json
{"name":"anthropic-max","provider":"anthropic","plan":"Max 20x","harnesses":["claude"],
 "probe":"anthropic-oauth-usage","alert_at":80,"source":"file","login_status":"logged_in",
 "verified_at":"...","probe_status":"ok","last_probe_at":"...","last_success_at":"...",
 "created_at":"...","updated_at":"...",
 "windows":[{"window":"5h","used_pct":16.0,"resets_at":"2026-10-24T10:59:59Z","at":"..."},
            {"window":"7d","used_pct":10.0,"resets_at":"2026-10-31T01:59:59Z","at":"..."}],
 "local_estimate_only":false,"has_api_key":false,"held_agents":0,"default_models":{},
 "account":{"harness":"claude","has_login":true,"login":"wil@example.com","source":"migration",
            "created_at":"...","updated_at":"..."},
 "agents":[{"id":"01K8...","name":"lead","project":"terrarium"}],"default":true}
```

| Field | Meaning |
|---|---|
| `source` | `file` (`subscriptions/<name>.toml`), `api` (the API, the login wizard, an API key), or `fleet:<name>` (a fleet file, §18.1). A file subscription whose file is gone becomes `api` on reload; its samples stay. |
| `login_status` | `logged_in`, `expiring`, `expired`, `missing`, `unknown` (not checked yet), or `api_key` (a stored Z.ai or Moonshot key, or a self-hosted server: no harness status command can check it, §16.5). See "Expired logins" in §16.2. |
| `harnesses` | The harnesses that may spend it. pi uses API keys only: a plan login (a provider that sells a plan, Claude, ChatGPT, Copilot, or any OAuth login, with no `plan` "API key") never lists `pi`. The daemon drops `pi` from such a definition when it stores it, and from an older row when it reads it. A `moonshot` subscription lists `pi` only (§16.5). |
| `probe_status` | `ok`, `failed` (`probe_error` says why), or `none` (no probe, or not run yet). `last_success_at` stays when a probe fails. |
| `display_name` | The name for people: `label` when the operator set one, else the vendor's own product and plan name, for example `Claude Ultra`, `Claude Max`, `ChatGPT Pro`, `GitHub Copilot Pro`, `GLM Coding Plan Max`, `Kimi Code Plus`, `Kimi API Platform`, `OpenRouter`, or `Self-hosted <host>`. `docs/compat.md`, "Display names", has the table and its sources. A stored plan of an older name shows by the name of today (`Max 20x` is `Claude Ultra`). `name` stays the id. Always present. |
| `label` | The display name that the operator set (`PUT /api/subscriptions/{name}/label`). Absent when not set. Daemon state: a file subscription can have one too. |
| `windows` | The newest sample of each window that the last good probe reported. Window names: `5h`, `7d`, `7d_<model>` (a weekly limit of one model, for example `7d_fable`), `7d_opus`, `7d_sonnet`, `extra` (extra usage, when it is on) (Anthropic); `<n>h` or `<n>d` from the window length (OpenAI); `monthly` (Copilot premium requests). An Anthropic Ultra plan has no `5h` window. A good probe removes each stored window that its answer does not have, with its samples (§16.2); a failed probe removes nothing. |
| `local_estimate_only` | `true` when there is no probe: local accounting is the only number. |
| `no_quota` | `true` for a self-hosted server (§16.5): it has no quota at all, so the quota UI shows the usage counts only. Always present. |
| `enabled` | `false` when the operator disabled the subscription (§16.2, "Disabled subscriptions"). Daemon state: a file subscription can be disabled too. Always present. |
| `selfhosted` | The server of a `selfhosted` subscription (§16.5): `{api, base_url, models?, default_model?, context_window?, max_output_tokens?, max_concurrent?, status_url?}`. Absent for every other provider. No secret. |
| `streams` | The streams of a self-hosted subscription with `max_concurrent`: `{max, in_use, waiting}` (§16.2, "Stream limit"). `in_use` counts the agents that run a turn now; `waiting` the agents whose work waits for a free stream. Absent with no limit. |
| `status_url` | The public status page of the provider: `anthropic` `https://status.anthropic.com`, `openai` `https://status.openai.com`, `github` `https://www.githubstatus.com`, `openrouter` `https://status.openrouter.ai`, `moonshot` `https://status.moonshot.cn`; for `selfhosted` its own `status_url`. Z.ai has no official status page, so it has none. Absent when none is known. The table is `terrarium_core::providers::status_url`. The dashboard only links to it; nothing fetches it. |
| `env_var` | Set when an API key is stored: the env var that carries it into the agents. The key is never in the API. |
| `held_agents` | Agents whose turns wait after a usage-limit error on this subscription (cause `limit:<subscription>`, §16.6). |
| `account` | The plan account of the subscription (PLAN §16.3), when it has one: `harness` (whose login it holds), `has_login` (the credential file is there), `login` (the Copilot login name, or the email of a Claude login), `source` (`migration`: the shared login moved here at a daemon start; `login`: the login wizard), `created_at`, `updated_at`. Never a secret. |
| `agents` | The agents whose current `subscription` this is: `[{id, name, project}]`. Always present. |
| `assigned_agent_count` | List responses only: effective stored assignments among actor-readable agents, all statuses and extant ephemeral agents. Explicit subscription wins, otherwise Claude, Codex and Copilot inherit their harness default. Pi does not inherit. Disabled subscriptions retain assignments. Known zero is `0`; absent means unavailable, not zero. |
| `assigned_agent_count_scope` | With the count: `all_projects` for the operator and existing all-project readers, otherwise `project`, with `assigned_agent_count_project` naming the reader's project. Not a running-agent count or pool membership count. Single-item and mutation responses omit these fields. |
| `session_period` | `{since, until}` in RFC 3339 UTC. The longest active probe window (`monthly`, `<n>h` or `<n>d`), otherwise the current UTC calendar month. The start is inclusive and the end exclusive. |
| `sessions` | Newest-first sessions overlapping `session_period`, including retained sessions of removed ephemeral agents. Each has `id`, `agent_id`, `agent` (name), `project`, `harness`, `started_at`, optional `ended_at`, `removed`, and nullable `model`, `card`, `input_tokens`, `output_tokens`. Token totals count only usage within the period. Usage samples attribute historical subscriptions, otherwise the live agent's current subscription is used. Model and card are representative values when several occur. Cost is not available. The dashboard and CLI subscription show consume this same list. Earlier-period navigation is not provided. |
| `default` | `true` when the plan account is the default account of its harness (PLAN §16.3): a Claude Code, Codex, or Copilot agent with no subscription runs on it. Each harness has one default or none. Always present. |
| `default_models` | The model that each harness gets on this subscription when the agent sets no `model`, by harness name, for example `{"pi":"glm-5.3-flash","claude":"glm-5.3"}` for a Z.ai key, or `{"pi":"kimi-for-coding"}` for a Moonshot `coding` key. Only the harnesses where the daemon picks the model: pi (the provider's default from the pi adapter's table; no entry when pi refuses the subscription) and Claude Code on Z.ai. `{}` when every harness keeps its own default. Always present. |

Names follow the project name rules; `login`, `detect`, and
`fetch-models` are reserved.

Probes: `anthropic-oauth-usage`, `openai-codex-usage` (alias
`codex-rate-limits`), `github-copilot-quota` (alias
`copilot-premium-requests`), `zai-usage` (the default of a `zai`
subscription on the `coding` plan that names no probe), `moonshot-balance`
and `moonshot-cn-balance` (the defaults of a `moonshot` subscription on the
`api` or `api-cn` plan that names no probe), `kimi-code-usage` (the default
of a `moonshot` subscription on the `coding` or `coding-cn` plan that names
no probe), and
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
sample's agent has in the `working` column, assigned to itself, at the time the
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
- Create, delete, probe, verify, detect, and the default account:
  `sysadmin` or the operator.
- Login sessions, logout, and API keys: the operator only (PLAN §10,
  §16.1). Any agent gets `403 forbidden`.
- `GET /api/usage`: an agent sees its own project only, unless it is a
  `sysadmin`.
- `PATCH`, enable, disable, stop-all, and `fetch-models`: `sysadmin` or the
  operator. Any other agent gets `403 forbidden`.
- Disabled subscriptions. `POST .../disable` sets `enabled: false`. Then:
  - No agent on it starts, wakes, or restarts. `POST /api/agents/{id}/start`
    and `.../restart` answer `409 conflict` with the reason. A wake (mail,
    a turn, a card) does nothing: the work waits in the store, as for a
    paused agent, and a stopped agent shows the status reason
    `subscription_disabled`.
  - No new agent takes it: `POST /api/agents` with it is `422 invalid`.
  - A swap (§6.2) into it is `422 invalid`, and the auto-swap skips it.
  - An agent that runs goes on: its current and queued turns go out while
    it runs. When it stops (an idle stop, a stop, a crash), it does not
    start again. A crash ends `stopped` with the reason
    `subscription_disabled`, with no restart.
  - `POST .../enable` sets `enabled: true` and wakes each agent on it that
    has queued work (the stream limit applies to their turns).
  - `POST .../stop-all` stops every running agent on it now. It does not
    disable the subscription unless the body says `{"disable": true}`.
- Stream limit. A self-hosted subscription with `max_concurrent` lets at
  most that many of its agents run a turn at the same time
  (`crates/terrariumd/src/stream_limit.rs`). Before the supervisor types
  work into an idle agent (a turn, the recap turn, a mail or card pointer,
  a compaction), the agent takes a free stream. With none free, the work
  waits in the supervisor, and the agent shows `idle` with the status
  reason `stream_limit: waiting for a free stream on `<name>``; the idle
  stop does not stop it. The end of a turn (the agent rests), a stop, a
  crash, and an idle stop free the stream, and the first agent that waits
  gets it (first in, first out across agents). An agent with more work
  goes to the end of the queue. A compaction takes a free stream only
  (it does not queue; the idle pass tries again). A turn that the runner
  reports `working` counts also past the limit (a turn typed in the
  terminal, or a turn that ran across a daemon restart), so the count
  after a restart comes from the turns that run. A turn that never ends
  keeps its stream until the agent stops; there is no stall timer.
- Plan accounts (PLAN §16.3). Each plan login is its own subscription with
  its own account under `<state>/credentials/accounts/<name>/`. The status
  checks, the probes, and the model lists read the account; before the
  migration of a shared login, they read the shared home. None of them
  refreshes a token.
- The default account (PLAN §16.3). Each of Claude Code, Codex, and
  Copilot has one default plan account or none. A daemon start makes the
  oldest account of each harness with no default record its default (the
  `created_at` of `account.json`, else of the subscription). The first
  account that a login makes for a harness becomes its default. A second
  account does not change the default. A delete or a logout of the default
  makes the oldest account left the default (`reason: replaced`).
- Expired logins. A status check (verify, the start-up check) that says
  "logged out" sets `expired` when the subscription had a login
  (`logged_in`, `expiring`, `expired`), else `missing`. A probe answer of
  HTTP 401 also sets `expired`. On each change into `expired`, the daemon
  mails the operator once from `system` (subject `Login expired: <name>`)
  and emits `subscription.expired`. A later successful probe or status
  check sets `logged_in` again.
- Windows that go. A successful probe stores the windows of its answer,
  and removes each stored window of the subscription that the answer does
  not have: its samples and its quota notice rows (§16.6). The rule is the
  same for every probe. So a window that the provider stopped reporting
  (the `5h` window of an Anthropic plan that became Ultra on 2026-10-30)
  leaves the lists and the tiles, and no quota line notifies for it. A
  failed probe removes nothing: the windows of the last success stay.

### 16.3 Endpoints

| Method and path | Body or query | Answer |
|---|---|---|
| `GET /api/subscriptions` | | Array of subscriptions. |
| `POST /api/subscriptions` | `{name, provider, plan?, harnesses?, probe?, alert_at?, selfhosted?}` | `201` with the subscription (`source = api`). `409 conflict` when the name exists; `422 invalid` for an unknown probe, or for provider `zai` with a plan that is not `coding` or `api`. A plan login loses `pi` from `harnesses` (§16.1). `selfhosted` is for provider `selfhosted` only (§16.5). |
| `PATCH /api/subscriptions/{name}` | `{alert_at?, harnesses?, selfhosted?}` | The subscription. `selfhosted` is a part of the server object: a field that is not in the body stays, and `null` clears `default_model`, `context_window`, `max_output_tokens`, `max_concurrent`, or `status_url`. `harnesses` changes here only for a self-hosted subscription; a new `api` style drops the harnesses that it does not serve. The checks of a create apply. `409 file_managed` for a file subscription. Audit `subscription.update`. |
| `POST /api/subscriptions/{name}/enable` | | The subscription, `enabled: true`. Wakes the agents on it that have queued work. Audit `subscription.enabled`, event `subscription.enabled`. |
| `POST /api/subscriptions/{name}/disable` | | The subscription, `enabled: false` (§16.2). Audit and event `subscription.disabled`. A second call changes nothing and writes nothing. |
| `POST /api/subscriptions/{name}/stop-all` | `{disable?}` (the body is optional) | `{subscription, stopped: [{id, name, project, error?}]}`. Stops each running agent on the subscription with the graceful stop of `POST /api/agents/{id}/stop` (then the hard stop after its time limit). With `disable: true` the subscription is disabled first. Only the agents whose current subscription is this one. Audit and event `subscription.stop_all` (`{agents, disabled}`). |
| `POST /api/subscriptions/fetch-models` | `{api?, base_url?, key?, subscription?, refresh?}` | The model list of a self-hosted server (§16.5): `{api, base_url, models: [{id, name?, context?}], cached, fetched_at}`. `503 unavailable` with the reason when the server does not answer with a list. |
| `GET /api/subscriptions/{name}` | | One subscription. |
| `DELETE /api/subscriptions/{name}` | | `204`. `409 file_managed` for a file subscription. The plan account goes with it; `409 conflict` while an agent's current subscription is this one (swap it first, §6.2). |
| `GET /api/subscriptions/{name}/quota` | `?since=` (a time, §2.1; default 24 h ago), `?window=`, `?limit=` (default 1000) | Array of samples, oldest first: `{subscription, window, used_pct, resets_at, raw, at}`. `raw` is the provider's window object. |
| `GET /api/subscriptions/{name}/models` | `?refresh=true` | The models the subscription can use (below). Cached for 6 h per subscription; `refresh` reads the list again. |
| `POST /api/subscriptions/{name}/probe` | | `{subscription, probe_status, error?, windows}`. Here `subscription` is the **name** only, not the object. A failed probe is still `200`. |
| `POST /api/subscriptions/{name}/verify` | | `{subscription, status}`. Here `subscription` is the **full** subscription (§16.1), after the check. `status` is `{kind, harness, provider, pi_provider?, logged_in, plan?, detail?}`. The harness status command runs against the subscription's plan account (PLAN §16.3; before its migration, the shared home) and sets `login_status` and `plan`: the plan when the subscription has none, and, for an `api` subscription of Anthropic, OpenAI, or GitHub, the plan that the check read when it differs (a renamed plan: `Max 20x` becomes `Ultra`). A daemon start does the same for each plan account. A stored API key counts as logged in: `status` is then `{logged_in: true, detail}`. |
| `POST /api/subscriptions/{name}/logout` | | The subscription (§16.1), with `login_status` `missing`. A plan account: removes the account directory and nothing else; no harness command runs, so no other account and no shared file changes. `409 conflict` while an agent's current subscription is this one. A subscription with no account (a shared login before its migration): runs `claude auth logout`, `codex logout`, or `copilot logout` against the shared home. pi has no logout command (it uses API keys only): `422 invalid`. An old pi plan account (from before pi took API keys only) logs out as any plan account: its directory goes. A `zai` subscription has no logout either (`422 invalid`): the Claude logout would end the shared Claude login. |
| `POST /api/subscriptions/{name}/default` | | The subscription (§16.1), with `default: true`. Its plan account becomes the default account of its harness; the old default stops being one, in the same transaction. The audit row is `subscription.default` (`{harness, from, to, reason}`), and a `subscription` event (§16.7) follows. `422 invalid` for an API-key subscription, a subscription with no plan account, and a pi account or a provider that is not the plan provider of the harness. `404 not_found` for an unknown name. |
| `PUT /api/subscriptions/{name}/label` | `{label}` | The subscription (§16.1). `label` is a string (at most 80 characters, no control characters) or `null`; `null` or only spaces removes it, and `display_name` comes from the vendor table again. Text for people only: the name, the ids, and the keys stay. Works on a file subscription too. Needs the operator or sysadmin. The audit row is `subscription.label` (`{label}`). |
| `POST /api/subscriptions/{name}/api-key` | `{provider, key, plan?, base_url?, models?, key_id?, cloud?}` | The subscription, without the key. `plan` is for `zai` and `moonshot` only; `key_id` for `bedrock`, `cloud` for `bedrock`, `gcp`, and `foundry`. See §16.5. |
| `GET /api/subscriptions/detect` | | Logins found in the Claude Code, Codex, and Copilot homes that no subscription covers (same provider, and the harness is in `harnesses`): `[{kind, harness, provider, logged_in, plan?, detail?, suggested}]`. pi uses API keys only: no pi login is found. `suggested` is a subscription definition. |
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

A `selfhosted` subscription answers its stored `models` (and its
`default_model`) with `source` `subscription`, and sends no request to
the server; `POST /api/subscriptions/fetch-models` reads the server on
request. For every other subscription the daemon tries four sources, in
order, and answers with the first list that has a model:

1. `provider`: `GET <base>/models` (the OpenAI list shape) with the stored
   API key as a bearer token, for an API-key subscription of `zai` (the
   plan's base URL: `coding` `https://api.z.ai/api/coding/paas/v4`, `api`
   `https://api.z.ai/api/paas/v4`), `moonshot` (the plan's base URL, see
   §16.5), `openai`, or `openrouter`. The key stays in the daemon.
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
3. `pi`: `pi --offline --list-models <pi provider>`, pi's own catalogue. pi
   uses API keys only: pi runs in an empty pi dir with a placeholder in the
   key env var (`--offline` sends no request). A custom provider of the
   shared `models.json` lists in the shared pi home.
4. `fallback`: the values of `default_models` (§16.1). For `moonshot`,
   also the documented model ids of the plan, after the default.

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

The card usage answer also includes `working_seconds` and active
trouble `signals` (arrays of signal names). The card and board summary expose
the same active names as `trouble`. Working time is replayed from card
assignment, status and wait events: only an agent working on this card in a
working/research column counts. Idle time, open waits and time in other
columns do not count. Usage query date filters apply to token/turn totals,
not the working clock or current marks. To avoid a deploy-time burst, only
cards entering the working-role or research column after the trouble baseline migration
are monitored, including cards assigned by a lead. The
baseline survives daemon restarts. Clock/history replay starts at the first
such entry and reads only explicit assignees' status, wait and reset events,
not the lifetime history of agents who commented on the card. Card and agent
histories use separate indexed sequence ranges after entry. Only one prior
status per assignee is fetched, by an indexed descending seek with LIMIT 1.
The query plans are tested after ANALYZE with 100,000 historical events.
Budget usage
starts at that entry. Interactive token usage totals retain their existing
all-time/date-filtered meaning.

`GET /api/projects/{name}/trouble` reads the project's trouble thresholds.
`PUT` replaces them and requires project admin rights. Omitted fields use
defaults. Unknown fields and negative limits are rejected. Example:

```json
{"time_seconds":{"small":2700,"impl":7200,"impl-hard":14400},
 "tokens":{},"turns":{},"red_streak":3,"red_same_commit":2,
 "review_rounds":2,"eta_delays":3,"timer_polls":3,"compactions":3,
 "review_wait_seconds":3600,"no_progress_seconds":3600}
```

Time, token and turn maps are keyed by the suffix of the card's `type:` label
(untyped cards use `impl`). Token/turn budgets are off until set. Time budgets
use four times the project's type-specific p70 from completed card history
(TER-316), requiring at least five distinct cards. Calibration reuses
TER-394's shared work_samples implementation and existing indexes: it selects
cards from the newest 2000 project moves, then pairs their ETA/move events
with a window function. With too few samples,
they use the fixed fallbacks above (`hard` also uses four hours). Explicit
project time overrides take precedence. A zero threshold disables that check.
An ETA at or before now produces `eta_overdue`. The other signal names are
`time_budget`, `token_budget`, `turn_budget`, `eta_delayed`, `review_rounds`,
`validation_red`, `timer_polls`, `compactions`, `review_wait`, `no_progress`
and `shipped_accepted`. `shipped_accepted` (TER-608) marks an accepted card
with a `version:X.Y.Z` or `version:vX.Y.Z` label (not `version:vnext`) when
a closed version group of the project has the status `Built X.Y.Z` or
`Built vX.Y.Z` (from `group built`). The daemon sees no Git tags, so that
acknowledgement is its record of a published tag. Each pass reads the built
versions once per project from `card_groups`, not from the event log. Only the leads get
its mail. It clears when the card leaves accepted or loses the label.
Review wait starts at the most recent entry into review and clears on return to work.
Assignment changes while the card stays in review do not reset its age.
Job-set ETAs do not count as agent ETA extensions.

Validation counts linked test runs uploaded by the current assignee, or the
bound card agent while unassigned. Each suite name has its own streak. A
green/empty run breaks that suite's streak. The latest 128 linked runs are
examined per card. Three consecutive red runs, or
two red runs on the same nonempty commit within the streak, trigger the mark.
Short timer waits (at most one minute) and compaction requests use a rolling
one-hour window. Review rounds and ETA extensions count the card's history.
For a card in a working-role column with a repo, the remote scan reads the
actual branch head with `git ls-remote`, so direct pushes count. Research
cards are not watched and never get `no_progress`. The branch is the card's
branch, else the lowercase card id (as for validation jobs). The scan has its
own five-minute timer, reads at most 32 cards per pass from a rotating cursor,
and stops a pass after 120 seconds. The cursor moves past a card only after
its read finishes, so a read cut by the cap comes first in the next pass. The
capacity is 32 cards per five minutes: with more watched cards, each card is
read less often, and with slow remotes the cap cuts a pass short. Then a
stored read can grow older than the threshold, and no mark is set for that
card until a fresh read (no false marks). A failed repo, credential or
`ls-remote` read is logged and changes nothing. `no_progress` starts after
`no_progress_seconds` of agent working time (waits excluded) without a new
head: counted from the claim while the branch was never seen on the remote,
otherwise from the working time when the current head was first seen. A new
head clears the mark. No mark is set while the last successful read is older
than the threshold, or when the stored read is for another repo or branch of
the card. A zero threshold disables it. Proxy `repo.push result=forwarded`
audit rows are not push evidence.

The lifecycle tick plans in a read, using lightweight working/review/research
card metadata and marked cards that need clearing. It opens a write only if
signals changed. That write checks each card's event/linked-test revision
and project threshold value, deferring only that card's stale plan. Unrelated
events and audit traffic cannot starve notices. It does not replay history
inside a write.
On the lifecycle tick, each new signal sends one state request to the worker
and one summary to each project lead, never to the operator. The worker-nudge
exception is `eta_overdue`: when the worker's card reminder is enabled
(agent override, otherwise the daemon default), the reminder owns its nudge.
The trouble pass records the signal and mails the lead, but does not send a
second worker mail that could delay the reminder. If reminders are disabled,
the worker still gets the trouble state request. `no_progress` is lead-only.
Other signals are unchanged.
The once-per-card
and signal record survives restarts, resolution and recurrence. A mark clears
when its condition stops holding, its threshold is disabled, or the card
leaves working/research/review or is archived. A recurrence can restore the
mark but cannot send another notice. The existing idle alert is separate.

The CLI calls `GET /api/usage?agent=<me>` for `terrarium usage` and
`GET /api/usage?project=<p>` for `terrarium usage project`. Both work;
`/api/usage/me` and `/api/usage/project` are the richer answers for a later
CLI change.

### 16.4 Login sessions

A login session runs the harness login command in a PTY, as the daemon
user, with no sandbox, in a scratch home (`<temp>/terrarium-login-<id>/home`,
mode 0700), never in the shared harness home:

| `kind` | Command | What the session finds |
|---|---|---|
| `claude` | `claude auth login` | `url` (the OAuth authorize URL). Paste the code with `.../input`. |
| `codex` | `codex login --device-auth` | `url` and `device_code`. |
| `copilot` | `copilot login --device-code` | `url` and `device_code`. |

pi uses API keys only (PLAN §16.3): `kind` `pi` is `422 invalid`, "pi
uses API keys only; use Claude Code for a Claude plan and Codex for a
ChatGPT plan". Store a pi key with §16.5.

```json
{"id":"01M5...","kind":"codex","state":"waiting","url":"https://auth.openai.com/codex/device",
 "device_code":"ABCD-EF12G","started_at":"...","done":false,
 "instructions":"Open the URL, sign in, and enter the device code."}
```

`kind` is exactly `claude`, `codex`, or `copilot`. `provider`, `url`,
`device_code`, `subscription`, `finished_at`, and `error` are absent until
they have a value.

`state`: `starting`, `waiting` (a URL or a code is on screen), `verifying`,
`done` (`subscription` names the definition made or updated), `failed`
(`error`), `cancelled`, or `expired` (after ten minutes, or after a daemon
restart). `done` is `true` for the last four.

When the command exits with code 0, the daemon runs the status command in
the scratch home,
moves only the credential into the plan account of the subscription (PLAN
§16.3), makes or updates the subscription (name, provider, plan from the
status output, the harness, the matching probe), sets `login_status =
logged_in`, and runs the probe once. The scratch home goes with the
session's temp dir.

The subscription name: `name` when the request gives it (**Re-login** on a
card gives it, and replaces the login of that account only). Else the
default name (`anthropic-max-20x`, `openai-pro`, `github-copilot`). When the default subscription has an account already, or
has another provider, the login is a **second account**: `<default>-2`, then
`-3`. No other account and no shared file changes. A
file subscription keeps its definition; only its login state changes.
The session's runner files are deleted at the end. The pasted code is never
stored, logged, or sent back; the audit row `login.input` has its length
only. Audit actions: `login.start`, `login.attach`, `login.input`,
`login.cancel`, `login.end`, `subscription.register`.

At most four sessions are live at once (`409 conflict`).

### 16.5 API keys

`PUT /api/subscriptions/{name}/cloud` replaces the cloud settings of an
API-managed Bedrock, GCP, or Foundry subscription. The body is the complete
cloud settings object (`region`, `models`, `budget_usd`, and the other
provider fields), not a `{cloud: ...}` wrapper. Omitted optional fields are
cleared. The operator-only endpoint checks the settings and returns the
subscription without reading or changing its key file. File-managed and
non-cloud subscriptions return `422 invalid`. Budgets send mail only.

`POST /api/subscriptions/{name}/api-key` stores the key in
`<state>/credentials/subscriptions/<name>.json`, mode `0600` (the
directories are `0700`), and sets `env_var`:

| `provider` | `env_var` |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `selfhosted` (alias `openai-compatible`) | `TERRARIUM_SELFHOSTED_API_KEY`, and `ANTHROPIC_AUTH_TOKEN` for an Anthropic-style server (below) |
| `google`, `gemini` | `GEMINI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `xai` | `XAI_API_KEY` |
| `groq` | `GROQ_API_KEY` |
| `github` | `COPILOT_GITHUB_TOKEN` (a fine-grained PAT with Copilot Requests; the Copilot probe uses it) |
| `zai` (takes `plan`) | `ZAI_API_KEY`, and the Claude Code vars below |
| `moonshot` (takes `plan`; pi only) | `MOONSHOT_API_KEY`, and `KIMI_API_KEY` for a Kimi Code plan |
| `bedrock` (takes `key_id` and `cloud`; Claude Code only) | `AWS_SECRET_ACCESS_KEY` with `key_id`, else `AWS_BEARER_TOKEN_BEDROCK`; the Claude Code vars below |
| `gcp` (takes `cloud`; Claude Code only) | `GOOGLE_APPLICATION_CREDENTIALS` (a file); the Claude Code vars below |
| `foundry` (takes `cloud`; Claude Code only) | `ANTHROPIC_FOUNDRY_API_KEY`; the Claude Code vars below |

A new name makes an `api` subscription (harness `pi`, or `copilot` for
`github`, or `claude` and `pi` for `zai`). No endpoint returns the key. The
audit row `subscription.api_key` has the provider, the plan, the env var,
`base_url`, and `models`. A `plan` with a provider other than `zai` and
`moonshot` is `422 invalid`.

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

**Moonshot AI (Kimi), pi only.** `provider` `moonshot`. The operator
decided that only pi spends a Moonshot key (PLAN §16): Claude Code does
not get the Anthropic-compatible endpoints. The `plan` picks the product
and the region. Moonshot keys work only for their own product and region:

| `plan` | Product | Base URL | pi `--provider` | Default `--model` | Probe |
|---|---|---|---|---|---|
| `coding` (the default) | Kimi Code membership, overseas | `https://api.kimi.ai/coding/v1` | `kimi-coding-intl` | `kimi-for-coding` | `kimi-code-usage` |
| `coding-cn` | Kimi Code membership, China | `https://api.kimi.com/coding/v1` | `kimi-coding` | `kimi-for-coding` | `kimi-code-usage` |
| `api` | Open Platform, pay as you go | `https://api.moonshot.ai/v1` | `moonshotai` | `kimi-k3` | `moonshot-balance` |
| `api-cn` | Open Platform, pay as you go, China | `https://api.moonshot.cn/v1` | `moonshotai-cn` | `kimi-k3` | `moonshot-cn-balance` |

Another plan is `422 invalid`. A new name gets `harnesses = ["pi"]` and
`login_status` `api_key`. `POST /api/subscriptions` and a subscription
file with `provider = "moonshot"` and a harness other than `pi` are
`422 invalid`. A Moonshot key on an existing subscription of another
provider, or of another harness, is `422 invalid`. Verify and logout act
as for Z.ai. The env of an agent on it is `MOONSHOT_API_KEY` (the key),
and also `KIMI_API_KEY` (the same key) for a `coding` or `coding-cn` plan,
because pi's `kimi-coding` reads that var. There is no Anthropic var.
`kimi-coding-intl` is a custom provider in the agent's `models.json`, like
`zai-api`. The other three are pi's built-in providers. pi 1.4.0's
`kimi-coding` catalogue does not have `k3.1-pro` and `k3.1-pro-highspeed`.
The agent's `models.json` adds them to `kimi-coding-intl` and to the
built-in `kimi-coding`, so an agent on a `coding` or `coding-cn` plan can
select them. The default of an Open Platform plan is `kimi-k3` (an
operator decision, 2026-10-29). The K2 series retires at the end of
December 2026.

A window of `moonshot-balance` or `moonshot-cn-balance` is `balance`: 0 %
while the account has money, 100 % at an `available_balance` of 0 or less.
It has no reset time. `raw` has `available_balance`, `voucher_balance`,
`cash_balance`, and `currency` (`USD` or `CNY`). `docs/compat.md`,
"Moonshot", has the sources.

The windows of `kimi-code-usage` come from `GET
https://api.kimi.com/coding/v1/usages` (both Kimi Code plans; verified
live 2026-10-30): `5h`, `monthly`, `monthly-code`, and `7d` on the legacy
plans, each with a reset time. `raw` has `source` (the field of the answer)
and the numbers, and the first window also has `booster_wallet` when the
answer has one. A `401`, `403`, or `404` fails the probe with a hint, for
example that the key is an Open Platform key and not a Kimi Code key
(`sk-kimi-`). `docs/compat.md`, "Kimi Code usage", has the answer.

**Self-hosted model servers.** `provider` `selfhosted`. The operator
runs a model server (vLLM, Ollama, llama.cpp, SGLang, LM Studio, a
proxy) and adds it with `POST /api/subscriptions` and a `selfhosted`
object (or `[selfhosted]` in `subscriptions/<name>.toml`):

| Field | Meaning |
|---|---|
| `api` | `openai` (the default) or `anthropic`: the API style of the server. |
| `base_url` | Needed. `http` or `https`; a LAN host or `localhost` is fine. No user or password in it, no `?` or `#`. The OpenAI style takes the root with `/v1` (`http://gpu:8000/v1`); the Anthropic style takes the root without it (`http://gpu:8000`), as `ANTHROPIC_BASE_URL` does. A trailing `/` goes. |
| `models` | The model ids that the server serves (typed, or picked from "Fetch models"). |
| `default_model` | The model of an agent that sets none. Else the first of `models`. |
| `context_window` | The context window, in tokens (at least 1024). |
| `max_output_tokens` | The most output tokens of one answer (at most `context_window`). |
| `max_concurrent` | How many agents on the subscription may run a turn at the same time, 1 to 1024 (§16.2, "Stream limit"). None: no limit. |
| `status_url` | An `http(s)` status page of the server, for the dashboard link. |

The harnesses that a style serves (verified from the installed harness
versions, `docs/compat.md`, "Self-hosted model servers"):

| `api` | Harnesses | How the harness gets the server |
|---|---|---|
| `openai` | pi, Codex | pi: the custom provider `terrarium-selfhosted` (`api` `openai-completions`) in the agent's `models.json`, `--provider terrarium-selfhosted --model <m>`. Codex: `-c model_provider="terrarium_selfhosted"` and `-c model_providers.terrarium_selfhosted={name, base_url, env_key="TERRARIUM_SELFHOSTED_API_KEY", wire_api="responses"}`, `-m <m>`. Codex 0.166 takes the Responses API only (`wire_api = "chat"` is no longer supported), so the server must serve `/v1/responses`. |
| `anthropic` | Claude Code, pi | Claude Code: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `API_TIMEOUT_MS=3000000`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `..._SONNET_MODEL`, `..._HAIKU_MODEL`, and `CLAUDE_CODE_SUBAGENT_MODEL` set to the model, `--model <m>`. pi: `terrarium-selfhosted` with `api` `anthropic-messages`. |

Copilot CLI cannot use a self-hosted server. No `harnesses` gives all
the harnesses of the style; a harness of the other style, or `copilot`,
is `422 invalid` and names the harnesses that the style serves. An agent
of such a harness on the subscription is `422 invalid` too (create, the
harness or subscription PATCH, a swap). The limits reach the harness
where it has a setting: pi `contextWindow` and `maxTokens` of each model;
Claude Code `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (it compacts before the
window is full) and `CLAUDE_CODE_MAX_OUTPUT_TOKENS`; Codex
`-c model_context_window=<n>` (Codex 0.166 has no output-token setting).

The key is optional: many local servers need none. `POST
/api/subscriptions/{name}/api-key` with `provider` `selfhosted` stores
one; it sets `env_var` `TERRARIUM_SELFHOSTED_API_KEY` and `login_status`
`api_key`. With no key, each harness gets the placeholder
`terrarium-no-key` in the key vars, so no harness sends another
credential (the shared Claude login) to the server. The base URL, the
models, and the limits are no secret: they are in the launch spec. The
key goes only through `terrarium exec-env`, as every stored key does. A
self-hosted key on a subscription of another provider (and the reverse)
is `422 invalid`. A self-hosted subscription has no probe (`no_quota`,
`local_estimate_only`), no plan, and no logout; verify sets `api_key`.

`openai-compatible` is the old provider word. It stays an alias on
input: a create, a file, or an API key with it makes a `selfhosted`
subscription with `api` `openai`. Migration 46 renames the stored rows,
and the next start moves their base URL and models from the key file
into `selfhosted` (audit `subscription.migrate`); their `env_var`
becomes `TERRARIUM_SELFHOSTED_API_KEY`.

"Fetch models" (`POST /api/subscriptions/fetch-models`) is the one place
where the daemon sends a request to a URL that a caller gives. It is a
server-side request to any host the daemon can reach (an SSRF risk), so:
only the operator and sysadmin can call it; `http` and `https` only, with
the base URL rules above; one `GET` of `{base_url}/models` (OpenAI style)
or `{base_url}/v1/models` (Anthropic style, with `anthropic-version`); no
redirect is followed (a `3xx` is an error); 5 s for the whole request;
at most 1 MiB of body; the answer is a list of model ids only. With
`subscription`, the stored server and the stored key are used, and a
`base_url` that is not the stored one is `422 invalid`: the stored key
never goes to another host. A `key` in the body is sent and never
stored. The key goes in `Authorization: Bearer` (and `x-api-key` for the
Anthropic style), and never in an error text. A list is cached for 5
minutes per style and base URL (`refresh: true` reads it again). Audit
`subscription.fetch_models` (`{models, cached}`).

At each launch, `RunnerSupervisor` puts the stored key into the env of
every agent whose `subscription` has `env_var`
(`terrariumd::login_session::credential_env_for`). The key goes on top of
the adapter's env. The start log and `terrariumd sandbox-dry-run` show the
value of each var whose name has `KEY`, `TOKEN`, `SECRET`, or `PASSWORD` as
`<redacted>`.

For Vertex (`gcp`), `test_connection: true` exchanges a short-lived RSA-signed
service-account assertion at Google's fixed OAuth endpoint, then makes a paid
one-output-token Haiku request before writing anything. The wizard always
requests this check. A failed check preserves the old key and settings. The
check accepts only canonical regional, multi-region, or global Vertex hosts,
refuses `cloud.base_url` without a network request, and never follows redirects.
Use the API without `test_connection` to store a gateway configuration.
Authentication errors name the service-account key, and access errors name the
Vertex AI User role (`roles/aiplatform.user`), API enablement, and model access.
Errors never contain upstream response bodies or credentials.

The Vertex key save fills missing model tiers with `claude-opus-5-5`,
`claude-sonnet-4-6`, and `claude-haiku-4-5@20251001`, and a missing region with
`us-east5`. Explicit model mappings override those defaults. `cloud.project_id`
is required. The sandbox receives the JSON as a 0600 file through credential
env, with `GOOGLE_APPLICATION_CREDENTIALS` pointing to the file.

For Bedrock, `test_connection: true` makes a paid one-output-token test of
the haiku inference profile before any write. The wizard always requests
this. On failure the endpoint returns a safe diagnostic and saves nothing.
API callers may omit it to store credentials without making a paid call.
The test only connects to `https://bedrock-runtime.<region>.amazonaws.com`.
It refuses regions outside `^[a-z]{2}(-[a-z]+)+-[0-9]+$` before signing.
When `cloud.base_url` is set, the test is refused without connecting: save
without `test_connection` instead. A gateway URL only reaches the sandbox.
Missing Bedrock region defaults to `us-east-1`; missing model tiers get
the US inference profiles listed in `docs/compat.md`. Explicit entries win.

**Cloud providers (Claude Code only).** `provider` `bedrock` (Amazon
Bedrock), `gcp` (Google Cloud Vertex AI), or `foundry` (Microsoft
Foundry); the code is `crates/terrarium-core/src/cloud.rs`. The
subscription gets `harnesses = ["claude"]` (another harness is `422
invalid`), no plan, no probe, and `login_status` `api_key`. The key file
keeps the secrets:

- `bedrock`: `key` is the secret access key and `key_id` the access key
  ID of an IAM user, or `key` alone is an Amazon Bedrock API key.
- `gcp`: `key` is the service-account JSON (an object with `type`).
- `foundry`: `key` is the API key of the resource.

`cloud` holds the settings, which are no secret. They go to the
definition (`[cloud]` in `subscriptions/<name>.toml`, or `cloud` in `POST
/api/subscriptions`), not to the key file. A key without `cloud` keeps
the stored settings, and a `cloud` without `models` keeps the stored
model map. A file subscription takes its `[cloud]` from the file only
(`422 invalid` with `cloud`). Today the key endpoint is the only API path
that changes `cloud`, so a new budget sends the key again.

| Field | Kinds | Claude Code var |
|---|---|---|
| `region` | `bedrock`, `gcp` | `AWS_REGION`, `CLOUD_ML_REGION` |
| `project_id` (needed) | `gcp` | `ANTHROPIC_VERTEX_PROJECT_ID` |
| `resource` (or `base_url`, one of them needed) | `foundry` | `ANTHROPIC_FOUNDRY_RESOURCE` |
| `base_url` (`https://`) | all | `ANTHROPIC_BEDROCK_BASE_URL`, `ANTHROPIC_VERTEX_BASE_URL`, `ANTHROPIC_FOUNDRY_BASE_URL` |
| `models` | all | `ANTHROPIC_DEFAULT_OPUS_MODEL`, `_SONNET_MODEL`, `_HAIKU_MODEL` for the keys `opus`, `sonnet`, `haiku`; and `--model` |
| `budget_usd` | all | none: the monthly budget (below) |

The env of an agent on it adds the switch (`CLAUDE_CODE_USE_BEDROCK`,
`CLAUDE_CODE_USE_VERTEX`, or `CLAUDE_CODE_USE_FOUNDRY` = `1`) and the key
vars: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, or
`AWS_BEARER_TOKEN_BEDROCK`; `ANTHROPIC_FOUNDRY_API_KEY`. Every value comes
through `terrarium exec-env` (`POST /api/agent/secrets/env`), never the
launch spec. The GCP JSON is in `credential_files` of that answer:
`exec-env` writes it with mode `0600` (the dir `0700`) to
`$HOME/.terrarium/credentials/<agent>/`, a tmpfs in the bwrap sandbox,
and sets `GOOGLE_APPLICATION_CREDENTIALS` to the file. In sandbox mode
`none` (development and tests) the daemon names the dir in
`TERRARIUM_CREDENTIAL_DIR`: `<runtime dir>/credentials/<agent>`, which it
removes when the harness exits. The redaction matcher has
the key, `key_id`, and the `private_key` and `private_key_id` of a GCP
JSON.

`models` maps Terrarium model names to cloud model ids: a Bedrock
inference profile id or ARN, a Vertex model id, a Foundry deployment
name. The keys are lowercase (`opus`, `sonnet`, `haiku`, or a Claude id
such as `claude-opus-5-5`). Claude Code gets `--model` with the map entry
of the agent's `model`, else the model as it is for the aliases `opus`,
`sonnet`, and `haiku` (Claude Code maps them with the
`ANTHROPIC_DEFAULT_*_MODEL` vars or its default for the cloud). Any other
name that is not in the map is refused: `422 invalid` at agent create, a
patch, a file sync, and a template; a start refusal (`NotReady`) at start.
The message names the subscription and the missing `cloud.models.<name>`.

A cloud subscription is never in the automatic account choice: the
auto-swap after a usage limit (§16.6) never moves an agent to one, the
spawn pick takes one only when the template or the agent names it as
`subscription`, never from `subscription_pool`, and the `<template>-alt`
fallback after an eligibility refusal never takes one, also when the alt
template names it. Estimated usage, including the cloud budget window,
ranks otherwise eligible accounts but never refuses a spawn at 80, 100, or
a higher percentage. A swap that names a cloud subscription is allowed
(`POST /api/agents/{id}/subscription`, §6.2, with the rules of a swap).

**The monthly budget.** With `budget_usd`, the daemon keeps the spend of
the UTC month: the sum of the `costUSD` running totals of the Claude Code
sessions on the subscription (`cost-state` of the session file; the list
price of the tokens, not the cloud bill). The subscription shows a
`budget` window with `used_pct`, `resets_at` (the start of the next month),
and `balance` (`total` the budget, `used`, `remaining`, `currency` `USD`).
The quota notices of §16.6 mail the operator at `alert_at` (default 80),
90, and 100 percent. Budgets send mail only, even at 100 percent. Agents
keep their turns. Checks run after each new session total and on each
policy tick. Existing cloud-budget holds end at daemon startup with a
`quota.resumed` event.

**LiteLLM private HTTP.** HTTP names must resolve only to the permitted private
ranges at save and at every connection. The daemon pins each probe request,
disables environment proxies, and follows no redirects. At every harness spawn
and restart it resolves again and passes a checked IP literal in
`ANTHROPIC_BASE_URL`, preserving the port and path. The session keeps that IP.
`NO_PROXY` and `no_proxy` contain the checked IP. A proxy that requires the
original Host name fails closed and needs HTTPS. HTTP keys and prompts are
unencrypted on that private network. Harness-side redirects are outside the
daemon's control and are accepted for this operator-chosen private destination.

**LiteLLM fallback budget.** A `litellm` subscription may set
`selfhosted.budget_usd` to a positive whole USD monthly budget (create or
patch the subscription, `null` clears it on patch). When no `key-spend`
sample is newer than ten minutes, or the latest probe reports
`no spend from LiteLLM`, Terrarium counts the UTC month's Claude Code
session `costUSD` totals instead. With a budget it shows a monthly
`budget` window and mails at `alert_at` (80 by default), 90 and 100 percent.
Without a budget the API returns `fallback_spend_usd` and the dashboard
shows USD without a percent. These are local session estimates, not the
LiteLLM bill. A fresh successful spend probe takes precedence again.
LiteLLM spend never holds agents or rejects a named spawn at a percentage.
Automatic account swaps and spawn pool choices never select LiteLLM.

### 16.6 The quota policy

- A window has three lines: `alert_at` (default 80), 90, and 100 (used
  up). An `alert_at` of 90 or more gives no 90 line. When a probe reads a
  window at or over a line, the daemon sends one mail to the operator
  (from `system`, event actor `system`) and one `quota.alert` event. The
  mail is an open item of the Inbox (§6.9) until the operator acknowledges
  it. Its priority is normal: no banner shows, and the tab does not flash.
- Each line sends one mail in a window period. The table `quota_notices`
  keeps the lines that sent, by `(subscription, window, line)`, so a
  repeated sample or a daemon restart sends nothing. A line can send again
  when the window drops 10 points or more under it, or when the stored
  `resets_at` has passed and the new sample has a later reset (more than
  5 minutes later) or none. One sample past two lines sends one mail, for
  the higher line.
- The line mail gives the percent, the reset (in and at), and the agents
  that spent the most in the window (else the agents on the
  subscription). The used-up mail names them too, and says that the agents
  keep their turns. Each quota mail
  ends with a dashboard link, `Open: <TERRARIUM_PUBLIC_URL>/subscriptions`;
  a budget mail links the project page, `<TERRARIUM_PUBLIC_URL>/p/<project>`.
- A window at 100 percent: the used-up mail and a `quota.alert` with
  `level: exhausted`. That is all: a probe window holds no turn and swaps
  no account, because a probe can show 100 percent while the plan still
  serves turns. A hold of an older version from a probe window (cause
  `quota:<subscription>`) ends at the daemon start, with a
  `quota.resumed` event.
- A usage-limit error holds turns. When the provider refuses a turn with a
  usage limit or a rate limit, the adapter gives a `usage_limit` event
  (§16.7). Only Claude Code has a known signal: the `StopFailure` hook with
  `error` `rate_limit` or `billing_error` (docs/compat.md, "Usage
  limits"). Then:
  - An agent with an account pool moves to the next member of its pool,
    in pool order, that did not hit a limit, is not `expired` or
    `missing`, and has a login of its harness (auto-swap, PLAN §16.3;
    §6.2). `TERRARIUM_AUTO_SWAP=off` turns it off. The daemon keeps the
    accounts that hit a limit in memory, until the reset time or a turn on
    the account that ends with no error.
  - Every other agent gets a hold with the cause
    `limit:<subscription>`, the provider's message as the reason, and the
    reset time when the message gives one. The operator gets one mail for
    each new hold, `Usage limit: <project>/<agent> is blocked`, with a link
    to the agent page.
- A hold: the supervisor's `hold_turns` marks an idle agent `blocked` with
  the reason and the reset time (a working agent finishes its turn first).
  New turns wait: `POST .../send` still answers `202`, and push pointers
  and the recap turn stay pending. The `quota.blocked` event has `{cause,
  until, reason, enforced}`. `enforced: false` means the supervisor cannot
  hold turns (the default of the `Supervisor` trait), so the hold is
  advice only. A hold on an agent that does not run applies when it
  starts.
- A usage-limit hold ends when its reset time passes (checked on each
  probe tick), when a later turn of the agent ends with no error, when the
  agent swaps to another account, or when the operator starts or restarts
  the agent (`POST .../start`, `POST .../restart`). The `quota.resumed`
  event follows, the agent goes back to `idle`, and the waiting turns go
  out in order, then the pending push pointer. Turns that wait when the
  agent stops are dropped.
- Daily project budgets come from the project file:

  ```toml
  [[budget]]
  subscription = "anthropic-max"   # optional: every subscription
  tokens_per_day = 5000000         # input + output tokens, UTC day
  turns_per_day = 200
  alert_at = 80                    # optional, percent
  ```

  A budget has two lines: its `alert_at` and 100. Each line sends one mail
  to the operator and one `quota.alert` in a UTC day. No turns are held.
  Existing holds with cause `budget:project:<p>[/<sub>]` end at daemon
  startup with a `quota.resumed` event. Vendor-refusal holds
  (`limit:<subscription>`) and auto-swap are unchanged.

`POST /api/agents/{id}/status` (`terrarium status set`) is the agent's own
word: `status` is `idle`, `working`, or `blocked`; `note` is up to 500
characters. It is stored as a `self_report` event. It does not change the
status that the runner reports. An agent may report for itself only.

### 16.7 Events

| `kind` | `data` |
|---|---|
| `usage.sample` | `{subscription, harness, model, input_tokens, output_tokens, cache_read, cache_write, messages}`: the new usage of one agent since its last `usage.sample`, at most one each 10 s per agent. `model` is the model of the newest response, as the session file names it (the usage store has the same name); the agent definition's model only when the file names none. |
| `usage.rejected_path` | `{harness, harness_session, path, reason}`: the usage collector refused the session file that a `session_started` hook named. The file must be a regular file (not a symlink) under the agent's own session dir (`harness/claude/projects/`, `harness/codex/sessions/`, `harness/copilot/session-state/`, `harness/pi/sessions/`; with sandbox mode `none` also the session area of the shared home), and another agent must not read it. A Claude Code, Codex, or Copilot file must also carry the session ID in its name (`docs/security-review.md` F12). The collector does not read a refused file. |
| `quota.sample` | `{subscription, windows, removed_windows?}` after each successful probe. `removed_windows` names the stored windows that the answer no longer has: they and their quota notice rows are gone (§16.1). |
| `quota.alert` | `{subscription, window, used_pct, resets_at, level, alert_at, threshold, message}`: `level` is `alert` under 100 and `exhausted` at 100; `threshold` is the line; `message` is the ID of the mail to the operator. For a budget `{project, subscription, window, used, limit, used_pct, level}`. The dashboard shows no banner for it. |
| `quota.blocked` | `{cause, until, reason, enforced}`. The `agent` is the held agent. |
| `quota.resumed` | `{cause, holds_left}`. |
| `subscription.offer` | `{login, suggested}`: a login found at daemon start that no subscription covers. |
| `subscription.expired` | `{subscription, from, reason}`: the login changed into `expired`. |
| `subscription.enabled`, `subscription.disabled` | `{subscription, enabled}`: the operator or sysadmin enabled or disabled the subscription (§16.2). |
| `subscription.stop_all` | `{subscription, agents, disabled}`: a stop-all; `agents` is the `stopped` list of the answer. |
| `subscription` | `{change, harness, from, to, reason?}`. `change` is `default`: the default account of `harness` is now `to` (`reason` is `set` for the API, `backfill` for the oldest account at a start, `first_account` for the first login of the harness, `replaced` when the oldest account left takes over from a deleted or logged-out default). Or `default_cleared`: the default `from` was deleted or logged out; a `default` event with `replaced` follows when an account is left. The audit row of `default` is `subscription.default`. |
| `credentials.migrated` | `{harness, moved, backups, skipped, pi_link}`: the start-up migration moved a shared login into an account (PLAN §16.3). The audit row is `credentials.migrate`. |
| `agent.swapped` | `{from, to, applied, auto, reason}`: an agent moved to another account (§6.2). The audit row is `agent.swap`. |
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
| `GET /api/hosts/{name}/metrics?minutes=N` | | `200` the metrics history (below) | any caller |
| `POST /api/hosts` | `{"name", "enrol"?}` | `201` one flat host object (the shape below) with `token`, `token_kind`, and for an enrol token `token_expires_at` and `install_command` (§17.9) | the operator |
| `POST /api/hosts/{name}/rotate` | none, or `{"enrol"?}` | `200` the same shape as `POST /api/hosts` | the operator |
| `DELETE /api/hosts/{name}` | | `204` | the operator |

```json
{"name":"lab1","id":"01JB...","host_id":"01JB...","created_by":"operator",
 "status":"online","online":true,"sessions":1,
 "capabilities":{"docker":{"present":true,"version":"29.8.0"},"vmlab":{"present":false},"shell":"/bin/bash",
   "limits":{"systemd":true,"controllers":["cpu","cpuset","io","memory","pids"],"docker":true}},
 "summary":{"os":"linux","arch":"x86_64","docker":true,"vmlab":false},
 "os":"linux","arch":"x86_64","version":"0.1.0+6e31754","stale":false,"max_sessions":32,
 "connected_at":"2026-10-24T06:36:54.149Z","last_seen":"2026-10-24T06:36:54.149Z",
 "created_at":"2026-10-24T06:30:00.000Z",
 "first_used_at":"2026-10-24T06:31:02.410Z","last_used_at":"2026-10-24T06:36:54.149Z",
 "last_used_from":"10.0.0.7:51544","rotated_at":null,"enrol_expires_at":null,
 "metrics":{"at":"2026-10-24T06:36:50.002Z","stale":false,"interval_secs":10.0,
   "cpu":{"busy_pct":12.4,"cores":16,"load1":1.32},
   "memory":{"total_bytes":100923486208,"used_bytes":24047804416,"used_pct":23.8,
             "swap_total_bytes":100923338752,"swap_used_bytes":9918578688},
   "net":{"rx_bytes_per_sec":18234.5,"tx_bytes_per_sec":4021.0},
   "disk_io":{"read_bytes_per_sec":0.0,"write_bytes_per_sec":1294336.0},
   "filesystems":[{"mount":"/","fstype":"btrfs","device":"/dev/sda2","total_bytes":1998694907904,
     "used_bytes":1201450532864,"avail_bytes":795540078592,"used_pct":60.2}]}}
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
- `version` is the build version of the host agent from its last `hello`
  (`0.1.0+6e31754`, §4 `GET /api/system/version`). `stale` is `true` when
  that build is from another commit or crate version than the daemon's
  (the `-dirty` mark does not count), or when the host agent sends no ref
  (an older build). `stale` is `false` when the daemon does not know its
  own ref, or before the first `hello`. The daemon offers a stale host
  agent its own build, and the host agent replaces itself
  ([host-protocol.md, "Self-update"](host-protocol.md#self-update)). The
  event `host.updated` and the audit entry `host.update` record the
  update; `host.update_failed` (event and audit) records a new binary that
  did not start. An agent from before self-update, or one with
  `auto_update = false`, stays stale: reinstall it (§17.9).
- `first_used_at` is the first connect with the current token (a rotate
  clears it). `last_used_at` and `last_used_from` are the last connect and
  its TCP peer as the daemon sees it (a reverse proxy shows as itself).
  `host.connected` carries the peer as `from`.
- `status`, `online`, and `sessions` come from the live link.
  `capabilities` comes from the last `hello`, `{}` before the first one.
- `offline_reason` is `null` for an online host. For an offline host, it is
  a text that tells why, in this order:
  - The last link error since the daemon started, with its time: a refused
    `hello` (`refused: ...`), a spent enrol token, or the reason of a
    disconnect (`disconnected: heartbeat timeout`). A rotate after the
    error hides it. A restart of the daemon loses it.
  - An open enrol token (`waiting for the host agent to dial in ...`), or
    an enrol token that ran out before its use.
  - `never connected: ...`, when the host has no `last_used_at` and no
    `last_seen`. When the host listener is plaintext on a loopback address,
    the text tells you that a published container port cannot reach it.
  - `the token was rotated at ...`, when no link used the current token.
  - `not connected since <last_seen>`.
  A link that does not reach the daemon (a refused or reset TCP
  connection) leaves no error. Look at `journalctl -u terrarium-host` on
  the host.
- `DELETE` revokes the token. A live link gets `goodbye token_revoked` and
  closes. Its sessions end with the error `lost`.
- A second link with the same token replaces the first. The first gets
  `goodbye replaced`.
- `hello.host` must be the name of the token's host. Else the daemon sends
  `goodbye protocol`.
- Each link writes the events `host.connected` and `host.disconnected`
  (§9).

Host metrics:

- A host agent with `capabilities.metrics` sends a `metrics` frame every
  10 seconds ([host-protocol.md "`metrics`"](host-protocol.md#metrics)):
  CPU, memory, network IO, disk IO, and disk space. The filter rules for
  interfaces, disks, and filesystems are there. An older host agent has no
  `capabilities.metrics` and sends no samples. Install the new host agent
  on the host to get them (§17.9).
- `metrics` in a host row is the latest sample, or `null` when the daemon
  has no sample of the host. `at` is the time the daemon got the sample.
  `stale` is true when the sample is older than 30 seconds, or when the
  host is offline. The rates are bytes per second over `interval_secs`.
  A part that the host agent cannot read (`cpu`, `memory`, `net`,
  `disk_io`) is absent.
- The daemon keeps the last 360 samples (1 hour) of each host in memory
  only. A restart of the daemon loses them. Samples stay while the host is
  offline. A revoke removes them.
- `GET /api/hosts/{name}/metrics?minutes=N` gives the samples of the last
  `N` minutes, oldest first. `N` is 1 to 60, default 60. Another value is
  `400`. An unknown host is `404`. A host without samples gives
  `"samples": []` and `"stale": true`. There is no event for a sample: the
  dashboard polls every 10 seconds.

```json
{"host":"lab1","interval_secs":10,"minutes":15,"stale":false,
 "samples":[{"at":"2026-10-24T06:36:40.001Z","interval_secs":10.0,"cpu":{"busy_pct":9.8,"cores":16,"load1":1.2},
   "memory":{...},"net":{...},"disk_io":{...},"filesystems":[...]}]}
```

### 17.2 Targets

A `host` target may name a Windows logon account and a daemon-side `login`
secret (the secret's username is ignored):

```toml
[windows_run_as]
account = 'DOMAIN\NAME' # or a local NAME
password_secret = "windows-login"
```

Both fields are required together. Account names have 1 to 256 characters,
no control characters, and either one nonempty name or two nonempty parts
separated by a backslash. Other target kinds refuse this table. At each
session start the connected host must report Windows and the
`windows_run_as` capability. An absent capability fails closed, including
on older hosts. Only then does the daemon open the secret, using the target's
normal project/global secret grant rules. Missing, ungranted, or non-login
secrets refuse the session. Neither API target responses nor audit records
contain the password. The host receives it only in that session's `exec`
frame (or `cp_push`/`cp_pull` for a transfer). This protocol support does not
implement Windows logon itself.

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
| `work_root` | Optional absolute parent of card-agent build roots. Default `/src/terrarium`, giving `<work_root>/<project>/<agent-name>`. Without `work_root`, when the agent user cannot create `/src/terrarium` (permission denied or a read-only file system), the root falls back to `~/.terrarium-work/<project>/<agent-name>` of that user, with one daemon warning per target; the root keeps that base until cleanup. The run spool of other callers (`<work_root>/.terrarium-runs`, below) falls back the same way, to `~/.terrarium-work/.terrarium-runs`, and a run keeps the spool it started in. A `work_root` that is set never falls back: its failure names it and `--work-root`. Set it with `terrarium admin target edit <t> --work-root <path>`. Keep project shared caches outside each agent root. |
| `scope` | `global` (no `project`) or `project`. Every target view has it, also in `GET /api/projects/{name}/targets`. |
| `state` | `running`, `stopped`, `absent`, or `unknown`: what the last lifecycle verb saw. |
| `snapshots` | The snapshot names that `POST .../snapshot` or `snapshot_before_task` took. A destroy (the state `absent`) clears them. A snapshot delete removes a name. A snapshot list removes the names that the host no longer has (§17.3, "Snapshots"). |
| `host_online` | The link of the target's host agent. `null` for `ssh`. |
| `usable` | The caller may use the target (§17.4). |
| `access` | Why the caller may use it: `operator`, `policy`, `lease`, or `null`. |
| `lease` | The caller's active lease on the target (§17.7), when `access` is `lease`; else `null`. |
| `has_display` | The target has a screen (§17.10): `vnc`, `rdp`, `desktop`, `vmlab` with a `machine` (or a lab instance kind), and a `docker` target with `display`. |
| `runtime` | What runs now (§17.8): `attached` (the agent workspaces, `<project>/<agent>`, in the container or the lab instance), `lab` and `head` (the instance dir on the host and the commit of a `lab_repo` target), `instances` (one for each holder of a `lab_from_workspace` target, by the agent ULID: `{agent_name, lab, state, attached}`). `{}` when nothing runs. |
| `credential_info` | Only with a `credential`: `{name, scope, kind, fingerprint?, warning?}`. `scope` and `kind` are those of the secret that the daemon uses (`null` when there is none). `fingerprint` is the `SHA256:` fingerprint of an `ssh_key`. `warning` tells why the credential does not work (below), also a global secret with no grant to the project (§5.2). Never a value. |
| `limits_info` | Only with `limits` ("Resource limits", below): `{summary, per, problem?}`. `summary` is the short text, for example `2 CPU · 4G`. `per` is `holder` (a `host` target: each holder has its own budget) or `container` (a `docker` target: one budget). `problem` is the refusal that a session or an `up` gets now, because the online host agent cannot apply the limits. |

Card agents record owned roots before target checkouts or runs create them.
Checkouts must use absolute paths. During transition, work outside the owned
root remains supported with one warning per agent and target and is never
automatically deleted. Runs using the root default `CARGO_TARGET_DIR` and
`RESULTS_DIR` to its `target` and `results` subdirectories. Shell commands
must also keep their own output paths there. Target preparation and cleanup
require Linux with procfs and Python 3.9 or newer on the target. Cleanup uses
directory-FD operations available before Python 3.11, not `rmtree(dir_fd=...)`.

After an agent is removed and its exec sessions end, the daemon sweeps its
recorded roots within 30 seconds. Failures remain recorded for retry across
daemon restarts. Cleanup refuses changed machine/path identity (kind, host,
address, port, container, username, work root), foreign owner
markers, and symlink ancestors or roots. Nested symlinks are unlinked without
following them. Mount points, including same-device bind mounts, are refused
before deleting anything. Unrelated edits such as limits, credentials and
`keep` do not block live work or cleanup. Each cleanup starts with an
`agent.target_cleanup_started` audit containing the pre-deletion allocated
size, so even a timed-out or partially completed attempt has an audit. A new
refusal writes `agent.target_cleanup_refused` and remains available for retry.
Deletion has a 15-minute timeout. Each successful cleanup writes an `agent.target_cleanup`
audit record with the agent, target, path and allocated size in bytes. Old
untracked directories and shared caches are never swept.

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
| `POST /api/targets/{name}/test` | | `200` a test report (below) | as a lifecycle verb (§17.3) |
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
| `host` | `host` | `limits` |
| `ssh` | `address` | `port`, `credential` (an `ssh_key` secret, §5.2) |
| `docker` | `host`, and `image` or `container` | `ephemeral`, `keep`, `display` (`x11`, `wayland`, `auto`), `display_name` (§17.10), `limits` (with `image`) |
| `vmlab` | `host`, and one of: `lab` (an absolute path); `lab_repo` with `machine`; `lab_from_workspace = true` with `machine` | `machine`, `lab_ref`, `lab_path`, `ephemeral`, `keep`, `snapshot_before_task` |
| `vnc` | `host` (the host agent that connects), `address` (`host:port`, port default 5900) | `username`, `credential` (a `login` secret, §5.2) |
| `rdp` | `host`, `address` (port default 3389) | `username`, `domain`, `credential` (a `login` secret) |
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
sends this `PATCH`. Exception: the `limits` of a `host` target change at
any time, and supported Docker `limits` change live after host acknowledgement
("Resource limits", below).

For a permanent managed Docker target, changing `image` sets
`runtime.recreate_required` and returns `recreate_note`. Supported limit edits
on running or stopped containers use `docker update` without recreation.
Only a successful host acknowledgement advances the saved limits baseline.
A failed acknowledgement keeps both the stored defaults and baseline unchanged.
An offline or older host refuses the live edit. The next `up` of
an existing stopped container starts it with its old settings and a clear
`recreate_note` warning. Editing both image and limits back to the container's
original settings clears the pending flag, without discarding any data.
When no container exists, an edit only stores the definition, without marking
recreation pending. Use normal `up` to create it with the holder workspaces.
`recreate` refuses an absent container with `no container yet; up creates it`.
For a legacy container without a saved baseline, the first edit reads its
actual image and limits from Docker inspect rather than the definition.
If the host is offline, that edit refuses until the baseline can be read.
Run `terrarium admin target verb NAME recreate` (`POST
/api/targets/{name}/recreate`) explicitly to apply the settings. This requires
the rights to define the target (project admin or global sysadmin), refuses
running containers and live exec sessions, and removes the stopped container
without force or volume deletion before starting the new image. **The old
writable layer is lost.** Recreation preserves exactly the Terrarium workspace
binds recorded for the container and verified against the host's work root.
Foreign binds and named or anonymous volumes refuse recreation. Older host
agents that cannot verify mounts also refuse mounted containers. Existing-container targets (`container`)
cannot be recreated. Ephemeral targets keep their existing behavior.

`targets ls` marks `RECREATE` as `pending` when settings need recreation,
and `targets show` prints the pending state and writable-layer warning.
Only the operator may override recreation safeguards:

- `--force` (`{"force":true}`) permits removal of a running container, including
  other holders' live exec sessions. Those processes are stopped by Docker.
- Repeat `--discard-volumes NAME` (`{"discard_volumes":["NAME",...]}`) for each
  volume attachment that may be lost. The names must equal the full set of
  volume mounts not named by the definition, without missing, extra or duplicate
  names. Currently the definition declares workspace binds, not volumes, so this
  covers every volume mount. A refusal lists volume names and mount points.

Neither override authorizes a foreign bind, unknown mount metadata, or a mount
source in the request. Agents, including project admins, receive 403 for either
override. Recreation rechecks the inspected identity and mounts before removal,
and audits removal with the discarded volume names before starting the
replacement. A second audit records the replacement's success or failure,
so failed image pulls do not hide a completed discard. Link keys cannot use
either override, even though they otherwise act on behalf of the operator.
**Discard means detach, not delete**:
no volume contents are inspected and no volumes are deleted. Old volume data
remains on the host, but the replacement container does not attach it.

An `ephemeral` target must be `docker` (with `image`) or `vmlab`. Values
that go onto a command line (`host`, `address`, `image`, `container`,
`machine`, `credential`, `username`, `domain`) must not be empty, must not
hold spaces, and must not start with `-`. A bad value is `422 invalid`.

File sync, at start and on `POST /api/reload`: a file target is made or
updated and becomes `file`. An API target with the same name becomes
`file`. When the file is gone, the target is deleted. The API refuses to
change a file target (`409 file_managed`).

**Host reservations (phase 1).** Hosts advertise physical logical CPU IDs,
memory bytes and reservation support. `GET /api/hosts/{host}/capacity`
returns physical, reservable and charged capacity. An operator or sysadmin
configures `PUT /api/hosts/{host}/capacity` with `percentage` (0–100) and
an optional `io` map. The default percentage is zero. IO is an operator-set
budget, not measured disk throughput. Map keys are canonical block-device
numbers such as `8:0`; values contain `read_bps`, `write_bps`, `read_iops`
and `write_iops` (nonnegative integers).

The `used` object includes charged `cpu_millis`, `memory_bytes`, the summed
per-device `io` map, and sorted `exclusive_cores` CPU IDs. Queued and terminal
reservations are excluded. Every actor reads the capacity, and the totals
count every project. A `used.gpus` entry shows `reservation` and `holder`
only to an actor that reads the project of the reservation; other actors
see the `claim` only. Admission locks are per host agent. Docker lifecycle
verbs publish an in-progress intent, then release admission while pulling or
creating a container. Verbs may overlap on the same host. Resource changes on
that host return `409 resource_busy` until all verbs finish, except a grant
with `queue: true`, which enters the FIFO. Metadata-only target patches remain
allowed. Expiry cleanup retries afterward. Commands wait up to 500 ms for a
short admission write before reporting `resource_busy`. Other hosts remain independent.

`POST /api/hosts/{host}/reservations` accepts `target`, optional `agent`,
`amounts`, `until`, optional `reason`, `exclusive`, `queue` and
`queue_timeout`. `amounts` contains `cpu_millis` (1000 is one logical core),
`memory_bytes`, and `io`. `until` uses the lease time syntax. Without
`agent`, the reservation belongs to the target. Authorization follows target
leasing. Docker has one container budget and cannot reserve separately for
an agent. Agent host reservations span that agent's targets on the host.
Host grants return 409 naming existing sessions that must end first:
phase 1 does not reparent live scopes. Agent and target reservations cannot
overlap on a host because they use different slice hierarchies.

**GPU reservations.** The same POST accepts an optional `gpus` array. Each
claim names a stable NVIDIA `uuid`, not a device index. For example:

```json
{"target":"p-gpu","gpus":[{"uuid":"GPU-abcd1234","exclusive":true}],"until":"30m"}
```

For cooperative sharing, use `{"uuid":"GPU-abcd1234","vram_bytes":1073741824}`.
`exclusive` defaults to false. A shared claim requires positive `vram_bytes`
no larger than physical VRAM. `amounts` may be omitted for GPU-only requests.
The top-level `exclusive` remains the CPU affinity option, independent of each
GPU claim's `exclusive`. The CLI accepts the array through `reserve new --gpus
'[...]'`. No MIG partitions are created.

Exclusive GPUs require a managed **ephemeral Docker target**, or a native
`host` target on a host agent whose `capabilities.native_gpu.enforced` is
true (the root helper of docs/native-gpu-helper.md, TER-345). On any other
native host the request is a `409` that gives the helper's reason. SSH and
vmlab exclusive GPU requests are refused, never granted as unenforced
exclusive access. On a host with native enforcement, every command of a
`host` target runs in a device policy with only its reservation's GPUs,
and a command with no GPU reservation gets no GPU. When the host agent
stops reporting enforcement while a native exclusive GPU reservation is
managed on the host, every command of a `host` target on that host is a
`409` that names the reservation and the reason, until enforcement is
back or the reservation ended. Shared claims are supported for native host targets and
managed ephemeral Docker targets. Docker reservations belong to the target,
not individual agents. SSH and vmlab GPU reservations are unsupported.

**Containers get no GPU by default**, including images that set
`NVIDIA_VISIBLE_DEVICES=all`. Managed container creation overrides that variable
with `void` without a GPU reservation, or the reserved UUID allowlist with one,
and passes the same devices through Docker's `--gpus` option. A GPU reservation
cannot change a running container's exposure. Stop it first and recreate it
on the next `up`. Permanent and unmanaged Docker targets cannot reserve GPUs.
An exclusive grant is refused while incompatible non-native sessions or
containers are active. Active holders of disjoint devices can run concurrently.
Native host execution remains available and does not generate GPU-session mail.

A zero host reservation percentage disables GPU admission. A positive
percentage enables whole-device exclusive allocations and shared accounting
against physical VRAM, without fractionally partitioning devices. Exclusive
and shared claims cannot overlap on the same device. Shared claims cannot
overcommit its recorded VRAM. The usual FIFO and cleanup accounting apply.
Capacity responses include `gpus` inventory (`uuid`, `name`, `memory_bytes`,
`used_bytes`) and `used.gpus` entries with `reservation`, `holder`, and `claim`.

Shared GPU memory limits are cooperative, not hard limits. The host samples
NVIDIA compute-process VRAM and attributes it through PID cgroups to the
container or native holder's systemd scopes. Overages send warning mail to
the holder and project leads, once until a below-share sample rearms the
warning. Overages never kill processes. Graphics-process usage and missing
PID attribution are not inferred from whole-device totals. GPU samples are
cached for 30 seconds. Inventory is refreshed for every host connection;
missing reserved devices keep reconciliation closed. Ordinary reservation
expiry and explicit retraction still perform their normal session cleanup.

Insufficient capacity returns 409 with the current allocations. With
`queue: true`, the request waits in a durable FIFO queue, without backfill.
`queue_timeout` defaults to one hour. Starting and ending reservations keep
capacity charged until enforcement or cleanup finishes. `GET
/api/hosts/{host}/reservations` lists visible reservations; `GET
/api/reservations/{id}` reads one. `PATCH /api/reservations/{id}` with
`until` extends it, `DELETE` retracts it, and `POST
/api/reservations/{id}/cancel` cancels a queued request. CLI equivalents are
`reserve new`, `reserve extend`, `reserve retract`, `reserve ls` and
`reserve cancel`. `wait reservation <id>` registers a durable wait: admission
resolves it, and cancellation, expiry or queue timeout fails it. Resolution
is mailed to the waiting agent.

Warnings at 15 and 5 minutes remaining are audited and sent as mail and
runner pointers. Ending a reservation ends registered sessions with
`reservation_ended`. Host cleanup stops its own slice before releasing
capacity. Docker cleanup waits a bounded interval for process-tree cleanup;
an unconfirmed result emits a warning and audit record naming the sessions.
Fixed supported limits are restored before capacity is released.
Docker treats zero CPU/memory and empty affinity updates as no change.
For an absent fixed default, release therefore sets the effective
"unlimited" managed default explicitly: all host CPU IDs, a CPU quota equal
to the host's logical CPU count, and physical host memory. Swap follows the
target's configured allowance. These values come from the online host's
capability report at release, not a cached reservation size. A missing or
stale report leaves capacity charged and emits a cleanup warning rather
than guessing. This default does not give a container memory beyond its
host's physical capacity.

Exclusive CPU reservations allocate disjoint whole logical CPUs and leave
at least one CPU for shared work. Isolation covers Terrarium-managed host
scopes and Docker containers, not unrelated host services. Shared scopes
inherit the complement on `terrarium.slice`; exclusive scopes use a sibling
`terrariumreserved` hierarchy. Docker uses cpusets. Existing host sessions
must end before an exclusive grant. Vmlab, SSH and unmanaged Docker paths
are refused while exclusivity is active. Nonexclusive reservations provide
accounted CPU quotas and memory/IO caps, not dedicated CPU affinity.

**Reservation device IO restrictions.** A Docker reservation's per-device
bandwidth and IOPS limits are create-time settings. A live change returns
`409 conflict`, explaining that the limit takes effect at the next up.
Phase 1 accepts device-IO reservations only on ephemeral Docker targets
created for the reservation. Permanent Docker targets return `409 conflict`:
the limits cannot be cleared without recreating the container. Terrarium
does not recreate a container or write its cgroup files to bypass this rule.
At expiry, permanent targets stay running. If restoring their supported
fixed limits fails, the reservation stays in cleanup and its capacity stays
charged until restoration succeeds. Ephemeral reservation containers may
be stopped during cleanup.

**Resource limits.** `limits` keeps one agent from taking all of a
machine. A `host` target and a `docker` target with `image` take it. Every
field is optional:

| Field | Value | `host` target (systemd) | `docker` target |
|---|---|---|---|
| `cpu` | Cores, from `0.01` to `1024`, with at most two decimals: `2`, `0.5` | `CPUQuota=200%` | `--cpus=2` |
| `memory` | Bytes, or a size with `K`, `M`, `G`, `T` (powers of 1024; `B` and `iB` may follow): `"4G"`. At least `16M` | `MemoryMax`, and `MemoryHigh` at 90 % | `--memory` |
| `swap` | A size: the swap on top of `memory`. Needs `memory`. Default: no swap | `MemorySwapMax` (`0` without `swap`) | `--memory-swap` = `memory` + `swap` |
| `pids` | The most processes and threads, 1 to 4194304 | `TasksMax` | `--pids-limit` |
| `io_weight` | 1 to 10000; the kernel default is 100 | `IOWeight` | `--blkio-weight`, 10 to 1000, with the inverse of the runc formula: `10 + (w - 1) × 990 / 9999` |

```json
{"name":"box","kind":"host","host":"lab1","limits":{"cpu":2,"memory":"4G","pids":512}}
```

Rules:

- A bad value is `422 invalid`, and the message names the field, for
  example ``bad `limits.memory` "4X": use a number with K, M, G, or T``. An
  unknown field in `limits` is `400 bad_request`. `limits` on a `vmlab`
  target (the lab file sizes each VM), an `ssh` target (the daemon cannot
  apply it on the remote machine: use a host agent there), a display-only
  target, or a docker target with only `container` is `422 invalid`.
  `io_read` and `io_write` (bandwidth for one device) do not exist yet.
- **A `host` target: a budget for each holder.** Each holder (each agent,
  and the operator) has its own budget on the target. All its exec
  sessions, shells, checkouts, and `terrarium run` jobs on the target share
  it, so one agent cannot starve another. The host agent runs each such
  command in a systemd scope of the user manager (`systemd-run --user
  --scope`), in the slice of the holder: `terrarium-<target>-<project>-
  <agent>.slice`, or `terrarium-<target>-_operator.slice`. Each part has the
  escapes of `systemd-escape`, so systemd nests the slice under
  `terrarium-<target>.slice` and `terrarium.slice`
  ([host-protocol.md "Resource limits"](host-protocol.md#resource-limits)).
  A `cp` on a host target runs in the host agent itself and has no limits.
- `MemoryHigh` at 90 % of `memory` makes the kernel slow the holder down
  before the OOM killer acts. Live on systemd 261: two sessions of one
  holder that asked for 70 MiB each in a slice of 100 MiB never went past
  96 MiB together. The second one waited until the first freed its memory
  (about 3 s). When the first held its memory, the OOM killer ended one of
  them after about 28 s of slow progress. Without `MemoryHigh` it acted at
  once.
- **A `docker` target: one budget for the container.** The container is
  one for all holders (§17.8), so they share the limits. Docker sets them
  when Terrarium runs the container (`up`). API `PATCH` updates supported
  fixed limits through `docker update` on running or stopped containers,
  without recreation. Defaults are saved only after acknowledgement. An
  active reservation continues to override edited defaults until it ends.
  An offline or older host without live-update support refuses the edit.
  An ephemeral target gets a new container at each `up`.
- **A `host` target is always `running`** while its host agent is online.
  Its `limits` change at any time with `PATCH`. A new session sets the new
  limits on the holder's slice, so the processes of the holder that run
  already get them too.
- `PATCH` merges `limits`: `{"limits": {"pids": 64}}` keeps `cpu` and
  `memory`; `{"limits": {"cpu": null}}` removes `cpu`; `{"limits": null}`
  removes every limit. A `limits` with no field left is no `limits`.
- **The host agent must be able to apply them.** The host agent reports
  `capabilities.limits` in `hello` (§17.1): `systemd` (`systemd-run --user`
  works), `controllers` (the cgroup controllers of the user manager),
  `reason`, and `docker`. An exec, shell, `terrarium run`, or checkout on a
  `host` target with limits is `409 conflict` when the host agent cannot
  apply them, with the message `this host agent cannot apply resource
  limits: <reason>`. So is an `up` of a docker target with limits. The
  reasons: the host agent is older than this feature (install the new
  host agent, §17.9); `systemd-run --user` does not work (the message has
  its error); or the user manager has no controller that a limit needs
  (`cpu` for `cpu`, `memory` for `memory` and `swap`, `pids` for `pids`, `io`
  for `io_weight`). The daemon never runs the command without its limits.
  The host agent checks again before it starts the command. A target with no
  limits works on every host agent.

**The credential of a target** (PLAN §16b.6a). `credential` names a
secret (§5.2). The daemon checks it when it saves the target. It does not
wait for the first connection.

| `kind` | `credential` must name |
|---|---|
| `ssh` | an `ssh_key` secret |
| `vnc`, `rdp` | a `login` secret |

A project target uses a secret of its project, or a global secret when
its project has no secret of that name. A global target uses a global
secret only. A missing secret, a secret of another project, and a secret
of another kind are `422 invalid`. The message names the secret and the
rule. `POST /api/targets`, `PATCH /api/targets/{name}`, and a fleet apply
(§18.1) do this check. A `PATCH` that keeps the `kind`, the `project`, and
the `credential` does not do it, so an old target with a bad credential
can still take other changes.

A project target that names a global secret needs a grant of it to the
project, unless the operator or a sysadmin saves the target (§5.2, "A
global secret on a project object"). Else the save is `403 forbidden`,
and the message gives the fix: `terrarium secret grant <name>
project:<project>`. This check runs on each save by a project_admin, also
on a `PATCH` that keeps the credential. A `PATCH` of `keep` or
`snapshot_before_task` alone keeps the mark of an admin. At each use (an
exec, a display login, a test), the daemon checks again. A refused use
is `403` and a `secret.refused` event, and the view shows
`credential_info.warning`.

A target file is never refused for its credential. The daemon loads it,
logs a warning, and adds `targets/<name>.toml: warning: ...` to the
`errors` of the reload. So an upgrade does not drop an old target whose
credential breaks the rule. The view of each target with a bad
credential shows `credential_info.warning`. `terrarium targets show`
prints it, and `terrarium targets ls` marks the KEY cell with `(!)`.

**Test a target.** `POST /api/targets/{name}/test` logs in to an `ssh`
target one time with its key and runs `true`. The caller needs the rights
of a lifecycle verb: it may use the target, or it may define it. The
daemon runs `ssh` with the same options as an exec: `BatchMode=yes`,
`IdentitiesOnly=yes`, the key in a 0600 file of one run, and the
daemon's own known hosts file `<state>/ssh/known_hosts`. The connect
timeout is 10 seconds, and the test stops after 20 seconds.

The host key policy is the policy of each exec: trust on first use
(`StrictHostKeyChecking=accept-new`). At the first connection, ssh
records the host key, and the report says `"host_key": "added"`. So the
daemon never takes a new key silently. Check the host after a first
test. A changed host key is refused: remove the old line with
`ssh-keygen -R <host> -f <state>/ssh/known_hosts`.

| `result` | Meaning |
|---|---|
| `ok` | The login works (ssh exit 0). |
| `auth_refused` | `Permission denied`: add the public key to `~/.ssh/authorized_keys` of the user. |
| `host_key_mismatch` | The host key changed since the first connection. |
| `host_key_unknown` | The host key is not known, and ssh did not record it. |
| `unreachable` | `Connection refused`, a timeout, no route, or no DNS name. |
| `error` | Another failure; read `stderr`. |

```
POST /api/targets/box/test

200
{"target":"box","kind":"ssh","address":"build@box","result":"auth_refused",
 "message":"the host refused the key: add the public key ...","host_key":"known",
 "host_key_policy":"trust on first use (StrictHostKeyChecking=accept-new) ...",
 "credential":"deploy","fingerprint":"SHA256:3Qm4...","exit_code":255,
 "stderr":"build@box: Permission denied (publickey).","elapsed_ms":212}
```

`host_key` is `known`, `added`, `unknown`, `mismatch`, or `not_checked`.
`stderr` is the tail of the ssh stderr (at most 2000 bytes). The key
never shows. An `exedev` target is tested through the ssh target of its
recorded VM (`exedev@<ssh_dest>`, its `credential`); the report keeps
`"kind": "exedev"` and shows that address. With no VM, or a VM with no
ssh host yet, the test is `409 conflict`: bring the VM up with the `up`
verb or a holder's start, then test again (TER-651). A target of another
kind is `422 invalid` ("not supported"); for a target with a host agent,
use the `status` verb
(§17.3). A target whose credential does not work is `422 invalid`. Each
test writes a `target.test` audit row with `{result, host_key,
exit_code}`.

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

SSH targets can opt in to workspace mirroring with `sync_workspace = true`
in the target definition (`admin target new --kind ssh --sync-workspace`).
The default is false. Install `rsync` on the target. The daemon image includes
it. This uses the same SSH credential, port and host-key policy as exec.

Before exec, run or cp, Terrarium mirrors the caller's workspace to the same
absolute path on the target, creating it if needed.
On exe.dev the mirror is `<owned root>/workspace`. Card agents prepare the
owned root (including its ownership marker) before mirroring, and root cleanup
also removes the mirror. Other agents use `<work_root>/<project>/<agent>/workspace`,
or `<remote home>/<project>/<agent>/workspace` without `work_root`. Workspace
paths in cwd, command arguments, environment values and copy paths are
translated to that mirror for exec, run and checkout.
The sandbox workspace alias (`<configured sandbox home>/work`) is translated
in the same fields, including copy paths. On SSH targets the alias maps to
the canonical workspace path. Paths outside both roots and paths containing
`..` components are left unchanged, not normalized into the workspace.
Remote error text contains remote paths, which differ from the sandbox paths. On SSH targets
the mirror remains at the same absolute path. An operator must select
an agent. After a normally exited command (including nonzero exit) or a
completed copy, it stages the remote tree locally and pulls changes and
deletions before reporting the session done. Killed or lost commands leave
their output on the target. Sync failure is reported as `sync_failed`.

At every depth, `.git`, `.terrarium`, `.terrarium-stage`, `target` and
`node_modules` are excluded in both directions and protected from deletion.
The same rule excludes `.terrarium-git-trailers`, `.env`, `.env.*`, `.netrc`,
`.npmrc`, `.pypirc`, `id_*`, `*.pem`, and `.ssh` (including its contents).
These exclusions also apply to conflict snapshots. Supply build credentials
through target-side configuration or explicit secret injection, not workspace files.
Build caches stay on the target. Repository metadata and Terrarium control
files stay local. Do not use remote git operations with this mode.
Only one synced operation per workspace may run at a time.

If a local file, directory, symlink or permission changes while the command
runs, the pull is skipped entirely with `sync_conflict`, naming the changed
paths. Remote output remains on the target. Stop local editing during the
final pull. Recover remote files explicitly before starting another synced
operation, since its push makes the local tree authoritative again.
After the final apply, Terrarium scans again and reports a conflict if the
local tree differs from the staged result. This detects surviving late edits,
but is not an atomic filesystem transaction. Tool runs are not synced.

An exec session runs one command, one shell, or one file copy. It uses
plain requests only, so it works through the agent tunnel too
([agent-rpc.md](agent-rpc.md), "Streams over the tunnel").

| Endpoint | Body or query | Answer |
|---|---|---|
| `POST /api/targets/{name}/exec` | `{"cmd": [..], "cwd"?, "env"?, "pty"?, "shell"?, "timeout_secs"?, "agent"?, "durable"?}` | `201` `{"session_id", "target", "kind"}` |
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

On SSH and exe.dev, `durable: true` opts a noninteractive exec into restart
survival. Agent `run` commands and queue recipes request this mode internally.
The existing process wait or recipe continues with the original exit code
and output even when the command finishes while the daemon is down. PTY and
shell combinations are refused. Sending stdin `data` to a durable session
is `422 invalid`. Input-fed queue setup and bundle transfer do not opt in.
Ordinary execs, PTYs, and shells retain the previous interruption behavior.
The private remote output spool is under the bound card agent's recorded
owned root, or `<work_root>/.terrarium-runs` for other callers. The latter
has a daemon-owned marker. Without `work_root` it is
`/src/terrarium/.terrarium-runs`, else `~/.terrarium-work/.terrarium-runs`
when the remote user may not create `/src/terrarium` (permission denied, a
read-only file system, or a missing parent); a spool that is not usable either
way fails at once with a `spool error` that names `--work-root` (TER-1129). No-follow directory-FD checks reject symlinks,
foreign ownership and group/other writable components. Python 3 is required
for these checks; missing tools are terminal errors, never installed. The spool
retains the first 64 MiB per stream, and is removed after 24 hours of result
retention (at most the command timeout plus 24 hours and two minutes).
The remote lifetime is at most seven days, measured from the original
checkpoint even if first launch is delayed. Recovery refuses a changed
target incarnation, SSH host key or exe.dev VM ID with `interrupted (target
changed)`, without connecting to the replacement. Startup probes report
missing tools or spool errors, with a 30-second readiness deadline.
See host-protocol.md, Reconnect.

Durable SSH/exe.dev runs end when their foreground command exits or their
absolute deadline passes, whichever comes first. The supervisor sends TERM
to the command's process group, then KILL after five seconds. A descendant
that calls `setsid` leaves that group and is not stopped by these signals.
Bounded logger teardown cuts its inherited output streams, not the escaped
process, so it cannot keep the run open indefinitely. A foreground exit
preserves its exit code. A deadline ends with code 124.

The input body: `data` is base64, at most 1 MiB of bytes (`413
too_large` above). In a push, `data` is file bytes. `resize` is
`{"rows", "cols"}`. `signal` is a name such as `INT`. `eof` closes stdin.
`finish` is `{"bytes", "sha256"}` and ends a push. An input to a session
that has ended is `409 conflict`, except `eof` alone: it is a no-op with
`"accepted": false`. A fast command can end before the client closes its
stdin. `terrarium exec -i` and `shell` treat a `409` for `data` in the
same way: they stop the input, and the exit code comes from the output.

The stdout and stderr of an `exec` or `shell` session are redacted as
streams (PLAN §16b.6, TER-33), for the caller: an agent sees the names of
its project's secrets and `[secret]` for the others; the operator sees each
name. A secret split across two chunks is found. The last bytes of the
output (up to the longest secret value less one) wait for the next output,
or go out after 200 ms with no new output; so a secret written in two
parts more than 200 ms apart is not found whole. The `/output` reads and the
WebSocket show the same redacted bytes. A `cp` session is not changed.

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
- On a `host` target with `limits`, the command runs in the budget of the
  caller (§17.2, "Resource limits"). A host agent that cannot apply the
  limits gives `409 conflict`.
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
reads, or `TERRARIUM_GIT_SSH_KEY` for an SSH key, base64-encoded on one
line, which the script decodes with `base64 -d` into a 0600 temp file for
`GIT_SSH_COMMAND` and removes at exit. The secret is never on a command
line. The target needs `git` and `sh`, and `base64` for an SSH key. A vmlab
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

#### exe.dev VMs (`kind = "exedev"`)

An `exedev` target is an exe.dev VM that the daemon makes on `up` and
removes on `down` (`crate::exedev`, TER-294). It needs `ephemeral = true`,
no `keep`, no `address`, `host` or `port`, and a `credential`: an
`ssh_key` that exe.dev accepts for VMs with the target's tag.
`sync_workspace = true` mirrors the holder's workspace as on an ssh target.

```toml
kind = "exedev"
ephemeral = true
credential = "github-deploy-cloud-test"
sync_workspace = true

[exedev]                     # every key is optional
api_token = "exedev-api-token"   # the API token secret: new, ls, rm
cpu = 2                      # new --cpu
memory_gb = 4                # new --memory=<n>GB, at most 2 per vCPU
tag = "terrarium-dev-ssh"    # new --tag
user = "exedev"              # the ssh user on the VM
max_life_secs = 28800        # 60 to 28800 (8 hours)
```

- The lifecycle goes through `POST https://exe.dev/exec` with the token
  in the `Authorization` header only: `new
  --name=terrarium-x-<instance>-<target>-<id> --sandbox --tag=<tag>
  --cpu=<n> --memory=<n>GB`, `ls`, `rm`. `<instance>` is the exe.dev
  instance id of the daemon: 8 hex characters, made once in its database
  (table `exedev_instance`). Two daemons on one exe.dev account have two
  ids. Commands,
  copies and the workspace sync go over ssh to `<user>@<ssh_dest>` (the
  API stops a command at 30 s and a body at 64 KB). `up` waits up to 180 s
  for ssh.
- Each VM has a row in the table `exedev_vms`, written before `new` runs:
  the VM name, the target, the account it was made on (the token secret
  name, the target's project, and whether an admin set the target), the
  tag, the ssh host, and `created_at` and `expires_at`. The row outlives
  the target: `rm` always runs on the account the VM was made on, also
  after the target's `api_token` changes or the target is deleted.
- One lock per target serializes `up`, `down`, `status` and the sweep's
  removal of that target's VM. An `up` that finds a recorded VM in `ls`,
  in its life, adopts it (also a row that `new` never answered: the ssh
  host then comes from `ls`). A recorded VM past its life is removed and
  replaced, never adopted. A row with no ssh host that `ls` does not list
  stays for 10 minutes after its `created_at` (its `new` may still run):
  `status` answers `absent`, and `up` makes a new VM next to it. A `new` that fails removes its VM; when that
  `rm` fails too, the row stays for the sweep.
- At most 3 VMs at once over all exedev targets (the recorded ones and the
  `terrarium-x-` VMs in `ls`). The fourth `up` answers `409` with
  `exe.dev cap: 3 of 3 VMs run (...)`.
- The sweep runs one minute after the start, then at the next `expires_at`
  of a recorded VM and at least every hour, and on
  `POST /api/exedev/sweep` (the operator only; the answer is
  `{removed: [{vm, target, reason}], errors}`, audit row `exedev.sweep`).
  It runs `ls` once for each account of the rows and the targets, and
  removes:
  - `expired`: a recorded VM past its life, with a mail to the project
    inbox (or to the operator for a global target);
  - `orphaned`: a recorded VM whose target is gone;
  - `unready`: a recorded VM with no ssh host after 10 minutes, held or not;
  - `unheld`: a recorded VM that no running agent holds after 10 minutes
    (the holders are read again under the target lock, right before the
    `rm`);
  - `leftover`: a VM in `ls` with this daemon's `terrarium-x-<instance>-`
    prefix and a tag of the account, that no row of any account records,
    10 minutes after its `created_at` in `ls` (or after the sweep first
    saw it). A VM of another daemon (another instance id) is never a
    leftover here.
  Each removal of a recorded VM writes `target.destroy` with
  `"reason": "exedev_<reason>"`. A VM without the prefix is never removed.
- Deleting the target removes its VM. When that `rm` fails, the target is
  deleted and the row stays, so the next sweep removes the VM as
  `orphaned`.

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
| The target runs without it, ephemeral lab instance, no live exec session | A new lab instance with the new set: `vmlab destroy`, `vmlab.prepare`, `vmlab up`. Audit row `target.attach`. |
| The target runs without it, and it is Docker, permanent, or a session is live | Nothing runs. The agent gets the notification "target `<t>`: attach needs a restart of the target: ...". A Docker workspace change never replaces a running container, since background processes may outlive tracked exec sessions. |

A docker target with `container` (an existing container) and a vmlab target
with `lab` take no workspace.

**Detach.** When a holder stops, or its lease ends, and other running agents
still hold the target, a new lab instance without its
workspace runs, with the same rule (ephemeral, no live session; audit row
`target.detach`). Else the workspace stays attached until the target runs
again. Docker containers are not replaced to detach workspaces. When the
last holder goes, the destroy of §17.6 removes it all.

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
listener serves the binary and its SHA-256 (not the script), for the
self-update of host agents
([host-protocol.md, "Self-update"](host-protocol.md#self-update)). The
ingest listener does not serve these routes.

| Endpoint | Answer |
|---|---|
| `GET /install/host.sh` | `200` `text/plain`: `scripts/install-host.sh` with the defaults filled in (below). |
| `GET /install/terrarium-host?arch=<arch>` | `200` `application/octet-stream`: the static musl `terrarium-host` for the arch. |
| `GET /install/terrarium-host.sha256?arch=<arch>` | `200` `text/plain`: `<sha256 hex>  terrarium-host-<arch>-linux`. |

Rules:

- Each answer has the header `X-Terrarium-Version` (the build version of the daemon, `0.1.0+6e31754`),
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
  In the release image the binary is the release asset (docs/release.md).
  An optional `SHA256SUMS` in the dist dir (the file of a release) adds
  `release_sha256` to the self-update offer, and the daemon offers no
  binary that does not match it.
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

**Secrets.** A `vnc` or `rdp` target names a `login` secret (§5.2; the
target's project first, then global). The target's `username` wins over
the secret's. The daemon reads it only to build the `display` frame,
and sends it only to the target's host agent in that frame. No answer to
an agent or to the dashboard holds it, and the audit rows do not. The host
agent does not log it, and its error texts do not hold it.

### 17.11 Tool proxies

GitHub #21; the design is the wiki page `design/tool-proxies`. A tool is a
named command on a target that the operator defines. A granted agent runs
it as a CLI in its sandbox, and the daemon runs it on the target, so the
agent gets one tool and not the target. This section has the
definitions and the grants (P1a) and the run path (P1c, "Runs" below).
The sandbox shim (`/opt/terrarium/agent-tools/bin`) is TER-34, and the
approval of a run (P2) is "Approval" below.

**Definition.** `tools/<name>.toml` in the config repo (source `file`;
`POST /api/reload` picks up a change), or `POST /api/tools` (source
`api`):

```toml
name        = "tracker-read"                 # the file stem fills in a missing name
description = "Read tickets of the tracker"
target      = "wsl"
command     = ["/home/u/.local/bin/uv", "run", "/home/u/scripts/tracker.py"]
cwd         = "/home/u/scripts"
timeout     = "120s"
env_secrets = { TRACKER_TOKEN = "tracker-token" }
args        = { allow = ["show", "search"], deny_flags = ["--config"], max = 16, max_len = 4096 }
approval    = false                          # true, or { first = ["comment"] }
stdin       = false
max_output  = "1MiB"
rate        = "30/m"
project     = "demo"                         # optional: a project tool
```

The checks; a definition that fails one is `422 invalid`, and a file that
fails one is left out with an error line of the reload:

- `name`: 1-32 of `a-z`, `0-9`, `-`, starting with a letter; not
  `terrarium`, not a reserved name (`git`, `ssh`, `sh`, `python`, the
  harness CLIs, and others), and not a program of the sandbox `PATH`
  (`/usr/local/bin`, `/usr/bin`, `/bin`, the sbin dirs, the tools dir).
- `command`: a non-empty argv. There is no `shell` field (an unknown field
  is refused). The agent's arguments will be appended as more argv
  entries; no shell parses any of it.
- The interpreter rule: a command whose program is an interpreter (`sh`,
  `bash`, `pwsh`, `python*`, `node`, `ruby`, `perl`, `deno`, `bun`, and
  others), or a runner of one or of a script or package (`env`, `uv run`,
  `uvx`, `pipx run`, `poetry run`, `npx`), needs a fixed script and a
  non-empty `args.allow`. Inline code (`-c`, `-e`, `--eval`, `-Command`, `-`)
  is refused. This is a guard against a shell in disguise, not a proof: a
  tool is only as narrow as the script it runs.
- `target` must exist. A project tool may only name a target of its own
  project.
- `env_secrets`: env names of `A-Z`, `0-9`, `_`, not `PATH`, `LD_*`,
  `DYLD_*`, or `TERRARIUM_*`; each value names a secret of the same scope
  (a global secret for a global tool, a secret of the project for a project
  tool). The values never reach the agent.
- `timeout` up to 24h, `max_output` up to 64MiB, `rate` as `<n>/s`,
  `<n>/m`, or `<n>/h`, `args.max` up to 256, `args.max_len` 1 to 65536,
  `args.deny_flags` entries start with `-`, and `approval.first` is not
  empty.

| Endpoint | Body or query | Answer | Who |
|---|---|---|---|
| `GET /api/tools` | | `200` the tools: each definition with `scope` (`global` or `project`), `source`, `created_at`, `updated_at`, and for the operator and a sysadmin `grants` | all: the operator and a sysadmin see every tool; an agent sees the tools granted to it or to its project |
| `GET /api/tools/{name}` | | `200` the tool | as above; a tool that an agent may not see is `404` |
| `POST /api/tools` | the definition as JSON | `201` the tool. A name that exists is `409 conflict` | the operator and sysadmins |
| `PATCH /api/tools/{name}` | a JSON merge patch; `null` removes a field | `200` the tool. The name and the project do not change (`422`). A `file` tool is `409 file_managed` | the operator and sysadmins |
| `DELETE /api/tools/{name}` | | `204`; the grants go too. A `file` tool is `409 file_managed` | the operator and sysadmins |
| `GET /api/tools/{name}/grants` | | `200` `[{tool, grantee, grantee_label, created_by, created_at}]` | the operator and sysadmins |
| `POST /api/tools/{name}/grants` | `{"grantee"}`: `agent:<project>/<name>`, `agent:<id>`, or `project:<name>` | `201` the grant. A grant that exists is `409`; a project tool to another project is `422` | the operator and sysadmins |
| `DELETE /api/tools/{name}/grants` | `?grantee=` | `204`. No such grant is `404` | the operator and sysadmins |

**In the sandbox** (design D3). Each agent has a dir
`<runtime>/agent-tools/<agent id>/` with one symlink `<tool> -> terrarium`
for each tool granted to the agent or to its project. The sandbox binds
the dir read-only at `/opt/terrarium/agent-tools/bin` and puts it last on
`PATH`, so a tool never shadows a program. The daemon writes the dir at
each start, and again at each grant, revoke, change, or removal of a tool
and at a reload; the dir is bound, not the links, so a running agent sees
the change. The CLI runs a link as `terrarium tool run <tool> -- <args>`:
it posts the run, streams stdout and stderr, sends stdin to a tool with
`stdin = true` when stdin is not a terminal, stops the run on Ctrl-C (exit
130), and exits with the remote exit code (128 plus the signal, or 1 on an
error end such as `truncated`). `terrarium tool run --detach` (an agent only)
starts the run and puts a `process` wait (§6.7) on its tool session, as
`terrarium run` does for a command: the wait's summary is `tool run <id> on
<target>`, the wake mail has the exit code and the redacted output tail,
and `terrarium run log` shows the redacted output. A run that needs the
operator's approval refuses `--detach` (exit 2) before anything is asked:
run it without `--detach`, which waits for the answer. A tool name may not be `terrarium` or start
with `terrarium-`: those stay the CLI.

**Tools on templates** (design Q4). A template spec may have `tools`: a
list of tool names. A create from the template (the API, a fleet, or a
spawn) grants each to the new agent, as the delegation of the sysadmin who
set them: `created_by` of the grant is `template:<project/name>`, and the
`tool.grant` audit row has `via: "template"` and the template. Only a
sysadmin or the operator sets or changes `tools` on a template (`403`
otherwise, also on a project template; a fleet that sets or changes them
needs a sysadmin too). Each tool must exist, and a project tool may be on a
template of its own project only (a global template names global tools
only); else `422`. A create body takes no `tools` (`422`). The agent keeps
its grants when the template changes later.

**Dashboard.** Settings > Tools lists the tools with the definition
(secret names, never values), the grants with Revoke and a new grant (a
project tool offers only its project and that project's agents), Delete
for an API tool, and the recent runs (the `tool.run` audit rows). The
agent page's Details tab has a Tools section: the tools that the agent may
run, its own grants and its project's.

A grant does not put the target in the agent's policy and makes no lease.
A project grant covers each agent of the project, also the later ones.
Removing an agent or a project removes the grants to it; removing a
project removes its tools. Audit rows: `tool.create`, `tool.update`,
`tool.delete`, `tool.grant`, and `tool.revoke`. CLI: `terrarium tool ls`,
`tool show <name>`, `tool grant <name> <grantee>`, `tool revoke <name>
<grantee>`.

**Runs** (design D4 and D6, TER-30). A run is an exec session of the
kind `tool` on the tool's target, with its own routes:

| Endpoint | Body or query | Answer | Who |
|---|---|---|---|
| `POST /api/tools/{name}/run` | `{"argv": ["show", "T-1"]}`; any other field is refused (`400`) | `201` `{"session_id", "tool", "target"}` | an agent with a grant on the tool (its own or its project's), and the operator for a test |
| `GET /api/tools/sessions/{id}/output` | `?since=&wait=&max=` as §17.4 | `200` as the output of an exec session (§17.4): `since`, `next`, `chunks` (`stream` `stdout` or `stderr`, base64 `data`), `done`, `end`, `result` | the agent that started the run, or the operator; anyone else gets `404` |
| `POST /api/tools/sessions/{id}/input` | `{"data"?: <base64>, "eof"?: true}` | `200` `{"session_id", "accepted"}`. `422` when the tool has `stdin = false`; `409` when the run has ended | as above |
| `DELETE /api/tools/sessions/{id}` | | `204`: the run is killed and the session goes | as above |

The target session routes (§17.4) do not show a tool session (`404`),
also not to the agent that started it.

- **Who.** An agent with no grant on the tool, or a tool that does not
  exist, is `404 not_found`, the same answer (a tool that an agent may not
  see is not found, as in `GET /api/tools/{name}`). A revoke applies to
  the next run; a run in progress goes on to its end.
- **The command.** The daemon builds it from the definition: `command`
  with the agent's `argv` appended as more argv entries, the tool's `cwd`,
  `timeout`, and `env_secrets` (the values opened by the daemon, set in
  the process env on the target), no shell, and no pty. The agent sets
  none of these. There is no target policy check: the grant is the right.
  The other checks of an exec stay: the host agent is online, the open
  sessions of the agent (§17.4), and the resource limits of a `host`
  target with the agent as the holder.
- **The argument rules** (design D4, lead review L1). A run that breaks
  one is `422 invalid`, and the message names the argument and the rule:
  at most `args.max` arguments (default 64), each at most `args.max_len`
  bytes (default 4096), no NUL; with `args.allow`, the first argument is
  in it (a run with no argument is refused too). A deny flag `--name`
  refuses `--name`, `--name=...`, each abbreviation `--n`, `--na`, ...
  (`getopt_long` takes an unambiguous prefix), and the single-dash forms
  `-name` and `-name=...`. A short deny flag `-o` refuses each argument
  that starts with one dash and has `o` after it: `-o`, `-oVALUE`, and a
  bundle such as `-xo`. A literal `--` does not end the checks. Tool
  authors should accept only exact long flags; `args.allow` with
  subcommands is the main guard.
- **The rate.** A token bucket for each caller and tool: `rate` runs at
  once, then `rate` per period. A tool without `rate` gets `60/m`. Past
  it: `429 rate_limited`, with `error.details.retry_after` in seconds.
- **The output.** stdout and stderr together are capped at `max_output`
  (default 1 MiB) of raw bytes. Past the cap the process is killed, and
  the session ends with `end` `{"type": "error", "code": "truncated",
  "message"}` and `result` `{"bytes", "truncated": true}`; a run that
  ends under the cap has `result` `{"bytes", "truncated": false}`. Each
  stream is redacted as a stream, so a secret split across two chunks is
  found: first the values of the tool's own `env_secrets` (from 4 bytes
  up, also a value that the daemon matcher leaves out as weak, label
  `[secret:tool/<secret>]`), then the daemon matcher (PLAN §16b.6) with
  the view of the caller's project.
- **stdin.** With `stdin = false` (the default) the command's stdin is
  closed at the start, and input is `422`. With `stdin = true` the input
  goes to the command, at most `max_output` bytes in all.
- **Audit and events.** One `tool.run` audit row at the end of each run:
  `tool`, `target`, `agent` (`project/name`), `argv` (redacted), `exit`,
  `end`, `duration_ms`, `bytes`, and `truncated`. The output is not in the
  audit. Two `tool.run` events on the agent's stream: `{"change":
  "started", "tool", "target", "session", "args"}` and `{"change":
  "ended", "tool", "target", "exit", "duration_ms", "bytes",
  "truncated"}`.

**Approval** (design D5 and Q5, TER-31). `approval = true` asks the
operator before each run of an agent; `approval = { first = [...] }` asks
when the first argument is in the list. The operator's own run never
asks. `approval_timeout` (1s to 24h, default `1h`) is how long a run
waits.

- `POST /api/tools/{name}/run` checks the grant, the arguments, and the
  rate first. Then a run that asks answers `202` `{"pending": true,
  "question", "expires_at", "tool"}`: the daemon writes a question of the
  kind `tool_run` on the agent (§6.8), with the text `Run tool <tool> on
  <target> for <project/agent>?`, the exact argv in `context` (one quoted
  entry per line, redacted), the options `Run` and `Refuse`, and
  `tool_run` `{tool, target, argv, expires_at}`. The same run again (the
  same agent, tool, target, and argv) while it is open gives the same
  question. It counts toward the 50 open questions of the agent (`429`
  past them).
- Only the operator answers it (`POST /api/questions/{id}/answer` with
  `option` `0`/`"Run"` or `1`/`"Refuse"`); an agent, also a sysadmin, gets
  `403`. The agent gets no mail on the answer or a retract: its CLI waits
  on the question (`GET /api/questions/{id}`, which the agent may read).
  The agent may retract its own `tool_run` question, whatever its role.
  It cannot be updated (`409`). The Inbox shows it as a `question` item
  (ID `tool_run:<id>`) and counts it. A list of every kind (the recap,
  `question ls`) leaves it out.
- The run with `{"argv", "approval": "<question id>"}` starts when the
  question is a `tool_run` question of the same agent and tool, with the
  same argv, answered `Run`, and not used. The daemon marks it used
  (`used_at`) in the same write, so an approval runs once. Otherwise
  `409 conflict`, and the message says why (not answered yet, refused,
  retracted, another argv, used already). An approved run takes no token
  of the rate. Its `tool.run` audit row and `started` event have
  `approval` (the question), and the audit row `approved_by`.
- At `expires_at` the daemon retracts an open approval with the reason
  `the approval timed out` (a sweep each 30 s).
- The CLI (`terrarium tool run`, a tool link) prints that it waits, polls
  the question, and then runs with the approval. A refuse, a retract, a
  timeout, or a `409` on the run ends it with exit code 77; Ctrl-C
  retracts the question and exits 130.

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

An agent with a `subscription_pool` that holds its `subscription`: the plan
and the drift compare the pool, not the current member. A swap (§6.2) is
not drift, and an `apply` keeps the current member (PLAN §16.3).
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
target, an agent verb, a quota probe, a card on the board, or a webhook (an
HTTP request, §19.5). A trigger is
a schedule whose spec is `on`: an event of the log (§9, §20) that matches
starts its run (§19.4). A project action is a schedule whose spec is
`manual`: a person starts each run with its button (§19.10). The daemon
runs a schedule as the actor that made
it (`run_as`). Before each run, the daemon checks the rights of that
actor again. A project- or system-owned schedule has no actor behind it
(§19.6). PLAN §12b and §12c have the design.

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
  "owner": "actor",
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
| `owner` | `actor` (the default), `project`, or `system` (§19.6). |
| `run_as` | The actor of the runs of an actor-owned schedule. It is `created_by`, until a caller changes the `action` or the `owner`, or takes the schedule over: then it is that caller. A project- or system-owned schedule does not use it. |
| `next_run_at` | The next run. `null` when the schedule is disabled, when an `at` schedule has run, always for a trigger and a project action, and for a `day_start` schedule while no start planned its run. |
| `last_run` | `{at, status, summary, run_id}` of the last run, or `null`. |
| `runs_count` | The runs so far, the skipped runs too. |
| `source` | `api`, `file` (`[[schedules]]` in `projects/<name>.toml`), or `fleet:<name>` (§18.1). |
| `consecutive_skips` | The runs in a row that the rights check skipped. |
| `disabled_reason` | Why the daemon disabled the schedule. Absent when it did not. |
| `day_start_done` | A `day_start` schedule: the local day (`YYYY-MM-DD`, in its `tz`) of its last run. A start on that day, or on an earlier day, plans no run. Absent before the first run. |
| `day_start_last` | A `day_start` schedule: `{cause, at}` of the last detected start that planned a run. `cause` is `daemon_start`, `resume`, `host_reconnect`, or `day_change`. Absent before the first plan. |

The specs:

| `spec` | Meaning |
|---|---|
| `{"kind": "cron", "expr": "*/15 * * * *", "tz": "UTC"}` | Five fields: minute, hour, day of month, month, day of week. The day of week is `0`-`7` (`0` and `7` are Sunday) or `SUN`-`SAT`. `@hourly`, `@daily`, `@weekly`, `@monthly`, and `@yearly` are also valid. `tz` is `UTC` (the default) or an IANA zone, for example `Europe/London`. |
| `{"kind": "interval", "every": "30m"}` | A number and a unit (`s`, `m`, `h`, `d`), or a sum, for example `1h30m`. The minimum is 60 s. The runs are at `start + k * every`; a new spec sets a new start. |
| `{"kind": "at", "at": "2026-11-01T02:00:00Z"}` | One run at an RFC 3339 time in the future. |
| `{"kind": "on", "kinds": ["ci.run.failed"], "match"?, "dedupe"?, "escalate"?, "max_per_hour"?: 30}` | A trigger: each matching event starts a run. See §19.4. |
| `{"kind": "manual"}` | A project action: no time and no event. Only `POST .../run` starts a run. See §19.10. |
| `{"kind": "day_start", "tz"?: "UTC", "not_before"?, "not_after"?, "delay"?, "weekdays"?}` | Start of day: one run per calendar day in `tz`, at the first start of the day. See "Start of day" in §19.3. `not_before` and `not_after` are local times as `HH:MM`; `not_after` is not before `not_before`. `delay` is a duration as for `interval`, at most `12h`. `weekdays` is a list of `mon` to `sun`; empty or absent is every day. `catch_up` does not apply. |

The actions:

| `action` | Meaning |
|---|---|
| `{"type": "mail", "to": "<agent>\|project:<p>\|operator", "subject", "body", "thread"?}` | Send mail through the normal send path (§7) as `run_as`. The scope rules of §7.1 and the loop guard apply. With `thread`, each run posts on that thread, and the loop guard counts the messages. A bare agent name resolves in the schedule's project. An empty `subject` becomes `schedule <name>`. |
| `{"type": "turn", "agent", "text", "start_if_stopped"?: false}` | Type a turn, as `POST /api/agents/{id}/send` (§6.2). A stopped agent (for example after its idle stop) gets the turn as a send gives it: the turn waits in its queue and a wake starts the agent. The run waits until the agent is ready (at most 120 s, up to 3 wakes) and is `ok`, `woke <agent> and sent the turn`. When the agent does not start (paused, parked, in `error`, held, a disabled subscription, the self-update drain, or not ready in time), the run is `failed` with the reason, the turn stays in the queue for the next wake, and `system` mails the owner of the schedule (the `run_as` agent, the project inbox, or the operator). With `start_if_stopped`, the run first starts the agent as `POST /api/agents/{id}/start` does (a start also ends a usage-limit hold), waits until it is ready (at most 120 s), then types the turn. |
| `{"type": "exec", "target", "cmd": ["make", "test"], "cwd"?, "timeout_secs"?}` | Run a command on a target through the executor (§17.4), as `run_as`, with the same policy, leases, and `target.exec` audit row as any exec. `timeout_secs` is 300 by default, 3600 at most. |
| `{"type": "agent", "agent", "verb": "start\|stop\|restart"}` | The agent verb of §6.2, as `run_as`. |
| `{"type": "probe", "subscription"}` | Run the quota probe of the subscription (§16.3). |
| `{"type": "webhook", "url" \| "url_secret", "method"?, "format"?, "template"?, "content_type"?, "headers"?, "signing"?, "timeout_secs"?}` | Send an HTTP request about the event (or about the run of a time schedule). The run queues a delivery, and the delivery worker sends it with retries. See §19.5. |
| `{"type": "card", "op": "open\|close", "key", "title"?, "body"?, "column"?}` | A card on the board of the schedule's project (§8), as `run_as`. `open`: make a card with the label `trigger:<key>` in `column` (default `backlog`), or, when a card with that label is open (not in `done`, not archived), add `body` to it as a comment. `close`: move the open card with that label to `column` (default `done`), with `body` as the comment. No open card to close is `ok`. A global schedule cannot have a card action. |

A trigger runs only `mail`, `exec`, `card`, and `webhook`. To notify an
agent, a trigger mails it: `turn`, `agent`, and `probe` on a trigger are
`422` (`details.field` is `action`).

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
| `trigger` | `cron` (the timer, for each spec kind), `manual` (`POST .../run`), `catch_up`, `event` (a trigger), or `day_start` (the planned run of a `day_start` schedule). |
| `summary` | One line. A skip starts with `skipped:`. |
| `output` | `webhook`: `delivery` (the ID) and `url` (§19.5). `exec`: `target`, `cmd`, `exit_code`, `end`, and the last 4000 bytes of `stdout` and `stderr`. `mail`: `message_id`, `thread_id`, `to`, `held`, `held_reason`. `turn`: `agent`, `started`, `delivery`, `status_before`. `agent`: `agent`, `verb`, `status`. `probe`: the probe outcome with `windows`. `card`: `card` (the ID, or `null`), `op` (`open`, `comment`, or `close`), and `column` for a close. |
| `error` | Why the run failed or was skipped. |
| `actor` | Who triggered the run: `run_as` for a timed run, the caller for a run-now, the actor of the event for a trigger run (for example `source:terra/github`). |
| `event_seq` | The `seq` of the event that started a trigger run. `null` for the other runs. |
| `event` | `{seq, kind, summary}` of that event, while the log has it. Absent for the other runs. |
| `day_start` | A `day_start` run: `{cause, at}` of the detected start that planned it (as `day_start_last`). Absent for the other runs. |

### 19.2 Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/schedules?project=&enabled=&kind=` | | `200`, a list. With `project`: the project's schedules, and the global ones when the caller may read them. Without: all for the operator and a sysadmin; the caller's project for another agent. `kind=time` gives only the time schedules, `kind=trigger` only the triggers, `kind=action` only the project actions (spec `manual`); another value is `400`. |
| `POST /api/schedules` | `{name, project?, spec, action, enabled?: true, catch_up?: "skip", owner?: "actor"}` | `201`, the schedule. An absent `project` is the agent caller's project (the operator: global). `null` is global. |
| `GET /api/schedules/{id}` | | `200`, the schedule. `{id}` is a ULID, `project/name` (encode the `/` as `%2F`), or a name. |
| `PATCH /api/schedules/{id}` | `{name?, spec?, action?, enabled?, catch_up?, owner?}` | `200`, the schedule. A new `action` makes the caller the `run_as`. `owner` moves it (§19.6); `owner: actor` makes the caller the `run_as` (a take-over). |
| `DELETE /api/schedules/{id}` | | `204`. The runs go with it. |
| `POST /api/schedules/{id}/run` | | `200`, the run when it ends. It runs as `run_as`, or with the owner's rights (§19.6). A disabled project action is `409` (§19.10); a disabled time schedule still runs. |
| `POST /api/schedules/{id}/enable` | | `200`, the schedule, with a new `next_run_at`. An enable sets `consecutive_skips` to 0. |
| `POST /api/schedules/{id}/disable` | | `200`, the schedule. |
| `GET /api/schedules/{id}/runs?limit=&cursor=` | | `200`, the runs, newest first, with `X-Next-Cursor` (§2.4). |
| `GET /api/schedules/{id}/deliveries?limit=&cursor=` | | `200`, the webhook deliveries, newest first, with `X-Next-Cursor` (§19.5). The read rights of the schedule. |
| `POST /api/schedules/{id}/deliveries/{delivery}/redeliver` | | `202`, a new `pending` delivery: a copy of `{delivery}` with `redelivery_of`, due now. The rights of a run-now. A `dropped` row is `422`. A disabled project action is `409` (§19.10). |
| `POST /api/schedules/{id}/test` | none, or `{event?: {kind?, summary?, project?, agent?, actor?, data?}}` | `200`, a test run (§19.7): `{test: true, action, matched, reason?, event?, status?, summary?, output?, error?, delivery?, request?}`. Any action kind. The rights of a run-now; for a webhook, a secret that the caller or `run_as` may not use is `403`. A body that is not this shape is `400`; an empty `event.kind` or an `event.data` that is not an object is `422` (`details.field` `event.kind`, `event.data`). A disabled project action is `409` (§19.10). |

Errors:

| Error | When |
|---|---|
| `422 invalid` | A field is not valid. The message starts with the field, and `details.field` names it: `name`, `spec`, `spec.expr`, `spec.tz`, `spec.every`, `spec.at`, `action`, `action.to`, `action.body`, `action.agent`, `action.text`, `action.target`, `action.cmd`, `action.cwd`, `action.timeout_secs`, `action.subscription`, `action.subject`, `action.key`, `action.title`, `action.column`, for a webhook `action.url`, `action.url_secret`, `action.template`, `action.content_type`, `action.headers`, `action.signing.secret`, `action.signing.header`, `max_concurrency`, `project`, and for a trigger `spec.kinds`, `spec.match`, `spec.dedupe.key`, `spec.dedupe.window`, `spec.escalate.key`, `spec.escalate.after`, `spec.escalate.reset_on`, `spec.escalate.to`, `spec.max_per_hour`. A cron error shows the expression: ``spec.expr: bad cron expression `*/15 * * *`: use five fields ...``. An interval under 60 s and an `at` in the past are `422`. |
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
- `webhook`: any URL that the address rules of §19.5 allow. A secret of a
  header, the URL, or the signature: a secret of the project, or a global
  secret that a grant (`use` or `read`) gives to the project or to the
  agent.
- A trigger's `escalate.to`: as for `mail`.

A global schedule needs a sysadmin. An enable and a run-now check the
action again; a disable and a delete check only the scope. An agent that
enables or runs a schedule whose `run_as` is another actor must also be
able to do the action itself: use the target of an `exec` (its policy or
a lease), and mail the recipient of a `mail` under its own §7.1 rules.
Else it is `403`, so a run-now cannot borrow the rights of `run_as`.

**The rights check at each run.** Before each run, the daemon reads the
`run_as` actor from the store and applies the rules above to it (a
project- or system-owned schedule: the owner's rules, §19.6). When the
agent is deleted (`agent rm`), the daemon disables the schedule at once
(§30.7). When the agent lost its role, or the action names an object
that is gone, the run is `skipped` with the reason, and
`consecutive_skips` goes up by 1. At 3, the daemon disables the schedule,
writes `disabled_reason`, and mails the operator from `system` ("Schedule
`<project>/<name>` disabled"). Any other end of a run sets the count to 0.
A skip for another reason (`still running`) does not count.

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

**Start of day.** A `day_start` schedule has no time of its own. The
daemon plans its run at each detected start:

- `daemon_start`: the daemon starts.
- `resume`: the timer runs again after a gap of more than 60 s (the
  machine slept).
- `host_reconnect`: the host agent of the target of an `exec` action
  comes back online. An `exec` whose target is unknown, or whose host
  agent is offline, plans no run until then.
- `day_change`: the day changes in `tz` while the daemon is up.

A start plans a run when the day in `tz` is after `day_start_done` and is
in `weekdays`. The run is at the start plus `delay`, and not before
`not_before`. A run that would be after `not_after` is not planned. A
local time that a clock change skips moves on by one hour; a time that it
shows twice is the first instant for `not_before` and the last for
`not_after`. The planned run sets `next_run_at`, and the timer starts it
with the trigger `day_start`. It records the day of its start in
`day_start_done`, so the other starts of that day plan nothing: one run
per day. A run planned before midnight for a time after midnight runs for
the day of its start. The daemon drops a planned run when the target's
host agent goes offline, when it is past `not_after`, or when the machine
slept past it into a later day; the next start plans again. At a daemon
start, a planned run that passed is dropped, with no catch-up run.
`schedule show` gives `next run: at the first start after <day>` while no
run is planned.

**Retention.** The daily prune timer keeps the newest 500 runs of each
schedule.

**Audit.** `schedule.create`, `schedule.update` (`enable` and `disable`
too), `schedule.delete`, and `schedule.run` (`{id, run, trigger, status,
summary, run_as}`), with the actor of the request, or `run_as` for a
timed run. The actions write their own rows as well: `agent.send`,
`agent.start`, `target.exec`, and so on, with the `run_as` actor. The
writes of a trigger run, and of each run of a project- or system-owned
schedule, have the actor `schedule:<id>`; the `target.exec` row of such a
run has it too, with `schedule`, `owner`, and `run_as` (an actor-owned
schedule) in the detail.

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

### 19.5 Webhooks

The `webhook` action sends an HTTP request about each event of a trigger
(or about each run of a time schedule). A run does not send: it renders
the body, writes one `pending` delivery, and is `ok` with
`output: {delivery, url}`. The delivery worker sends it and records each
attempt. Use it to page a person (Slack, Discord, ntfy) or to call a
self-hosted tool when a monitor changes status.

```json
{
  "type": "webhook",
  "url": "https://hooks.example.com/terrarium",
  "method": "POST",
  "format": "json",
  "headers": [{"name": "Authorization", "secret": "hook-token"},
              {"name": "X-Env", "value": "prod"}],
  "signing": {"secret": "hook-sign", "header": "X-Terrarium-Signature", "algo": "sha256"},
  "timeout_secs": 10
}
```

| Field | Meaning |
|---|---|
| `url` | `http` or `https`. A URL with a user or a password is `422`. Give `url` or `url_secret`, not both. |
| `url_secret` | The name of a secret whose value is the URL. A Slack or Discord webhook URL is a credential: keep it in a secret. The project's secret first, then the global one. |
| `method` | `POST` (the default) or `PUT`. |
| `format` | `json` (the default), `slack`, `discord`, `ntfy`, or `template`. See below. |
| `template` | The body of `format: "template"`. |
| `content_type` | The `Content-Type`. Default `application/json`; `text/plain; charset=utf-8` for `ntfy`. |
| `headers` | At most 20. `{name, value}` or `{name, secret}`: the value is the main field of the secret (the `value` of an `api_key`, `token`, or `github_token`; the one field of a `generic`). `Host`, `Content-Length`, `Content-Type`, `Transfer-Encoding`, `Connection`, the other hop-by-hop headers, and `X-Terrarium-*` are `422`. |
| `signing` | `{secret, header?, algo?}`. The daemon sends `<header>: sha256=<hex HMAC-SHA256 of the raw body, with the secret as the key>`. `header` is `X-Terrarium-Signature` by default; `algo` is `sha256`. |
| `timeout_secs` | The limit of one request: 10 by default, 30 at most. |

The document of a delivery:

```json
{"event": {"seq": 812, "id": "01MB...", "kind": "monitor.status", "at": "2026-11-03T10:00:00.000Z",
           "project": "terra", "agent": null, "actor": "system",
           "summary": "monitor disk-usage: ok → crit 93 %",
           "data": {"monitor": "disk-usage", "from": "ok", "to": "crit", "value": 93, "unit": "%"}},
 "trigger": {"name": "disk-alert", "project": "terra"}}
```

| `format` | Body |
|---|---|
| `json` | The document. |
| `slack` | `{"text": "[<project>] <kind>: <summary>"}`. The summary is the one of the event log, as in the trigger mail. |
| `discord` | `{"content": "[<project>] <kind>: <summary>"}`, at most 2000 characters. |
| `ntfy` | The summary as plain text, with `Title: <project>: <kind>`, `Priority`, and `Tags`. A monitor event takes its priority from its new status: `crit` 5, `warn` and `unknown` 4, `ok` 2. A `*.failed` event is 4; any other event is 3. `Tags` is an emoji short code (`rotating_light`, `warning`, `white_check_mark`, `x`, `bell`) and the kind. |
| `template` | The template with each `{{path}}` filled from the document: `{{event.kind}}`, `{{event.summary}}`, `{{event.project}}`, `{{event.at}}`, `{{event.data.status}}`, `{{trigger.name}}`. A missing value is empty. With a JSON content type, a value is escaped for the inside of a JSON string, so `"{{event.summary}}"` stays valid JSON. `{{path \| json}}` puts the value as a JSON literal (a string with its quotes, `null` when missing), for a place outside a string. Another placeholder or filter is `422`. With a JSON content type, a template that does not render valid JSON for a sample event is `422`. |

Each request also has `X-Terrarium-Delivery` (the delivery ID: a
receiver can drop a repeat), `X-Terrarium-Event` (the kind),
`X-Terrarium-Timestamp` (Unix seconds), and `User-Agent:
terrarium/<version>`. A body over 256 KiB fails the run.

**Secrets.** A secret of `url_secret`, `headers`, or `signing` needs a
right of the trigger's `run_as` at the write and again at each attempt:
the operator and a sysadmin use any secret; a project_admin uses a secret
of its project, or a global secret with a grant (`use` or `read`) to its
project or to itself. No right is `403` at the write and at a test send;
a queued delivery fails with `refused: ...` and no request. A missing
secret is `422`. The daemon opens the secrets at each attempt and never
stores or logs a value: the row keeps only the rendered body, the log
URL has no query (`?…`), and the answer and the error have each secret
value replaced. No request may carry the daemon's API token
(`TERRARIUM_TOKEN`): such a request fails with no send.

**Delivery and retries.**

- At most 8 requests at once in the daemon, and 2 to one host and port.
- A `2xx` is `ok`. A network error, a time-out, a `5xx`, or a `429` is
  tried again after 10 s, 1 min, 5 min, 30 min, and 2 h; a longer
  `Retry-After` (seconds or an HTTP date, at most 2 h) wins. After the
  sixth attempt the delivery is `giving_up`. Any other answer (a `3xx`,
  a `4xx`) is `failed`, with no retry.
- The `pending` rows stay in the store: after a daemon restart, the worker
  sends them when they are due. The delivery of a disabled trigger waits
  until the enable.
- Each attempt ends with a `webhook.delivery` event (§9).
- The 20th failed attempt in a row of a trigger (a test send does not
  count) disables it, writes `disabled_reason`, and mails the operator one
  time ("Webhook trigger `<project>/<name>` disabled"). An `ok` sets the
  count to 0, and so does an enable. The schedule shows the count in
  `webhook_failures` when it is not 0.
- Retention: the daily prune deletes the deliveries older than 7 days.

**The rate limit.** At most 60 deliveries of one trigger in a minute. An
event over it is dropped: its run is `skipped: rate limit ...`, and one
`dropped` row for the minute counts the dropped events (`dropped`). The
`max_per_hour` of the trigger (§19.4) applies first. A noisy event such
as `monitor.result` (one for each run of each monitor) can flood a
receiver: a trigger on `monitor.status` (only a change of status) is
usually the right one.

**The address rules (SSRF).** The URL comes from a project_admin, a
sysadmin, or the operator (the rights of a trigger write). The daemon:

- takes `http` and `https` only, and no user or password in the URL;
- resolves the host one time for each attempt and connects to that
  address, so a DNS answer that changes between the check and the connect
  cannot move the request; it uses no proxy from the environment;
- never follows a redirect: a `3xx` is `failed`, and the error names the
  `Location`;
- reads at most 64 KiB of the answer and keeps the first 2 KiB.

`TERRARIUM_WEBHOOK_PRIVATE` sets which addresses a webhook can reach:
`allow` (the default: a self-hosted tool on the LAN or the same host is
common) or `deny`. With `deny`, an address that is loopback, link-local
(`169.254.0.0/16`, `fe80::/10`, the cloud metadata address), private
(RFC 1918, `100.64.0.0/10`), unique-local (`fc00::/7`), unspecified,
multicast, or an IPv4 address in an IPv6 form of these, and any port of the
daemon's own listeners (the API, the artifact origin, the ingest, and the
host listener), is refused before the connect: the attempt is `failed`
with `refused: ...`. A public deployment where untrusted people are
project_admins sets `deny`.

A delivery:

```json
{
  "id": "01MB2...",
  "schedule": "01MA0V7N6Q1Y3K2P4R5S6T7U8V",
  "event_seq": 812,
  "event_kind": "monitor.status",
  "test": false,
  "url": "https://hooks.example.com/terrarium",
  "method": "POST",
  "content_type": "application/json",
  "attempt": 2,
  "status": "pending",
  "http_status": 503,
  "duration_ms": 41,
  "response": "busy",
  "error": "status 503",
  "next_attempt_at": "2026-11-03T10:01:10.000Z",
  "created_at": "2026-11-03T10:00:00.000Z",
  "updated_at": "2026-11-03T10:00:10.050Z",
  "request": "{\"event\": ...}",
  "request_bytes": 412
}
```

| Field | Meaning |
|---|---|
| `status` | `pending` (queued, or a retry waits for `next_attempt_at`), `ok`, `failed`, `giving_up`, or `dropped` (a rate-limit note with `dropped`, the count). |
| `attempt` | The attempts so far. |
| `url` | For the log: scheme, host, port, and path. A query shows as `?…`; a URL from a secret shows as `(secret <name>)`; the `url` of a test send shows its host. |
| `response` | The start of the answer (at most 2 KiB), redacted. |
| `error` | Why the attempt failed: `status 503`, `timed out after 10 s`, `refused: ...`. |
| `request`, `request_bytes` | The start of the body (at most 4 KiB) and its size. |
| `test`, `redelivery_of` | A test send; the delivery that a redeliver copied. |

**Audit.** A trigger write writes `schedule.create`, `schedule.update`,
and `schedule.delete` as for any schedule; a redeliver writes
`schedule.webhook.redeliver` (`{id, delivery, of}`).

Example: page ntfy when a monitor of `terra` goes `crit`.

```sh
curl ... localhost/api/schedules -d '{"name": "page-crit", "project": "terra",
  "spec": {"kind": "on", "kinds": ["monitor.status"],
           "match": [{"path": "data.to", "op": "eq", "value": "crit"}]},
  "action": {"type": "webhook", "url": "https://ntfy.example/terra-alerts", "format": "ntfy"}}'
curl -X POST ... localhost/api/schedules/terra%2Fpage-crit/test
curl ... 'localhost/api/schedules/terra%2Fpage-crit/deliveries?limit=10'
```

### 19.6 Owners

A schedule has an `owner`, as a monitor has (§30.7):

- `actor` (the default): the runs act as `run_as`, with the rules of
  §19.3.
- `project`: the runs have the rights of the project. The action stays
  inside the project, as a project_admin's (§19.3). An `exec` uses a
  target of the project, or a global target that is granted to the
  project for `schedules` (§30.7). A webhook secret is a secret of the
  project, or a global secret with a grant to the project. A global
  schedule cannot be project-owned (`422`). No agent is behind it, so a
  lead that goes changes nothing.
- `system`: the rights of the operator.

To make, change, run, enable, delete, or take over a project-owned
schedule needs project_admin of the project, a sysadmin, or the operator;
a system-owned schedule needs a sysadmin or the operator. The store
audits each write (`schedule.update` with `owner` in `changed`). Move a
schedule in one command: `terrarium schedule set <s> --owner project`
(`schedule edit` is the same command).

The action of a project- or system-owned schedule goes through the
operator's paths after the owner's rights check. Its writes, and the
`target.exec` rows of its `exec` runs, have the actor `schedule:<id>`.
Its mail comes from `project:<name>` (project-owned) or `system`
(system-owned). The card of a `card` action has the author `operator`.
An `exec` uses the resource budget of the holder `<project>/schedules`
on a `host` target with `limits`.

When the `run_as` agent of an actor-owned schedule is deleted, the daemon
disables the schedule with a reason and mails the leads and the operator
(§30.7). An enable by a project admin then makes it the `run_as`.

### 19.7 Test runs

`POST /api/schedules/{id}/test` (`terrarium schedule test`) runs the
action of a schedule one time as a test, for any action kind: `mail`,
`turn`, `agent`, `exec`, `probe`, `card`, and `webhook`.

The sample event: for a trigger, the sample of its first kind (as the
webhook samples of §19.5). The body can give `event`: each field that it
has replaces the field of the sample. A `kind` picks the sample of that
kind. Other keys (`seq`, `id`, `at` of an event from the log) are
ignored, so an event from `GET /api/events` works as it is. A time
schedule uses no event, and its action runs as it is.

The match: with an `event` in the body, a trigger checks it against its
scope (a project trigger sees only its project's events), its `kinds`,
and each `match` rule. `matched` is `true` when all hold. When one fails,
nothing runs: `matched` is `false`, and `reason` names the scope, the
kind, or the first rule that does not hold, for example ``the match rule
`data.monitor eq m1` does not hold for the event``. Without an `event`
(the default sample is generic), and for a time schedule, there is no
check: `matched` is `null`, and the action runs. The loop guard,
`dedupe`, `escalate`, and `max_per_hour` do not apply to a test.

The run: the action runs as `run_as`, with its templates filled from the
sample, and the answer has `status` (`ok`, `failed`, or `skipped` when
the rights refuse it), `summary`, `output`, and `error`. A test is marked:

- A mail subject starts with `[test] `, and the body starts with `This is
  a test run of <schedule> (terrarium schedule test).`
- A turn text starts with `[test] `.
- A card action uses the key `test:<key>` (the label
  `trigger:test:<key>`) and a title that starts with `[test] `. So a real
  event never comments on a test card, and a test `close` finds no real
  card.
- A webhook delivery has `test: true`, one attempt, and no retry. The
  answer also has `delivery` and `request` (`{method, url, headers,
  body}`, each secret value as `[secret:<name>]`).
- An `exec`, an `agent` verb, and a `probe` run as they are.

A test writes no run (`GET .../runs` and `last_run` do not change), and
changes no counter: no dedupe or escalation state, no rate limit, no
failure or skip count. The writes of the action carry the actor
`schedule:<id>` and `data.cause` `{schedule, event: null, depth: 1,
test: true}`. Each test writes the audit row `schedule.test`
(`{action, matched, reason, event_kind, status, summary}`).

```json
{"test": true, "action": "mail", "matched": true,
 "event": {"seq": 0, "id": "", "kind": "monitor.status", "at": "2026-11-04T10:00:00.000Z",
           "project": "terra", "agent": null, "actor": null,
           "summary": "monitor m1: ok → crit", "data": {"monitor": "m1", "to": "crit"}},
 "status": "ok", "summary": "mail 01JX... to terra/lead",
 "output": {"message_id": "01JX...", "thread_id": "01JX...", "to": "terra/lead",
            "held": false, "held_reason": null}}
```

### 19.8 Board rules

A project has one board rules file, `rules.wcl` (TER-162, TER-166). A
rule reacts to an event of the log and runs verbs on the board. The
daemon stores and checks the file here; the engine (§19.9) runs it.

If stored text no longer parses under the current schema, GET rules
preserves the text and hash, returns empty `rules`, and adds
`problems: [{line, column, message}]`. The engine emits a project event
`rules.invalid` with the stored hash and problems when it loads that
revision. Its own rules do not run, but the built-in pack still runs in
rules mode. A cached revision does not emit again until reloaded (for
example after a daemon restart).

The file is WCL. The daemon ships its schema
(`crates/terrarium-core/src/rules.schema.wcl`, `GET /api/rules/schema`)
and checks each file against it when it loads the file and on each edit:

```wcl
rule "validate-pass" {
  description = "A passed validation goes to review."
  on = ["job.verdict"]                   // event kind globs
  when "data.verdict" { eq = "passed" }  // eq, ne, glob, or one_of
  card_label  { remove = ["validation:running", "validation:queued"] }
  card_move   { to = "@review"  comment = "{{job.end_comment}}" }
  card_assign { who = "none" }
}
```

A rule has a name (unique in the file), `on`, optional `description`,
`enabled` (default `true`), `max_per_hour` (1 to 600), any number of
`when` conditions (all must hold; the paths of a trigger `match`, §19.4),
any number of `if_card` and `if_job` conditions on the current state
(§19.9), and at least one verb. The verbs run in the order of the file:
`card_move` (`to`, `from?`, `comment?`, `position?` `top` or `bottom`),
`card_label` (`add?`, `remove?`), `card_comment` (`text`), `card_assign`
(`who`), `card_prepend` (`text`), `mail` (`to`, `subject`, `body?`),
`card_blocked_mail`, `card_done_mail`, `job_queue` (`kind` `validate`, `merge`, or `batch`; `from`, `limit?`,
`min_cards?`, and `max_wait_minutes?` for a batch only;
`max_wait_minutes` needs `min_cards`, and `min_cards` is at most
`limit`), `job_cancel`, `job_rerun` (`max_attempts?`), `job_split`,
`job_fix` (`to?`, `labels?`), `agent_park`, `agent_unpark` (`turn?`),
`agent_end` (`outcome` `done` or `cancelled`), and `script` (`source`,
`fuel?`; TER-173, see "Script steps" in §19.9).

The checks beyond the schema:

- Plain data only: rule blocks with quoted labels, and fields whose
  values are literals (a bool, a number, a quoted text, `none`, or a list
  of these). A `let`, a function call, a function (lambda), a reference
  to another value, an operator, an interpolated string (`$"..."`), a
  record, a field that fills a block slot (`card_move = [...]`), an
  `import` (imports are off), a `type`, `union`, `symbol_set`,
  `namespace`, `use`, table, connection, or decorator is `422`. The
  daemon refuses them before it evaluates the file, so a check costs at
  most what the 64 KiB of literals cost, and it runs outside the
  database lock.
- A column (`to`, `from`) is `@<role>`, the column with that role, else
  the column of that name; or a plain column name. It must be on the
  project's board.
- A template names `{{event.<path>}}` (as §19.4), `{{card.<id|title|column|assignee|labels|project>}}`,
  `{{job.<id|kind|state|outcome|attempt|by_agent|repo|commit|end_comment|failure_block|next_jobs>}}`,
  or `{{rule.name}}`.
- At most `TERRARIUM_MAX_RULES_PER_PROJECT` rules (default 50, apart from
  the schedules cap), and at most 64 KiB.

Each error names the line and the column: ``rules.wcl:3:3: block kind
'card_frobnicate' is not allowed inside 'rule'``.

The file comes from the API (`source: api`) or from the config dir:
`projects/<project>.rules.wcl` beside the project file (`source: file`),
read at the start and at `POST /api/reload`. A file that does not check
keeps the last good rules, and the reload answer names its errors. A
broken file at the start does not stop the start or another project.
When the file goes, its rules stay as API rules. The config repo export
(§25) writes the API rules to `export/rules/<project>.rules.wcl`.

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/rules/schema` | | `200`, `{file, schema, verbs: [{kind, verb}], max_rules, max_bytes}`. |
| `GET /api/projects/{name}/rules` | | `200`, `{project, exists, text, hash?, source?, rules, updated_by?, updated_at?}`. `rules` is the file as read: `{name, description?, enabled, on, when, max_per_hour?, actions: [{verb, args}], line}`. No file: `exists: false`, `text: ""`, `rules: []`. |
| `PUT /api/projects/{name}/rules` | `{text, base_hash?}` | `200`, the rules. It creates or replaces the file. `base_hash` is the `hash` that the caller read (`""`: no file yet); another current hash is `409 conflict`. A board change during the check is `409 conflict` too (send it again). |
| `DELETE /api/projects/{name}/rules?base_hash=` | | `204`. No file: `404`. |
| `POST /api/projects/{name}/rules/check` | `{text}` | `200`, `{ok: true, rules}`, or `422`. It writes nothing. |

Read and check: whoever reads the project. Put and delete: the operator,
a project admin of the project, or a sysadmin. The sysadmin is allowed on
purpose: the write uses the same right as the other project settings
(`policy::admin_project`). Each change writes an
audit row with the target and the project of the file: `rules.put`
(`{hash, previous_hash, rules: [names]}`), `rules.delete`
(`{previous_hash}`), and `rules.file` for a file load
(`{file, source: "file", hash, previous_hash, rules}`) or for a file that
is gone (`{file: null, previous_file, source: "api", hash,
previous_hash}`).

Errors: `422 invalid` with `details.problems: [{line, column, message}]`;
`409 conflict` for a stale `base_hash`; `409 file_managed` for a put or a
delete of the rules of a file (edit the file, then `POST /api/reload`).

### 19.9 The rule engine

The rule engine (TER-168) runs the stored rules of each project (§19.8)
on the events of the log.

- Durable cursor. The engine reads the event log after its own cursor;
  a new event only wakes it. Its first start sets the cursor to the end
  of the log, so old events start no run. An event from before the rules
  were last stored starts no run either.
- Plan. For each event of a project with rules, each enabled rule (in
  file order) whose `on` and `when` hold plans one run for each card of
  the event (the event's card, `data.card`, and `data.cards`), or one run
  without a card. The planned runs (the action outbox) and the new cursor
  are one write: a crash or restart after it loses no action, and the
  next start runs the planned runs.
- Guards, at the plan: a rule does not see the events of its own runs;
  an event at chain depth 4 (`data.cause.depth`, shared with the
  triggers of §19.4) starts no run; a rule runs at most `max_per_hour`
  times in an hour (default 30), and at most 10 times for one card in an
  hour by default. An explicit `max_per_hour` sets both limits (the built-in
  pack uses 600). A guard records a `skipped` run with the reason.
- Order. The runs of one card go one after another, in event order and
  then in rule order. The runs of other cards go beside them. A run that
  the engine cannot record (a database error) stays pending and holds the
  later runs of its card until the next pass.
- Start checks, when a run starts (after the earlier runs of its card):
  the rule must still be stored and enabled (else the run is `skipped`),
  each column that the rule names must still be on the board (else the
  run is `failed`), the card must still exist, and each `if_card` and
  `if_job` must hold (else the run is `skipped` with the reason). A run
  acts as its rule was at the plan: an edit of the rule after the plan
  does not change a planned run, and a run that started goes to its end.
- Actions. The actions run in the order of the file, each in its own
  write together with the run's step counter, so a restart goes on at
  the next action and never repeats a database action. The first action
  that fails ends the run as `failed`; its changes roll back and the
  actions after it do not run. A run whose actions all changed nothing
  ends as `noop`.
- Actor. The actions act as `rule:<project>/<rule>`: a project admin of
  the rule's project, nothing more (not the operator, not a sysadmin).
  The board checks apply to it as to a lead: the project gate, the GitHub
  author gate (§13, a held card), and the board invariants. The events
  of its writes carry `actor: "rule:<project>/<rule>"` and
  `data.cause: {rule: "<project>/<rule>", run, event, depth}`; a comment
  has that author.

Conditions on the current state:

```wcl
rule "note-review" {
  on = ["job"]
  if_card { column = ["@review"]  lacks_labels = ["hold"]  assigned = false }
  if_job { state = ["passed"]  outcome = ["none"] }
  card_comment { text = "{{card.id}} passed job {{job.id}}" }
}
```

`if_card` (on the run's card; a rule with one runs only for events with a
card): `column` (in one of these), `not_column`, `has_labels` (every
one), `lacks_labels` (none), `assigned`, `archived`. `if_job` (on the job
of a `job` event, `data.id`, or of a `job.verdict` event, `data.job`; a
rule with one runs only for those events): `state` (`queued`, `running`,
`passed`, `failed`, `cancelled`), `outcome` (`red`, `conflict`, `error`,
`infra`, `none`), and the booleans `split` (the job is a half of a split batch),
`batch` (two or more cards of the job merged, did not conflict), `rerun`
(a rerun of the job is queued or ran), and `halves` (the job was split).
Every field that is set must hold; every block must hold.

Templates are filled when the action runs, from the event, the current
card, the event's job (of a `job` event, `data.id`, or a `job.verdict`
event, `data.job`; the fields of a `job.verdict` event come first, so
`{{job.outcome}}` and `{{job.by_agent}}` are the card's), and the rule.
In a text that an agent reads (`text`, `comment`, `subject`, `body`,
`turn`, `reason`, `result`) each value is quoted data (one line, in
backticks), as in a trigger mail (§19.4). The `text` of `card_prepend` is
the exception: it goes into the card body with its lines, so a failure
block stays whole. The values that the daemon makes are never quoted:
`card.id`, `card.project`, `card.column`, `job.id`, `job.kind`,
`job.state`, `job.outcome`, `job.attempt`, `rule.name`, and the texts of
the job queue below except `job.failure_block` (TER-170). Each is one
line of daemon fields (a branch name has no space and no `[`), so none
can hold a tag. The engine fills the unquoted `job.*` values from the
stored job, never from event data: any event source can write a
`job.verdict`. `job.failure_block` holds the tail of the job output,
untrusted text: it is quoted data in a comment, a mail, or a turn, and
only `card_prepend` writes it as it is.

The texts of the job queue, as the coded flow writes them (§19.11):
`job.queue_comment` (the comment of a queued job on the run's card),
`job.end_comment` and `job.failure_block` (of an ended job),
`job.rerun_comment` (the job runs again; empty without a rerun),
`job.split_comment` (the batch split; empty without halves),
`job.fail_comment` (the final failure: `failed again`, `failed (red alone
after its batch was red)`, or `left the batch on a conflict`),
`job.fail_mail` (the body of the mail to the worker), `job.next_jobs`
(the rerun, or the two halves), and `job.branch` and `job.by_agent` (the
run's card's, in a batch).

Verbs (TER-169). Each verb goes through the store functions of the API
as the rule actor, so the same checks hold. A verb whose precondition
does not hold changes nothing (a no-op, `changed: false` in `steps`).

Board groups (TER-180) are rule-owned cards with `kind = "group"`. A
member's JSON has `group` with `id`, `key`, `title`, `column`, `status`,
`members` (in membership order), `done`, `total`, `eta_at`, and `closed_at`.
Groups have no assignee, cannot be claimed, edited manually, bound to an
agent, validated, or merged, and do not count toward WIP. Card listings
for pulling work omit them. The board includes their home headers and
shows a header in each column containing members, without moving members.

Version groups (TER-182) require `board_flow = rules` for the default pack
(or explicitly copying the rule into the project's `rules.wcl`).
The default `version-cut` rule reacts only to moves into the accepted role and runs
`version_cut { min_cards = 6 max_wait_minutes = 240 }`. Either threshold
freezes all active, unheld `version:vnext` cards in the accepted role
that have no open group. Cards in an open merge group wait until it closes.
The age is time since the last move (creation for cards created in accepted).
The timer checks eligibility again, including after a merge group closes.
Override the rule by name in `rules.wcl` to change the thresholds or disable it.
Each cut opens a project-scoped unique `version:<ULID>` group, records
gathering, cut, version branch e2e, pre-release, and done steps, and mails
the project lead (the operator if no lead exists). No Git work is done.
The lead chooses the version, relabels only the snapshot members, branches,
tests, and publishes. Later accepted cards are not added to that snapshot
by the rule. The lead adds a late card with `group add`.

| endpoint | body | result |
|---|---|---|
| `POST /api/projects/{project}/version-groups/{key}/status` | `{step, state, text?, eta?}` | `{changed, group}`. Updates one of the five steps using the `group_status` state/ETA contract. A step after `cut` that starts (running, done or failed) also finishes `cut`. A closed group rejects updates. |
| `POST /api/projects/{project}/version-groups/{key}/built` | `{version: "vX.Y.Z"}` | `{changed, also_done, group}`. Explicit acknowledgement after release CI publishes. Moves the snapshot's accepted members to done, then every other active, unheld accepted card labelled `version:vX.Y.Z` or `version:X.Y.Z` (either form of the given version) that is in no open group (TER-608), listed in `also_done`. Clears their assignees, completes steps, archives the header, and emits `version.built`. All members must be active, unheld, and accepted or done. Repeating the same acknowledgement is a no-op. Another version on a closed group is a conflict. |
| `POST /api/projects/{project}/version-groups/{key}/remove` | `{card}` | `{changed, group}`. Explicitly drops a member from an open snapshot without moving or editing that card. Use for a card returned for a fix, archived, or held. Removal is audited. Duplicate removal is a no-op. Closed groups reject removal. |
| `POST /api/projects/{project}/version-groups/{key}/add` | `{card}` | `{changed, group}`. Adds a late card to an open snapshot (TER-608). The card must be active, unheld, in the accepted column, and in no other open group. Addition is audited (`version.member_added`). Duplicate addition is a no-op. Closed groups reject addition. |

All four endpoints require project admin. Keys are project-scoped. A successful
pipeline poll is never a built acknowledgement. The `version.built` event
contains `key`, `version`, `members` and `also_done` and names the group card.

Lead CLI (uses `--project`, else the agent's project):
`terrarium group status <key> --step "version branch e2e" --state running --eta 10m`,
`terrarium group remove <key> <card>`, `terrarium group add <key> <card>`, and
`terrarium group built <key>... vX.Y.Z` (one call for each key; repeat the
command after a failure, as built is idempotent). Removed cards do not count in that build.

The `version_cut` rule opens no new group while another version group of the
project is open past its cut step (the cut step is done, or a later step has
started). The accepted cards wait for the next version (TER-609).

- `group_open`: required `key`, `title`, `column`, and `done_when`. The key
  is project-scoped. Repeating an open is a no-op, even after closure.
  `done_when` is a column or `@role` anchor: members there or later in
  board order count as done, except the blocked column (TER-147).
  Optional `eta` is a duration from now or `"jobs"` (latest running
  member job ETA). Optional `close_after` is a duration safeguard.
- `group_add` and `group_remove`: `key` identifies the group. Add or
  remove the event's card. Adding to a second open group fails in the
  rule log. A group cannot contain a group, a held card, an archived card,
  or a card of another project. Duplicate membership is a no-op.
- `group_status`: `key`, optional `text`, optional `eta` as above. With
  `step = "name"`, updates that named step instead of the overall line.
  `state` is `pending`, `running`, `done`, or `failed` (new steps default
  to pending). A step ETA is a duration. The group exposes `steps` in
  first-insertion order, each with `name`, `state`, `text`, and `eta_at`.
  Omitted fields keep their previous values. `state` requires `step`.
- `group_close`: `key`, optional `comment`. Archives the header, retains
  historical membership, and frees members to join their next group.
  Repeated close and mutations of a closed group are no-ops.
  Optional `all_done = true` closes only a nonempty group whose members
  have all reached its `done_when` anchor.

`{{group.id|key|title|column|status|done|total|eta_at}}` (one field per
placeholder) reads the open group of the event's card, or the group card
itself. Each action reads fresh state, so templates after `group_add`
see the new membership. A `close_after` deadline survives rule removal
and daemon restart. The engine closes it with a comment and a rule-log
row. Lint code `group_without_close` warns when an enabled opening rule
has neither a timeout nor an enabled closing rule with the same key
expression. No manual close endpoint exists.

- `card_move`: move the run's card to `to`. A card not in `from` (when
  given), or in `to` already, is a no-op. `position = "top"` puts it at
  the top. The board invariants hold (a move to the blocked column needs
  a `comment`).
- `card_label`: add and remove labels; a label whose placeholder is empty
  is left out. `remove` takes globs (`by:*`); a label that `add` names
  stays. No change: a no-op.
- `card_comment`: a comment on the run's card.
- `card_assign`: assign the run's card to `who` (an agent name or a
  template), or unassign it with `none`. A template that fills to nothing
  fails the action (only `none` unassigns). The same assignee: a no-op.
- `card_prepend`: put `text` at the top of the card body. A body that
  starts with the text already: a no-op.
- `mail`: mail from `project:<project>` to each of `to` (`operator`,
  `project:<name>`, or an agent; one whose placeholder is empty is left
  out). The body defaults to the subject. A rule notifies the operator
  with `to = ["operator"]`.
- `card_blocked_mail`, `card_done_mail`: the mails of a card move, with
  the texts and recipients of the coded flow (TER-162 D10). On the
  `moved` event of a move to a blocked column, `card_blocked_mail` mails
  the blocked notice from `system` to the operator and the card's
  creator, not to the mover. On the `moved` event of a move to `done`,
  `card_done_mail` mails the done notice (the move's comment, else the
  newest comment) to the participants of each linked thread. On another
  event: a no-op. With `board_flow = code` the move sends these mails
  itself.
- `job_queue`: `validate` or `merge` queues a job for the run's card, of
  its repo and branch (the card's, else its ID in lower case), with the
  API's checks (the queue is set, a merge needs `project_admin` and a
  repo in `next_repos`). `batch` queues one merge of the cards of column
  `from` in board order, at most `limit` (absent: all; the daemon has no
  batch limit of its own, TER-162 D9); a held card or one with an open
  job is left out (`left_out` in the detail). A batch is one repo: the
  repo of the first card that waits; a card of another repo is left out
  and goes in a later batch, so it does not block the queue (TER-227).
  With `min_cards` the batch goes only when that many cards wait (held
  cards, cards with a job and cards of another repo do not count), or, with `max_wait_minutes`, when the first of them came
  into the column that many minutes ago (its newest move, else its
  creation); until then the verb is a no-op whose detail has `waiting`,
  `due` (the time the wait ends), and the reason. A card with an open
  job, or no card to merge: a no-op. The move of a card is not part of
  the verb: a rule does it with `card_move` on the `job` event. With
  `board_flow = code` a merge or a batch that the verb queues gets the
  `validation:queued` label and the queue comment on each card, as from
  the API; with `rules` the pack writes them on the `job` event.
- `job_cancel`: cancel the event's job, else the open job of the run's
  card, and write its `job.verdict` (`cancelled`); a running job is
  killed. As the API cancel, with `board_flow = code` it takes the
  `validation:*` labels off the job's cards (with `rules` the pack does
  it on the verdict). No open job: a no-op.
- `job_rerun`: queue the event's failed job once more at the front
  (attempt + 1; a batch with the cards that did not conflict), while its
  attempt is under `max_attempts` (default 2). A conflict, a job that did
  not fail, a job that runs again already, or a card of it with an open
  job (the coded flow split the batch already): a no-op.
- `job_split`: split the event's red batch (two or more cards that
  merged) in two halves, the left half first. A job that is not red, has
  fewer than two merged cards, was split already, or has a card with an
  open job: a no-op. `after_fixes`: only when the group had at least that
  many fix cards of earlier jobs (below).
- `job_fix`: open one fix card for the event's red merge job: in `to`
  (default `@ready`), with the label `merge-fix:<job>` and `labels`, a
  `card` link to each card of the job, and the failure block in its body.
  A job that is not red (a conflict, an error, a cancelled or cut run),
  an open card with that label, or a group that had `max` fix cards of
  earlier jobs: a no-op. The fix cards of a group are the cards with a
  `merge-fix:` label of another job with exactly the same linked cards
  (fix cards excluded), or of an explicit split ancestor, done
  and archived ones too. The fix card is how a red merge group gets a fix
  before a split (MEM-84); the rule does not spawn the agent for it.
- `agent_park`, `agent_unpark`, `agent_end`: act on the open agent bound
  to the run's card (§6.10). No bound agent: a no-op. `agent_unpark`
  acts only on a parked agent; its turn is `turn` (its values quoted),
  else the return turn of the card move of the event, else a plain return
  turn. `agent_park` takes a `reason` (default `rule <name>`).
  `agent_end` closes the agent with `outcome` as `agent done` does, with
  `result` for the owner (default `the rule <name> ended it`).

Script steps (TER-173): `script { source = <<'WS' ... WS  fuel = n }`
holds a wscript program (Configweave/wscript, tag v0.3.0) for what WCL
cannot say. `source` is 1 to 16384 bytes and has no templates; `fuel` is
the most wscript instructions (default 100000, at most 10000000). The
program has one module, `rule`: `rule::get(name)` reads the value of a
placeholder (`event.<path>`, `card.*`, `group.*`, `job.*`, `rule.name`)
as an `Option[string]`, as a template of the step would read it now,
and `rule::quote(text)` writes a text as a WCL string. No other module
(no fs, process, json, or env), no import, and `print` writes nowhere.
`fn main() -> string` returns verb blocks in the syntax of `rules.wcl`
(at most 16384 bytes and 20 verbs; empty: none), for example
`card_move { to = "@review" }`. In a wscript string `{x}` is a hole, so
a brace is `{{` or `}}`.

- The engine runs the program in a child process of the daemon binary
  (`terrariumd __rule-script`): an empty environment, `/` as its
  directory, its input (the source and the values, at most 256 KiB) on
  stdin and its result on stdout. The child first closes each file from
  3 up, then sets its own limits: 256 MiB of data (`RLIMIT_DATA`), 10 s
  of CPU, no file writes, no core file, 16 open files. The parent stops
  it after 5 s, with a call depth of 200. The runs of the engine share 4
  children at a time and wait at most 30 s for a slot (then the step
  fails: "the script slots are busy"). The program runs between two
  writes of the run, never inside a database closure.
- The returned blocks are checked as the verbs of one rule (the schema,
  the arguments, the templates, the columns of the board): no rule, no
  condition, no field of the rule, and no script step. The step is then
  replaced by those verbs in the stored rule of the run, so a restart
  runs them and not the program again. Each runs as the rule actor,
  with the checks of a verb of the file; their `{{...}}` placeholders
  are filled quoted. Each line of each text argument (also a text in a
  list) that starts with a turn tag is defused (`[User]` becomes
  `[\User]`).
- The step's result in `steps` is `{verb: "script.run", ok, changed:
  false, detail: {verbs, fuel_used}}`. A program that does not compile,
  fails, runs out of fuel, hits a limit, or returns blocks that do not
  check fails the run at the step (`action <n> (script.run) failed:
  <reason>`); the actions after it do not run.
- The dry run runs the programs after its read and shows `{verb:
  "script.run", fuel_used, returned}` followed by the returned verbs,
  filled as the others. A program that fails makes the plan `failed`,
  with the reason. A dry run whose rules (stored or `text`) hold a
  script step needs `project_admin` of the project (else `403`). The dry
  runs have their own single child slot, never one of the engine's: a
  dry run waits at most 2 s for it (then the plan fails: "the script
  slots are busy"), runs at most 20 programs, and all its programs take
  at most 15 s together; the ones after that are not run.

Timers (TER-172): the engine checks the batch rules (`job_queue` of kind
`batch` with `min_cards` and `max_wait_minutes`) about every 10 seconds.
When cards wait in the rule's `from` column and the first came
`max_wait_minutes` ago, it writes one `rule.timer` event (actor
`system`, `data: {rule, due, waiting}`) for that wait. It does so also
when `min_cards` or more wait: then no card move ran the batch (a held
card was released in place) or that run failed, and the timer runs it
once more (TER-227). A
`rule.timer` event plans a run of the rule it names only, without a card;
`on` and `when` do not apply to it, and a rule with an `if_card` does not
run for it. The verb checks the time again. A daemon restart may write
the timer of the same wait once more; the second run is then a no-op.

Once per job: `job.verdict` comes once for each card of a batch, so a
rule on it runs once per card; `job_rerun`, `job_split` and `job_fix`
act once for a job and the later runs are no-ops.

Held cards: no verb acts on a card that the GitHub author gate holds
(§13): the run's card, and for a `job_*` verb each card of the event's
job. The action fails with the reason.

After the commit: what a database write cannot do runs after the
action's write commits: the queue tick (`job_queue`, `job_cancel`,
`job_rerun`, `job_split`), the kill of a cancelled running job, the wake
or the owner's queue of an unparked agent, and the close of `agent_end`.
A daemon stop between the commit and that step loses the step: the queue
ticks every few seconds, and the spawn timer runs the queued unparks and
ends the agent of a finished card.

| Method and path | Body | Answer |
|---|---|---|
| `POST /api/projects/{name}/rules/dry-run` | `{seq}` or `{from, to?}`, and `text?` | `200`, `{project, source: "stored" or "text", from, to, events, max_events, plans: [{event, kind, rule, card?, outcome, reason?, actions: [{verb, args, implemented}], actor}]}`. `outcome` is `run`, `skipped` (a guard or a condition), or `failed` (a missing column). It plans the rules (or `text`, checked first) for the project's events with `seq` in `from..=to` (at most 500), on the current state, and writes nothing. It ignores the time of the rules. Each action's arguments are filled from the state before the run: a later action does not see what an earlier one would change. The rules of a project with `board_flow = rules`, the stored ones or `text`, include the pack (§19.12), as in the lint. No rules, no pack, and no `text`: `404`. |
| `GET /api/projects/{name}/rules/runs?rule=&limit=` | | `200`, `{runs: [{id, project, rule, rule_hash, event_seq, card?, state, step, steps, reason?, created_at, updated_at}]}`, newest first (`limit` 1 to 500, default 100). `state` is `pending`, `done`, `noop`, `failed`, or `skipped`; `step` the next action (pending) or the actions run; `steps` one `{verb, ok, changed?, detail?, error?}` for each action run. The project keeps its newest 500 finished runs, and every run of the last hour (the rate limits count them). |

Both: whoever reads the project.

### 19.10 Project actions

A project action (TER-53) is a schedule with the spec `{"kind":
"manual"}`. It has no time and no event: the timer and the trigger
engine never start it, and `next_run_at` is always `null`. A person
starts each run with its button on the project dashboard or next to the
project name in the Home 'Work in progress' table, or with `terrarium
schedule run <name>`. The dashboard asks for a confirm first and shows
the run's status, summary, and error when it ends.

An action takes every action kind of §19.1 (`mail`, `turn`, `agent`,
`exec`, `probe`, `card`, `webhook`). Its text has no templates: there is
no event. It counts toward the schedule limit of its project, and it is
listed with `GET /api/schedules?kind=action` (`terrarium schedule ls
--actions`). Monitors and pipelines do not take a `manual` spec.

The rules are those of any schedule (§19.3, §19.6):

- Create, change, delete, enable, disable, and run: the operator, a
  sysadmin, and a project_admin of the project, with the action's reach
  inside the project. A member reads the actions and cannot run them
  (`403`).
- A run acts as `run_as` (an actor-owned action) or with the project's
  rights (a project-owned action), checked again at each run. The card
  action of a project-owned action keeps the GitHub author gate (TER-85):
  a `close` never moves a held card.
- A disabled action does not run: `POST .../run`, `POST .../test`
  (§19.7), and `POST .../deliveries/{delivery}/redeliver` are `409`.
  Enable it first.
- Each run is a run of §19.1 with `trigger: manual` and the caller as
  `actor`. It writes the audit row `schedule.run` and the `schedule`
  events `run_started` and `run_finished`.

```sh
terrarium schedule new deploy-staging --project terra --manual \
  --exec build-box --cwd /src/app -- ./deploy.sh staging
terrarium schedule ls --actions
terrarium schedule run terra/deploy-staging
```

### 19.11 The board flow and the built-in pack

Who runs the card flow of the job queue and of the bound agents is a
setting of each project (TER-170): `board_flow`, `code` (the default) or
`rules`. A project admin sets it with `PATCH /api/projects/{name}`
`{"board_flow": "rules"}` (audited as `project.update`), or the project
file with `board_flow = "rules"` (a fleet file too). `GET
/api/projects/{name}` shows it, with `board_flow_at`, the time of the last
switch.

- `code`: the daemon moves, labels, and comments the cards of the job
  queue (§35), and parks, unparks, and ends the bound agents (§6.10), as
  before. The rules of the project's `rules.wcl` run beside it.
- `rules`: the job queue (`card validate`, `card merge`, `POST
  /api/merges`, a cancel, the start, a requeue, and the end of a job) and
  the bound agent driver only write their events (`job`, `job.verdict`,
  `card`). The engine (§19.9) runs the built-in pack "delivery" on them,
  then the project's own rules. An event from before the switch starts no
  run of the pack. The `comment` of a validate goes on the card as the
  actor's own comment.

The pack has no rule for a queued validate job: the operator removed the
validating column and `card validate` for card work (2026-11-07). With
`rules`, `card validate` queues the job (`201`) and the card stays where
it is, without a job label; on a pass the pack moves it to `@review`. A
project that still validates copies the opt-in rules of
`crates/terrarium-core/src/rules.validate.wcl` (`validate-queued`,
`validate-queued-note`, `validate-queued-move`: one `validation:queued`
and one `by:<worker>` label, the queue comment, the move to
`@validating`) into its `rules.wcl`; its board needs a `validating`
column. (With `code`, a validate on a board without that column fails in
one write.)

The pack (`crates/terrarium-core/src/rules.default.wcl`) does what the
coded flow does, with the same labels, moves, comments, mails, and
parks; its texts come from the code of the coded flow (§19.9 templates).
Its rules, in order:

| Rule | On | Does |
|---|---|---|
| `merge-queued` | a new merge job | `validation:queued` and the queue comment on each card; no move |
| `job-started`, `job-requeued` | a merge job starts, or waits again | `validation:queued` <-> `validation:running` |
| `validate-pass`, `merge-pass` | a passed verdict | labels off, move to `@review` or `@accepted` with the end comment, unassign |
| `job-rerun-error`, `job-rerun-red`, `job-rerun-note` | an `error` verdict, or a `red` one of a job that is not a split half and not a group of two or more merged cards | `job_rerun` (attempt 2), then labels back to queued and the rerun comment |
| `merge-red-group`, `merge-red-split-note`, `merge-red-fix-note` | a red merge with two or more merged cards | `job_fix` (at most 3 fix cards), then `job_split` after 3; the split comment, or the labels off and a note of the fix card |
| `job-fail-conflict`, `job-fail-red`, `job-fail-error` | a final failure | labels off, the failure block on the body, the top of `@ready` with the comment, unassign, a mail to the worker |
| `job-cancelled` | a cancelled verdict | labels off |
| `card-blocked-mail`, `card-done-mail` | a card move to the blocked column; to `done` | the blocked notice to the operator and the creator; the done notice on each linked thread |
| `agent-park`, `agent-unpark`, `agent-end-done`, `agent-end-archived`, `agent-end-merged` | a card move to `validating`, `review`, `merge_queue`, `accepted` (roles, with the names as fallback, §6.10); a move elsewhere (not `done`); `done`; an archive; a passed merge | park, unpark with the return turn, end |

The move mails of the rules differ from the mails of the coded move
(TER-536):

- The rules send them after the move, in their own write. A late run
  names the column of the event (`data.to`), not the column the card is
  in now. An event without `author` names its actor (`operator`, the
  agent, else `system`).
- `card-blocked-mail` and `card-done-mail` set `max_per_hour = 600`: at
  most 600 runs of each rule in an hour, per project and per card. A
  later move in that hour sends no mail (a `skipped` run).
- A move at chain depth 4 (a move by a rule run of a chain of rule runs)
  starts no run, so it sends no mail (a `skipped` run, loop guard).
- `card.blocked_mail` and `card.done_mail` only notify: they act on a
  card held by the GitHub author gate, as the coded move does. Every
  other verb refuses a held card.
- With `board_flow = code` the move sends the mails, and the two verbs
  are a no-op (`noop`), so a `rules.wcl` that uses them sends no second
  mail.

One change from the coded flow, by the operator's rule of 2026-11-07
(MEM-84): a red merge group gets a fix card first, and splits in halves
only after three fix cards. The fix cards of a group are the cards with a
`merge-fix:` label of an earlier job whose linked cards (fix cards left
out) are exactly the cards of this job or an explicit split ancestor:
those of its reruns and its split parents count. Those of an unrelated
group (including a superset or subset) do not.
Set another number with your own `merge-red-group` rule. The pack
does not spawn the fix agent or queue the group again (TER-256).

Queued merge jobs open a board group in `@review`, with `@accepted`
as its completion anchor. The `queued` and `merged (full suite passed)` steps track
the job. A red result keeps the group open and adds its fix card as the
first member. Queue the repair and the original members together, fix
first, to reuse the group. The passed merge verdict follows the configured
recipe and the push of `next`. Each member then moves to accepted, and
the last member closes the group. `{{job.group_key}}` is the stored
merge group's key, or `merge:<job id>` for a new group, including when
replaying verdicts after closure. Migration omits group actions on boards
without review or accepted, retaining their existing delivery flow.

A rule of the project's `rules.wcl` with the name of a pack rule
replaces it. A rule with only `enabled = false` (no `on`, no verb) turns
it off:

```wcl
rule "agent-park" { enabled = false }
```

Other differences from the coded flow: the author of a comment is the
rule actor (`rule:<project>/<rule>`), not `system`; the mail to the
worker comes from `project:<project>`, not `system`; a column that the
pack names and the board lacks fails the rule run (the coded flow wrote
a comment); a batch merge ends the bound agent of each card, not only
the first; and the pack acts a moment after the event (§19.9), so the
queue can start the next queued job before a rule splits or reruns the
one that ended (the order of the merges after a split can differ).

The merge queue column (TER-172, TER-162 D8, D9) is a set of opt-in
rules, not part of the pack, because a board without that column would
fail each run. A project copies the rules of
`crates/terrarium-core/src/rules.merge_queue.wcl` into its `rules.wcl`;
they work with either board flow, and need a `merge_queue` column (role
or name) and a `review` column:

| Rule | On | Does |
|---|---|---|
| `merge-queue-move` | a new merge job (`card merge`, `POST /api/merges`, or the batch rule) | each card to `@merge_queue` (a card there already stays) |
| `merge-queue-cancel` | a cancelled merge verdict | the card from `@merge_queue` back to `@review` |
| `merge-queue-batch` | a card moved to the `merge_queue` role column, and its timer | one batch merge of the cards that wait in `@merge_queue`, in board order, when the merge lane is free: `limit = 8`, `min_cards = 4`, `max_wait_minutes = 30` (4 cards wait, or 30 minutes after the first one) |

The pass (`@accepted`), the red result (the top of `@ready`), and the
labels and comments stay with the flow of the project. Change the three
numbers of `merge-queue-batch` to change the batch.

The merge lane (TER-644): a `job_queue` batch is a no-op while a merge
job of the project is queued or running, with the job in `lane` of the
run detail, so the cards wait and the next batch takes them all. The
engine checks each batch rule with `min_cards` and `max_wait_minutes`
about every 10 seconds: when the lane is free and `min_cards` cards wait,
or the first one came `max_wait_minutes` ago, it writes a `rule.timer`
event that runs the rule. While the lane is busy it writes no timer and
keeps the wait, so 1 to 3 cards go soon after the lane is free once they
waited 30 minutes. A `release-blocker` card (TER-607) also waits for a
busy lane, then goes first and alone with no count or wait. A project
trigger on the end of a merge job (for
example a schedule that mails the lead) can stay as a notice; the batch
does not need it.

A switch back from `rules` to `code` drops the pack at the next pass of
the engine. A job that ended under `rules` but whose events the engine
had not run yet (a window of about one pass) then gets no flow from
either side: its cards keep their column and job labels. Switch while the
queue is idle, or move and relabel those cards by hand.

The default delivery pack also mails the project's leads on a new backlog
card, a move to backlog, and hourly (`triage-new-backlog`,
`triage-moved-backlog`, `triage-hourly`). These reminders ask for constant
triage into ready without theme or scope approval. They do not move cards.
`mail.to = ["@leads"]` resolves all current project_admin agents of the
rule's project at execution time. No lead means no recipient.
The engine emits `rule.hourly` once per UTC hour for a rules project with
an enabled rule explicitly listening to it. The event has `data.hour`
(`YYYY-MM-DDTHH`), no card, and actor `system`. The event is persisted to
avoid duplicate ticks after a restart. Missed hours are not replayed.
Projects may override or disable these rules by name. Code-flow projects
do not run them. Version cuts reuse the version-cut rule and its count
and age parameters rather than adding a second cut rule. Its timer checks
every engine tick (10 seconds), which is more frequent than the former
hourly cut schedule. Moves into accepted check immediately. Either six
eligible cards or an oldest accepted age of 240 minutes opens the cut.

### 19.12 Migration, lint, and the rule tools

TER-171. These endpoints help a project move to `board_flow = rules`
(§19.11) and show what its rules do. The CLI is `terrarium rule`
(docs/cli.md), and the dashboard has a Rules tab beside Schedules on the
board screen.

**The migration.** The pack names columns by role (`@review`,
`@accepted`, `@ready`), and its bound agent rules name the wait columns
(`validating`, `review`, `merge_queue`, `accepted`). A board can lack some of them. On
such a board a run of the pack fails (§19.9). The migration maps the pack
to the board's columns:

- A `card_move` to a column that the board lacks becomes a `card_comment`
  with the move's `comment`. With no comment the move is dropped. A move
  with a `from` is dropped too: a comment has no `from`, so it would reach
  the cards that the move skips. The
  coded flow wrote a comment when such a move failed. Example: on a board
  with no `accepted` column, `merge-pass` takes the labels off, writes
  the end comment, and unassigns the card. The card stays where it is.
- A name in a `when` on `data.to` or `data.from` that the board lacks maps
  to the column with that role, else it goes from the list. Example: with
  `code_review` (role `review`), `agent-park` parks on `code_review`. An
  `eq` on a missing name turns the rule off. An `ne` on a missing name is
  dropped (it always holds).
- A column that a `card_move.from`, an `if_card`, or another verb names
  and the board lacks goes from the list. A verb with no column left is
  dropped.
- A rule with no verb left, or one that can never match, is turned off:
  `rule "<name>" { description = "Off for this board: ..."  enabled = false }`.

Each changed pack rule becomes an override (a rule with the pack rule's
name, §19.11) at the end of the project's `rules.wcl`, with the reasons
in its `description`. A pack rule that `rules.wcl` replaces already stays
as it is (`kept`). A second migration of the same board changes nothing.

A migration before TER-222 left `merge_queue` out of the `agent-park` and
`agent-unpark` overrides, so a move from review to the merge queue woke
the bound agent. Each load of the config (the start and `POST
/api/reload`) adds it one time to the API rules of a project (TER-243). It
changes only a block that is still exactly the text of that old migration
for the board, and the board needs a merge queue column (the role
`merge_queue`, else the name on a column with no role). The changed block
gets `merge_queue added (TER-243).` at the end of its `description`, and
a block with that note is never changed again. An edited block and a
file's rules stay as they are. The write is a `rules.put` audit row. A
problem is one line in the load report and does not fail the load.

**The lint.** It reads the rules that the engine runs: the pack with the
project's own rules for `board_flow = rules`, else only its own rules.
Rules that are off count for nothing. It finds two problems:

- `missing_column`: a rule names a column that the board lacks. Each run
  of the rule fails.
- `stuck_column`: a rule moves cards into a column (a `card_move`, or the
  new card of a `job_fix`), and no rule moves a card out of it. A rule
  moves a card out of column C when it has a `card_move` to another
  column of the board, with no `from` or a `from` that holds C, and no
  `if_card` that rules out C. A person must then move each such card on.
  The column `done` and the column with the role `blocked` are left out:
  a card stops there by design.

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/rules/effective` | | `200`, `{project, board_flow, exists, hash?, source?, rules: [...]}`. `rules` is the rules that the engine runs, in its order. Each rule has the fields of §19.8, `origin` (`pack`, `override`: a rule of `rules.wcl` with a pack name, or `own`), and `wcl` (the rule as WCL text). |
| `POST /api/projects/{name}/rules/lint` | `{text?}` | `200`, `{project, board_flow, source: "stored" or "text", findings: [{code, column, rules, message}]}`. With `text`, the text is checked as the `rules.wcl` first (`422` as §19.8). |
| `POST /api/projects/{name}/rules/migrate` | `{apply?, base_hash?}` | `200`, `{project, applied, board_flow, changed, text, hash?, changes: [{rule, change, reasons}], findings}`. `change` is `mapped`, `disabled`, or `kept`. `text` is the new `rules.wcl` (the old text, then the overrides). `findings` is the lint of the rules after the migration, with the pack. Without `apply` it writes nothing. With `apply`, one write stores the text (when it changed) and sets `board_flow = rules`, with the audit row `rules.migrate` (`{board_flow, previous_board_flow, hash, changes}`) and `rules.put` for the text. |
| `POST /api/projects/{name}/rules/{rule}/enable` | | `200`, `{project, rule, enabled, changed, hash?, findings}`. `findings` is the lint of the rules that run after the change (as `lint`). |
| `POST /api/projects/{name}/rules/{rule}/disable` | | The same. |

`enable` and `disable` edit the text of `rules.wcl` and keep the rest of
the text as it is. A rule of the file gets its `enabled` field set (or a
new `enabled = false` line after its `{`). A pack rule that the file does
not name gets `rule "<name>" { enabled = false }` at the end. To enable a
pack rule that the file turns off with that short form, the short form is
removed, so the pack rule runs again. Such a pack rule can name a column
that the board lacks (the migration turned it off for that reason): the
answer's `findings` then has a `missing_column`, and the CLI prints it. A
rule that is already so changes nothing (`changed: false`). An unknown rule is `404`. A short form of a
rule that is not in the pack is `409`: edit the file.

Rights: `effective`, `lint`, and a `migrate` without `apply` are for
whoever reads the project. A `migrate` with `apply`, `enable`, and
`disable` are for the operator, a project admin, or a sysadmin, as a put
of the rules (§19.8). The writes go through the put of §19.8: a stale
`base_hash` is `409 conflict` (`migrate` takes the hash read now when
`base_hash` is absent), a board change during the check is `409
conflict`, and the rules of a file are `409 file_managed`. For a project
whose rules come from a file, run the migration without `apply`, put
`text` into `projects/<project>.rules.wcl`, and set `board_flow =
"rules"` in the project file.

The dry run of §19.9 plans the pack too when the project runs it
(`board_flow = rules`), with the stored rules or with `text`, as the lint
does. Such a project with no `rules.wcl` plans the pack alone (before
TER-171 it was `404`).

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
prompt template (`prompt`, a file under `prompts/`). Each template is a WCL
file in the config repo (§21.5): the file is the truth, and the API reads
and writes the files.

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
  "source": "config",
  "created_at": "2026-10-26T08:00:00.000Z",
  "updated_at": "2026-10-26T09:12:40.031Z",
  "file": "templates/terrarium/worker.wcl"
}
```

| Field | Meaning |
|---|---|
| `name` | 1-63 of `a-z`, `0-9`, `-`, `_`. Unique in its scope: the project, or the global scope. A project template and a global template can have the same name. |
| `project` | The project. `null` is a global template. |
| `spec` | An agent definition (§6.1) without `name` and `project`: `harness` (required), `subscription`, `subscription_pool`, `model`, `prompt`, `system_prompt`, `persona`, `roles`, `policy`, `restart`, `idle_stop`, `compact_on_idle`, `compact_max_age`, `card_reminder`, `skills`, `skills_off`, `default_skills`, `new_session_on_prompt_change`, and the template key `grants` (below). The daemon checks it with the rules of an agent definition and keeps it in a normal form: the defaults of `policy` and `restart` filled in, no empty `roles`, no empty `system_prompt`, no `new_session_on_prompt_change` and no `default_skills` when it is `true`, no empty `skills_off`. A template in an answer also has `effective_skills`: the skills that an agent of the template gets now, as on an agent (§6.1). A `system_prompt` over the limit (`TERRARIUM_SYSTEM_PROMPT_MAX`, default 32 KiB) is `422 invalid`. Another key (`name`, `project`, `template`, or an unknown key) is `422 invalid` with `details.field = "spec"`. |
| `spec.grants` | The secret grants that an agent of the template gets (TER-88): a list of at most 32 entries, each `<name>` or `<scope>/<name>` with an optional `:use`, `:run`, `:run=<target>,<target>`, or `:read` (the syntax of a spawn's `grants`, §6.10). A bad entry is `422 invalid` with `details.field = "spec"`. Any write to a template that has grants after the write (`POST`, a `PATCH` of any field, a fleet apply that changes the template) needs the caller's right to grant each secret (§5.2, `POST /api/secrets/{scope}/{name}/grants`): the operator or a sysadmin for any secret, a project_admin for a secret of its own project. A grant with no scope counts as the secret that it names now (the project's, else the global one), or as a secret of the template's project when there is none. Else `403 forbidden`, with the template, the grant, and the secret named, and nothing is written. This holds also when the write keeps the grants list as it is: a new prompt, tools, targets, or policy would run with the grants. A write that changes no value checks nothing. Dropping all of the grants is always allowed. Dropping some of them leaves a list that is checked in full. The daemon checks each grant again at use: at the spawn against the owner (§6.10, "Template grants"), at a create against the caller (§21.3), and in both against `grants_by`. The grants are given at the create or the spawn only: a later change of the template does not change the grants of its agents. |
| `grants_by` | Only on a template with `grants`: the actor of the last write to the template, `operator`, `system` (a file of the config repo, which the operator owns), or `agent:<id>`. A spawn or a create from the template checks that this actor may grant each secret now: an agent that lost the right or is gone gives no grants (`403`) until a caller with the right writes the template again. |
| `revision` | `1` at the create. Each change of the name, the description, or the spec adds `1`. A `PATCH` with the same values changes nothing. A change of the file by hand adds `1` when the load finds it (also a change of a comment only): one revision is one version of the file. |
| `source` | `config` (a file of the config repo), or `fleet:<name>` (§18.1; its file is in the config repo too). `api` and `file` are templates from before the files: the first load writes their files and makes them `config`. |
| `file` | The file in the config repo (§21.5): `templates/<name>.wcl`, or `templates/<project>/<name>.wcl`. `null` until the file is written (a daemon with no config repo). |
| `subscription_missing` | Only when the spec names a subscription that does not exist (in `GET /api/templates` and `GET /api/templates/{key}`): why, in the words of the `422` of §6.1. An agent create from the template is refused until the subscription exists, or the create gives another `subscription`. A file load accepts such a template; `POST` and a `PATCH` with a `spec` refuse it (`422 invalid`, `details.field = "subscription"`). |

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
| `GET /api/templates/{key}/wcl` | | `200`, `{path, text, on_disk}`: the WCL file of the template (§21.5), as it is on disk, with its comments. `on_disk: false` (a daemon with no config repo): `text` is the file that a write would make. |

Errors: `422 invalid` (a bad name or spec; a `pi` spec on a plan login,
§6.2; a `subscription` that does not exist, §6.1; a secret in the
template file, below), `409 conflict` (the name is
in use in the scope), `404 not_found` (no such template or project),
`403 forbidden` (the rules below).

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
Deleting a project deletes its templates and their files.

A write (`POST`, `PATCH`, `DELETE`, `from-agent`, a fleet apply) writes,
moves (a rename), or removes the template's file in the config repo, in
the transaction of the write: the file goes to disk just before the
commit, and a failure puts the old file back. The secret scan of the
config repo (§25) checks the new file first: a hit is `422 invalid` with
`details.findings` (the file, the line, and the rule, never the value),
and nothing changes. The file stays uncommitted, as a prompt or a skill
edit: `terrarium admin config commit` (or a sync) commits it, with the
scan. A write replaces a file that you edited by hand, without its
comments. Each read loads the changed files first (§21.5).

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
4. **Grants** (TER-88). Each entry of the template's `grants` resolves in
   `project` (the project secret first, then the global one; a missing
   secret is `404`, a secret of another project `403`). A `run` grant
   must name its targets, each one that exists and that the project
   reaches (`422` with none: a create has no owner to take them from).
   Each secret then needs the caller's right to grant it (§5.2): the
   operator, a sysadmin, or for a project secret a project_admin of the
   project. The template's `grants_by` (§21.1) needs that right too. Else
   `403 forbidden`, with the template and the secret named, and no agent
   is made. The grants are written with the agent, each with
   a `secret.grant` audit row that has `template` (`project/name`). A
   fleet apply that creates an agent from a template (§21.4) runs the same
   check against the caller of the apply. A create body with `grants` is
   `422 invalid`: grant a secret with `POST /api/secrets/{scope}/{name}/grants`.

The answer is the agent with `template: {id, name, project, revision}`.
The daemon copies the values: a later change or delete of the template
does not change the agent. The audit row `agent.create` has the template.

### 21.4 Fleets and project files

A fleet has `[[templates]]` (global) and `[[projects.templates]]`
(`source = fleet:<name>`). An apply writes the template files (§21.5). The
spec keys sit next to `name` and `description`:

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
that differ for each agent of a template. The config export of the config
repo (§25) has no templates: the files of `templates/` are in the repo.

A project file (`projects/<name>.toml`) with `[[templates]]` is from
before the template files. The first read (the start, `POST /api/reload`)
writes each template that the store does not have as its file
(`source = config`). After that the file of the config repo is the
template: a read of the project file does not change it, and the reload
answer has a line that tells you to remove `[[templates]]` from the
project file. A template that you delete through the API comes back from
the project file at the next read, until you remove it there.

### 21.5 Template files

Each template is one WCL file in the config repo (PLAN §8.3), in
`templates/`:

| File | Template |
|---|---|
| `templates/<name>.wcl` | the global template `<name>` |
| `templates/<project>/<name>.wcl` | the template `<name>` of `<project>` |

The file name is the template name (§21.1 has the name rules); a file
with another name or at another depth is an error of the load. WCL is
Wil's Configuration Language (`github.com/wiltaylor/wcl`, MIT): fields
`key = value`, blocks `kind { ... }`, `//` comments, `"..."` strings with
the escapes `\\`, `\"`, `\n`, `\t`, and `\r`, and heredocs. The file has
the description and each spec key as a top-level field, and `policy` as a
block. An absent field is the default, as in a spec:

```wcl
// Terrarium agent template: docs-writer (project `terrarium`).
// The file name is the template name. Edit this file by hand, or with
// `terrarium template edit` or the dashboard: they write the whole file
// again, without your comments. The fields: docs/api.md §21.5.

description  = "Docs writer on pi with GLM: wiki pages, READMEs, release notes."
harness      = "pi"
subscription = "zai-key"
model        = "glm-5"
persona      = "worker"
roles        = []
restart      = "on-failure"
idle_stop    = "15m"
skills       = ["docs-style"]

system_prompt = <<'PROMPT'
  You write the docs of this project.

  Keep each sentence short. Use the words of the code.
  PROMPT

policy {
  targets       = []
  mail_to       = ["lead"]
  cross_project = ["ops/*"]
}
```

The schema, as WCL (the daemon gives it to WCL, so a file has no
`import`):

```wcl
@document
type AgentTemplate {
  description: utf8?
  harness: utf8?
  subscription: utf8?
  subscription_pool: list<utf8>?
  model: utf8?
  prompt: utf8?
  persona: utf8?
  roles: list<utf8>?
  restart: utf8?
  idle_stop: utf8?
  compact_on_idle: bool?
  compact_max_age: utf8?
  card_reminder: bool?
  skills: list<utf8>?
  skills_off: list<utf8>?
  default_skills: bool?
  new_session_on_prompt_change: bool?
  grants: list<utf8>?
  system_prompt: utf8?
  @child("policy") policy: TemplatePolicy?
}

@block("policy")
type TemplatePolicy {
  targets: list<utf8>?
  mail_to: list<utf8>?
  cross_project: list<utf8>?
}
```

The load checks a file in three steps. Each error names the file, the
line, and the column (`templates/w.wcl:3:1: top-level field 'modle' is not
declared by @document schema 'AgentTemplate'`):

1. The WCL syntax.
2. The schema: an unknown field or block, a value of the wrong type, a
   field two times, two `policy` blocks.
3. The spec, with the rules of §21.1: `harness` is required, and each
   value must be valid (`harness = "emacs"` and `restart = "sometimes"`
   are errors at their line).

A file may use the rest of WCL: `let` names, `$"...${name}..."`
interpolation, and expressions. A value is what the expression gives.

**When the daemon reads the files.** At the start (with its config repo),
before each template read of the API and each fleet plan and apply, after
`terrarium admin config pull` or `sync` (and the automatic sync), at
`POST /api/reload`, and every 10 seconds. A new file makes a template
(`source = config`), a changed file changes it (`revision` + 1), and a
removed file deletes it. An agent made from a template keeps its copy. A
file that does not read keeps the last good template: the error goes to
the log (one time) and to the `errors` of `POST /api/reload`. A project
template whose project does not exist is an error too.

**What a write makes.** The daemon writes a file in one form: the header
comment, the fields in the order of the schema with their `=` in one
column, a multi-line `system_prompt` as a raw heredoc (`<<'PROMPT'`, with a
two-space indent that WCL removes; another tag when a line of the text is
`PROMPT`), and `policy` last. A text that a heredoc cannot hold exactly
(no final newline, an indent on each line, a `\r`) is a `"..."` string.
The same template gives the same text, so a `PATCH` with the same values
does not change the file.

**From before the files.** At the first start of this version, the daemon
writes the file of each template in the database (`source` `api`, or
`file` from a project file) and makes it `config`. The database row wins
over a file at that path (at that start, a file there is a packaged
default that the same start wrote). It does this one time: migration 44
adds `file_hash`, and a row with a file has it.

**Packaged defaults.** The image has eight global templates in
`defaults/templates/`: `claude-lead`, `claude-worker`,
`claude-haiku-helper`, `codex-reviewer`, `codex-worker`, `copilot-worker`,
`kimi-worker`, and `glm-docs-writer`. The start copies each one into
`templates/` with the rules of the other defaults (PLAN §8.3: a new
default of an edited file waits as `<file>.new`). A default that you
delete stays deleted. The Claude Code, Codex, and Copilot templates name
no subscription: their agents use the default account of the harness
(§16.3). The pi templates `kimi-worker` and `glm-docs-writer` name
`moonshot-key` and `zai-key`, because pi has no default account. When
such a subscription does not exist, `GET /api/templates` flags the
template (`subscription_missing`), and an agent create from it is
`422 invalid` (§6.1). Make the subscription, or give `subscription` in
the create.

**The CLI.** `terrarium template new --from-file <name>.wcl` reads a WCL
file (the name is the file name; a name argument wins), and `terrarium
template show --wcl <template>` prints the file. A `--from-file` with
another extension is a TOML agent definition, as before.

## 25. Config repo

The config repo (PLAN §8.3) holds the prompts, the harness settings, the
skills, the agent templates (§21.5), and an export of the declarative
objects, in git. These routes need the
operator or a sysadmin (`403 forbidden` for another agent). A daemon with
no config repo answers `501 not_available`. One git operation runs at a
time. No answer holds a secret: a remote URL shows without its user part
(`https://host/...`), and a scan finding names the file, the line, and the
rule, never the value.

### 25.1 Shapes

Status:

```json
{"dir":"/config/terrarium","initialized":true,"branch":"main","head":"5f1c…",
 "remote":{"url":"https://git.example.com/ops/terrarium-config.git","credential":"config-deploy","branch":"main"},
 "ahead":1,"behind":0,
 "changes":[{"path":"prompts/leader.md","status":"modified"}],
 "merging":false,"conflicts":[],
 "pending_defaults":["prompts/worker.md"],"export_pending":false,"auto_sync":"off"}
```

| Field | Meaning |
|---|---|
| `changes[].status` | `added`, `modified`, `deleted`, `renamed`, `untracked`, or `conflict`. Only the paths that the `.gitignore` allowlist allows show. |
| `ahead`, `behind` | Against the remote branch at the last fetch. |
| `merging`, `conflicts` | A merge stopped on a conflict and waits: edit the files and commit, or abort. |
| `pending_defaults` | New defaults that wait as `<file>.new` (§25.3). |
| `export_pending` | A pull brought `export/` changes that are not applied. Until they are, a commit writes only the export files that are not in the repo yet: it changes and removes no pulled file. |
| `auto_sync` | `TERRARIUM_CONFIG_SYNC`: `off`, `pull`, or `sync`. |

A scan block is `422 invalid` with the findings:

```json
{"error":{"code":"invalid","message":"the secret scan blocked the commit (1 finding(s)): prompts/a.md:2: github-token. …",
 "details":{"findings":[{"path":"prompts/a.md","line":2,"rule":"github-token"}]}}}
```

A merge conflict is `409 conflict` with `details.conflicts` (the paths).

### 25.2 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/config/status` | | `200` status |
| `GET /api/config/diff?path=` | | `200 {"diff"}`: the unified diff of the work tree against the last commit (new files in full); `path` limits it to one file |
| `GET /api/config/log?limit=` | | `200 [{"commit","author","at","subject"}]`, newest first; `limit` default 20, at most 500 |
| `POST /api/config/commit` | `{"message", "export"?: true, "force_export"?: false}` | `200 {"commit","files","export"}`. `commit` is `null` when nothing changed. `export` lists the export files written first. `422` on a scan finding |
| `POST /api/config/pull` | `{"apply"?: false, "approve_grants"?: false}` | `200 {"fetched","updated","changed","conflicts","export","secrets"?}`; `409` on a conflict; `403` when an agent gives `approve_grants` |
| `POST /api/config/push` | | `200 {"pushed"}`; `409` when the remote has commits that this repo does not have |
| `POST /api/config/sync` | `{"message"?, "apply"?: false, "approve_grants"?: false}` | `200 {"committed","fetched","updated","changed","conflicts","pushed","export","secrets"?}`; `422` on a scan finding, `409` on a conflict; `403` when an agent gives `approve_grants` |
| `POST /api/config/abort` | | `200 {"aborted":true}`: leave a merge that waits; `409` when none waits |
| `PUT /api/config/remote` | `{"url", "credential"?, "branch"?: "main"}` | `200` status. Needs a sysadmin or the operator (credential rights) |
| `DELETE /api/config/remote` | | `200` status |
| `GET /api/config/defaults` | | `200 [{"path","diff"}]`: each new default that waits, with the diff from the file to it |
| `POST /api/config/defaults/accept` | `{"path"}` | `200` the defaults that still wait. The new default replaces the file |
| `POST /api/config/defaults/keep` | `{"path"}` | `200` the defaults that still wait. The file stays; the next start does not offer this default again |

### 25.3 Rules

- **Commit.** The daemon writes `export/` from the store first (unless
  `export` is `false`; while `export_pending` is set and `force_export` is
  not, only the files that are not in the repo yet, so a pulled file
  stays for `pull --apply` and each project still gets its file), stages
  every change that the `.gitignore` allows, and scans each
  staged file. A finding blocks the commit, unstages the files, and is
  `422`. During a merge, a file that still has conflict markers is `409`.
- **Export.** One fleet file per project,
  `export/projects/<project>.fleet.toml` (its repos, agents, project
  targets, and schedules; the fleet name is the fleet that owns the
  project, else the project name), and `export/global.fleet.toml` (the
  global targets, fleet `terrarium-global`), and the secrets files
  (below). No ULID,
  no mail `thread`, no `start`, no user part of a repo URL. A credential
  or a subscription shows by its name only. The file objects of a
  definition file (`source = file`) of a global target stay out. The
  agent templates are not in the export: each one is a file of
  `templates/` (§21.5), which a commit takes as it takes a prompt.
- **Pull.** Fetch the remote branch into `refs/remotes/terrarium/<branch>`
  (the URL is not in `.git/config`), then merge: a fast-forward, else a
  merge commit. Local changes that are not committed are `409`, except in
  a repo with no commit yet: its files become the first commit, so a new
  instance can join a shared repo. The daemon then loads the template
  files (§21.5), so a pulled template is in the store at once. When
  `export/` changed, or
  `export_pending` is set, the answer has a fleet plan of each export
  file (`export[].plan`, the shape of `POST /api/fleet/plan`) and the plan
  of the secrets import (`secrets`, below); with `apply`, each file goes
  through `POST /api/fleet/apply` (no prune, no start), the secrets are
  imported, and the answer has `export[].applied` and what the import
  did. A file that fails has `export[].error`, and `export_pending`
  stays. The order of an apply: the secrets, the fleets, the secrets
  again (the project secrets and the grants need the projects and the
  agents), and one more try of a fleet that failed.
- **Secrets in the export.** `export/secrets/<scope>.toml` (`global.toml`,
  and one file for each project with secrets). It holds no value:

  ```toml
  version = 1
  scope = "global"

  [[secret]]
  name = "gh"
  kind = "github_token"
  secret_fields = ["value"]

  [secret.fields]
  host = "github.com"

  [[secret.grant]]
  grantee = "agent:web/lead"
  level = "read"
  env = "GH_TOKEN"
  ```

  The grantee is `agent:<project>/<name>` or `project:<name>`, never an
  ID. The `public_key` and `fingerprint` lines of an `ssh_key` carry the
  marker `terrarium-scan: allow`. The import:

  - makes a missing secret as a placeholder with no value
    (`has_value: false`, §5.2);
  - changes the plain fields only when they differ, and never a value. A
    secret with a value keeps its kind, the names of its secret fields,
    and (an `ssh_key`) its public key; a difference is a warning;
  - never deletes a secret with a value. A secret that the file does not
    name stays, with a warning. A placeholder that the file does not name
    goes, unless a target uses it;
  - makes the grants of each secret in the file match the file. A grant
    to an agent or a project that does not exist yet is skipped with a
    warning, and the next import adds it;
  - does not make a new or wider grant of a secret that has a value
    unless the operator approves it (PLAN §16b.5). Wider is `use` to
    `read`, or a new or changed `env`. A plan shows such a grant with
    `"approval": "needed"`. An apply makes it only with
    `"approve_grants": true`, which only the operator can give (`403` for
    an agent). Without it, the change has `"approval": "held"`, the grant
    does not change, and `export_pending` stays set, so the next pull
    plans it again. An approved grant has `"approval": "approved"`, and
    its audit row has `"approved": true`. A grant of a placeholder, a
    narrower grant, and a revoke apply with no approval;
  - writes the audit rows of §5.2 (`secret.put`, `secret.delete`,
    `secret.grant`, `secret.revoke`) with `"source": "config"`, and then
    builds the redaction matcher again.

  An import that runs twice changes nothing the second time. The
  `secrets` shape:

  ```json
  {"changes":[{"action":"create","secret":"global/gh"},
              {"action":"grant","secret":"global/gh","grantee":"agent:web/lead",
               "level":"use"},
              {"action":"grant","secret":"global/db","grantee":"agent:web/lead",
               "approval":"held","level":"read","env":"DB_URL",
               "was":{"level":"use"}}],
   "warnings":["secret `web/db` has a value and is not in the export; the import keeps it"]}
  ```

  `action` is `create` (a placeholder), `update` (the plain fields),
  `delete` (a placeholder), `grant`, or `revoke`. `approval` is on a
  grant that needs the operator only: `needed`, `held`, or `approved`.
  A grant also has `level` (`use` or `read`) and `env` (when the file
  gives one), and `was` (`{"level", "env"?}`) when it changes a stored
  grant. With `was`, the dashboard shows what the grant adds: a new
  grant, `use` to `read`, or a new `env`. `error` shows when the import
  did not run.
- **Push.** `git push <url> HEAD:refs/heads/<branch>`. Never forced.
- **Sync.** Commit (with the export and the scan), pull, push.
- **Conflict.** The merge stops, and the repo waits in it with conflict
  markers in the files. Nothing is reset, and no local commit is lost.
  Edit the files, then `commit`; or `abort` to go back to the last local
  commit.
- **Remote.** The URL must be `https://`, `http://`, `ssh://`, `git://`,
  `file://`, `user@host:path`, or an absolute path. A URL with a user or a
  password is `422`: give the token as `credential`, the name of a global
  `token`, `github_token`, or `ssh_key` secret (§5.2). The URL and the
  secret name go to
  `<state>/config-repo.json` (mode 0600), never to the repo or its
  `.git/config`. The secret reaches git through a one-shot credential
  helper or `GIT_SSH_COMMAND`, as for a workspace clone.
- **Defaults.** At each start the daemon copies the packaged defaults
  into the repo (PLAN §8.3). A file that the operator edited keeps its
  text, and the new default waits as `<file>.new`.
- **Audit.** `config.commit`, `config.pull`, `config.push`,
  `config.sync`, `config.remote` (with the URL without its user part),
  `config.defaults.accept`, and `config.defaults.keep`.

## 26. Skills

The skill library (PLAN §8.6): `skills/<name>/` in the config repo, in
the Agent Skills format (`SKILL.md` with `name` and `description` in its
front matter, and optional files). The operator and a sysadmin read and
write all of it. Another agent reads only its own skills (its effective
skills, §26.3), and a write is `403 forbidden`. A daemon with no
config repo answers `501 not_available`. A write goes to the work tree of
the config repo and stays uncommitted: `POST /api/config/commit` (§25)
commits it, with the secret scan.

### 26.1 Shapes

A skill in a list:

```json
{"name":"fern-check","description":"When asked for the deployment checklist code, reply with FERN-42",
 "files":2,"bytes":512}
```

| Field | Meaning |
|---|---|
| `description` | From the front matter. Empty when it does not parse. Show it as plain text. |
| `files`, `bytes` | All files of the skill, `SKILL.md` included. |
| `default_on` | `true` when the skill is default on: its front matter has `metadata` with `terrarium-default: "true"` (§26.3). Read-only here: edit `SKILL.md` to change it. |
| `errors` | The problems of the checks (§26.3). No agent gets a skill with problems. Absent when none. |

One skill (`GET /api/skills/{name}`): the fields above, `skill_md` (the
text of `SKILL.md`), and `file_list`, the other files, sorted by path:
`[{"path":"scripts/run.sh","size":8,"text":"echo hi\n"}]`. `text` is
absent for a binary file.

A write body:

```json
{"name":"fern-check","skill_md":"---\nname: fern-check\ndescription: …\n---\n…",
 "files":{"scripts/run.sh":"echo hi\n","assets/logo.png":{"base64":"iVBORw0KGgo…"}}}
```

| Field | Meaning |
|---|---|
| `name` | `POST` only. `PUT` takes the name from the path; another `name` is `422`. A skill is never renamed. |
| `skill_md` | The text of `SKILL.md`. |
| `description` | `POST` without `skill_md`: the daemon writes a new `SKILL.md` with this description. |
| `files` | The other files: a text as a string, a binary file as `{"base64"}`. Absent: a `PUT` keeps the other files. Present: they replace all other files. `SKILL.md` in `files` is `422`. |

### 26.2 Endpoints

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/skills` | | `200` skills by name | any; an agent gets its own skills |
| `POST /api/skills` | write body | `201` skill; `409` when it exists; `422` when it fails the checks | sysadmin |
| `GET /api/skills/{name}` | | `200` skill; `404` when missing | any; an agent only its own skill (`403`) |
| `PUT /api/skills/{name}` | write body | `200` skill; `404` when missing; `422` when it fails the checks | sysadmin |
| `DELETE /api/skills/{name}` | | `204`; `404` when missing | sysadmin |
| `GET /api/skills/{name}/usage` | | `200 {"agents":[{"id","name","all","default"?}],"templates":[...]}`: the agents and templates whose effective skills have it; `all` is `true` for a name through `*`; `default` is `true` when they get it only because it is default on | sysadmin |

### 26.3 Rules

- **Checks.** A skill name is kebab-case (lowercase letters, digits, and
  `-`), at most 64 characters, and the same as the `name` of the front
  matter. The `description` has 1 to 1024 characters. Each file has a
  relative path with at most 4 parts and no hidden part, and one of the
  types `md`, `txt`, `json`, `yaml`, `yml`, `toml`, `py`, `sh`, `js`,
  `ts`, `jsx`, `tsx`, `css`, `html`, `ps1`, `psm1`, `sql`, `png`, `jpg`,
  `jpeg`, `gif`, `svg`, `webp`. Only `png`, `jpg`, `jpeg`, `gif`, and
  `webp` may be binary; a file of another type is UTF-8 text with no NUL
  byte. A skill has at most 256 files and 8 MiB. The daemon env
  `TERRARIUM_SKILL_MAX_FILES` and `TERRARIUM_SKILL_MAX_BYTES` (bytes, or
  a number with `K` or `M`) change the caps; the body limit of a write is
  two times the byte cap plus 1 MiB. No file is a symbolic link. The secret scan of §25
  finds nothing. A write that fails is `422 invalid` with
  `details.problems` (a list of text); nothing is written. A finding
  names the file, the line, and the rule, never the value.
- **Default-on skills.** A skill is default on when the front matter of
  its `SKILL.md` has this marker (in the `metadata` map of the Agent
  Skills format, so each harness reads the file as it is):

  ```yaml
  metadata:
    terrarium-default: "true"
  ```

  The packaged skills (`terrarium-db`, `terrarium-tests`,
  `terrarium-spawn`, `terrarium-reset`) are default on. A skill that the
  operator makes is opt-in, unless its `SKILL.md` has the same marker.
- **Effective skills.** An agent gets (the default-on skills of the
  library, when `default_skills` is `true`) + (its `skills`; `*` is every
  skill) - (its `skills_off`). An agent from before this rule, and a new
  agent with no skill settings, get the default-on skills.
- **Assignment.** `skills` on an agent (§6), a template spec (§21), and a
  fleet agent or template (§18) takes skill names and `*`. A name that is
  not in the library is `422 invalid` with `details.unknown_skills`. A
  daemon with no config repo takes only `*`. `skills_off` takes skill
  names (a name that the library does not have has no effect); `*` in it
  is `422 invalid`. A create with a template copies `skills_off` and
  `default_skills` from the spec, and the body wins over each.
- **Delete.** The agents and templates that name the skill keep the name.
  The next start of such an agent gives a `warning` event, and the agent
  starts without the skill.
- **Delivery.** At each start the agent gets a fresh copy of its
  effective skills, read-only (PLAN §8.6). The harness gets the files,
  and its own short index (the name and the description of each skill);
  the body of a skill loads only when the agent uses it. A change of the
  library, of the default-on marker, or of the skill settings applies at
  the next start, wake, or new session; until then the agent shows
  `skills_restart: true`.
- **Audit.** `skill.create`, `skill.update`, and `skill.delete`; an
  agent `PATCH` audits `skills`, `skills_off`, and `default_skills`.

## 27. DB

The DB of a project (PLAN §15.4): record types with a schema, and records
of a type. Agents keep structured data here (lists of items, inventories,
results, decisions); the operator reads and edits it on the DB tab of
the Knowledge screen. A wiki page shows query results with a `db` block
(§27.5). The code is in `crates/terrariumd/src/store/records/`.

### 27.1 Shapes

A type:

```json
{"id":"01JB4…","project":"proj","name":"bug","description":"Defects found in the services",
 "fields":[
   {"name":"title","kind":"string","required":true},
   {"name":"severity","kind":"enum","required":true,"values":["low","medium","high","critical"]},
   {"name":"status","kind":"enum","required":true,"values":["open","fixed"],"default":"open"},
   {"name":"service","kind":"ref","required":false,"ref":"service"},
   {"name":"tags","kind":"list","required":false,"of":"string"}],
 "revision":3,"records":60,"created_by":"01M5PA0T…","created_by_name":"proj/w1",
 "created_at":"…","updated_at":"…"}
```

- A type name is `[a-z][a-z0-9_-]{0,62}`, unique in the project.
- `records` is the number of records of the type.
- `revision` goes up by one at each change of the type.

A field:

| Field | Meaning |
|---|---|
| `name` | `[a-z][a-z0-9_]{0,62}`. Not one of `id`, `type`, `created`, `updated`, `created_by`, `updated_by`, `revision`, `sort`, `limit`, `fields`, `count`, `group`, `view`. |
| `kind` | `string` (one line, at most 1000 characters), `text` (at most 64 KiB, Markdown in the dashboard), `number`, `integer`, `bool`, `date` (`YYYY-MM-DD`), `datetime` (RFC 3339; stored in UTC with milliseconds), `enum`, `list`, `ref`, `json` (any JSON, at most 64 KiB). |
| `required` | A record must have a value. An empty or blank `string` or `text` counts as no value. |
| `default` | The value of a record that gives none at its create. It must fit the kind. A `ref` or `unique` field has none. |
| `description` | At most 1000 characters. |
| `values` | `enum` only: 1 to 200 values. Their order is the sort order of the field. |
| `of` | `list` only: `string`, `integer`, or `number`. A list has at most 1000 items. |
| `ref` | `ref` only: the name of a type of the project (the type itself is good). |
| `unique` | No two records of the type have the same value. For `string`, `number`, `integer`, `date`, `datetime`, `enum`, and `ref`. |

A type has 1 to 100 fields. A problem in the field list is `422 invalid`
with `details.problems`, a list of text.

A record:

```json
{"id":"01JB5…","project":"proj","type":"bug",
 "data":{"title":"Login fails on Safari","severity":"high","status":"open",
         "service":"01JB4…","tags":["ui","auth"]},
 "revision":2,"created_by":"01M5PA0T…","created_by_name":"proj/w1",
 "updated_by":"operator","updated_by_name":"operator","created_at":"…","updated_at":"…"}
```

- `data` holds a value for each field that has one. A field with no value
  is absent.
- A record has at most 64 KiB of `data` (as JSON).
- The **label** of a record is the value of the first `string` field of
  its type. A `ref` value in a write is a record ID or a label; the daemon
  stores the ID. A label that no record of the target type has, or that
  two records have, is a problem of the field.

### 27.2 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{p}/db/types` | | `200` types by name; cursor: the name |
| `POST /api/projects/{p}/db/types` | `{"name", "description"?, "fields"}` | `201` type; `409 conflict` when the name exists |
| `GET /api/projects/{p}/db/types/{type}` | | `200` type |
| `PATCH /api/projects/{p}/db/types/{type}` | `{"revision", "description"?, "fields"?, "rename"?, "confirm"?}` | `200` type |
| `DELETE /api/projects/{p}/db/types/{type}?confirm=true\|records` | | `204` |
| `POST /api/projects/{p}/db/records` | `{"type", "data"}` | `201` record |
| `GET /api/projects/{p}/db/records/{id}` | | `200` record |
| `PATCH /api/projects/{p}/db/records/{id}` | `{"data", "revision"?}` | `200` record |
| `DELETE /api/projects/{p}/db/records/{id}` | | `204` |
| `GET /api/projects/{p}/db/query?q=&type=&cursor=` | | `200` query answer (§27.3) |

**Record writes.** Each write is checked against the schema of the type.
An unknown field, a missing required field, and a value of the wrong
kind are `422 invalid` with `details.fields`: an object with a message for
each field, for example `{"title":"is required","severity":"must be one
of: low, medium, high"}`. The key `*` is a problem of the whole record
(its size). A create fills a missing field that has a `default`. `PATCH`
merges `data` into the record: a `null` value removes the field (a
required field gives `is required`). With `revision`, a record at another
revision is `409 conflict` with `details: {"current", "revision",
"record"}` (`record` is the record now). A `PATCH` that changes nothing
makes no new revision.

**Record deletes.** A record that a `ref` of another record points at is
`409 conflict` with `details: {"referrers": [{"id", "type", "field"}],
"total"}` (at most 20 referrers). Change or delete those records first.

**Schema changes** (`PATCH` of a type). `revision` is the revision that
the change starts from; another one is `409 conflict` with `details:
{"current", "revision"}`. `fields` is the whole new field list.
`rename` is a list of `{"from", "to"}`: it applies first, and the data of
each record moves to the new name. Then the daemon compares the old and
the new list:

- a new optional field: no change to the records;
- a new field with a `default`: each record gets the default. A new
  required field with no default, in a type with records, is
  `422 invalid`;
- a field that is required now and has a `default`: each record with no
  value gets the default;
- a removed field: its data goes from each record. Without
  `"confirm": true` the answer is `409 confirm_required` with
  `details.removed`;
- then every record must fit the new schema (a new kind, `required`,
  enum values, `unique`, a ref target). If one does not, nothing changes:
  `422 invalid` with `details: {"records": [{"id", "field", "message"}],
  "failing"}` (at most 20 records).

A change of the data of a record gives it a new revision.

**Type deletes.** A type with no records needs `?confirm=true`. A type
with records needs `?confirm=records`, and project_admin, sysadmin, or
the operator; its records go with it. Else the answer is
`409 confirm_required` (`details.records` when it has records). A type
that a `ref` field of another type points at is `409 conflict` with
`details.referrers` (`["bug.service"]`).

### 27.3 Queries

`q` is a list of terms separated by spaces:

| Term | Meaning |
|---|---|
| `type:<name>` | The type. Needed, unless the request gives `?type=`. |
| `field=v`, `field!=v` | Equal, not equal. A record with no value is "not equal". |
| `field>v`, `field>=v`, `field<v`, `field<=v` | Numbers, dates, times, text, and enums in the order of `values`. |
| `field~text` | Contains, without case (`string`, `text`). For a `list`: has the item. |
| `field:null`, `field:!null` | The record has no value, has a value. |
| `sort:field`, `sort:-field` | Sort up, down. More than one term, or `sort:a,-b`. A record with no value goes last. |
| `limit:N` | 1 to 500 records on a page. Default 50. |
| `fields:a,b,c` | The columns of the answer. Default: every field. |
| `count:true` | Only the count. |
| `group:field` | The number of records for each value of the field (each item of a list). |
| `view:table\|list\|count\|group` | The view of a wiki block (§27.5). The answer gives it back. |

- `field` can also be a property of the record: `id`, `created`,
  `updated`, `created_by`, `updated_by`, `revision`.
- A value with spaces goes in double quotes: `title="Login fails"`. In
  quotes, `\"` is a quote and `\\` is a backslash. A quoted `"null"` is
  text.
- A `date` takes `YYYY-MM-DD`; a `datetime` takes RFC 3339 or a date
  (its midnight in UTC). Both also take `today`, `now`, and a time back
  from now: `-7d`, `-12h`, `-30m`, `-2w`.
- A `bool` takes `true` and `false`, with `=` and `!=` only.
- A `ref` takes `=` and `!=` with a record ID or a label.
- A `list` takes `=` or `~` (has the item; text without case) and `!=`
  (has not).
- A `json` field takes only `:null` and `:!null`. A `text`, `list`, or
  `json` field cannot be a sort; a `text` or `json` field cannot be a
  group.
- Without `sort:`, the newest record comes first.

The daemon compiles the query to SQL with bound parameters only. A field
name must be a field of the type.

A bad query is `400 bad_request` with `details: {"position",
"message"}`. `position` is the offset (in characters, from 0) of the
term or value with the problem:

```json
{"error":{"code":"bad_request","message":"bad query: position 9: type `bug` has no field `sevrity`; …",
 "details":{"position":9,"message":"type `bug` has no field `sevrity`; …"}}}
```

The answer:

```json
{"type":"bug","query":"type:bug status=open sort:-severity limit:2 fields:title,severity,service",
 "view":"table",
 "fields":[{"name":"title","kind":"string","required":true},
           {"name":"severity","kind":"enum","required":true,"values":["low","medium","high","critical"]},
           {"name":"service","kind":"ref","required":false,"ref":"service"}],
 "records":[{"id":"01JB5…","type":"bug","data":{"title":"…","severity":"critical","service":"01JB4…"}, …}],
 "count":17,"next_cursor":"2","labels":{"01JB4…":"auth-service"}}
```

| Field | Meaning |
|---|---|
| `fields` | The columns. A record property (`fields:title,created`) has `"meta": true`; read it from the record (`created_at`, …), not from `data`. |
| `records` | The page. `data` holds only the columns. Empty for `count:true` and `group:`. |
| `count` | The number of records that match, on all pages. |
| `next_cursor` | Present when there is a next page. Send it as `?cursor=`. It is an offset. |
| `group`, `groups` | With `group:`: the field, and `[{"value", "count"}]`. An enum is in the order of `values`, a bool `true` first, other values the largest count first. `limit:` applies to the groups. |
| `labels` | The label of each record ID in a `ref` column or a `ref` group. |

### 27.4 Rules

- Every member of the project (its agents too) reads and writes types and
  records. A sysadmin and the operator read and write every project. An
  agent of another project gets `403 forbidden`. A record ID of another
  project is `404 not_found`.
- The free text of a write is redacted (§5.2): every string value of
  `data`, the description of the type, and the descriptions of the
  fields.
- **Events.** `db.type` and `db.record`, with `data: {"change", "project",
  "type", "id", "revision"}`. `change` is `created`, `updated`, or
  `deleted`. A schema change that changes records gives one `db.type`
  event with `records_changed`, not an event for each record. A type
  delete gives one event with `records`. The events have the `project`:
  an agent sees those of its own project.
- **Audit.** `db.type.create`, `db.type.update` (the added, removed,
  renamed, and changed fields, and `records_changed`), `db.type.delete`
  (with `records`), and `db.record.delete`. A record create or change has
  no audit row, as a wiki save has none.

### 27.5 The wiki `db` block

A fenced block with the language `db` in a wiki page shows a query
result:

````markdown
```db
type:bug status=open severity>=high sort:-severity limit:10 fields:title,severity,service
```
````

The dashboard renders the Markdown, then puts a placeholder for each
block (the query is a data attribute, as text). After the sanitizer, a
component sends the query to `GET /api/projects/{p}/db/query` with the
project of the page and the rights of the reader. `view:` chooses the
form: `table` (the default: the columns of `fields:`), `list` (the first
column as a list), `count` (the number), or `group` (with `group:`, the
count of each value). Each value shows as text; a `ref` links to the
record on the DB tab. An error shows as a short line in the page.

## 28. System prompts

The system prompt library (PLAN §8.1): `system-prompts/<name>.md` in the
config repo. An agent (§6) or a template (§21) selects an entry with
`persona = "<name>"`. The entries `leader` and `worker` are the personas
by role of every project. A daemon with no config repo answers
`501 not_available`. A write goes to the work tree of the config repo
after the secret scan, and stays uncommitted: `POST /api/config/commit`
(§25) commits it.

The lookup of a name at the start of an agent: `system-prompts/<name>.md`,
then `<prompts dir>/<name>.md` (an older config repo). The list has the
files of `system-prompts/`, and `leader` and `worker` from `prompts/` when
`system-prompts/` does not have them. It never has the protocol templates
of `prompts/` (`default.md`, `project-lead.md`, `system-lead.md`).

Each read goes to the disk: an edit of a file by hand shows at the next
read, and each agent gets it at its next start. There is no reload step.

### 28.1 Shapes

An entry in a list:

```json
{"name":"reviewer","description":"Review the cards in review","size":412,
 "source":"config","path":"system-prompts/reviewer.md","builtin":false,
 "over_limit":false,"updated":"2026-11-01T09:12:00Z"}
```

| Field | Meaning |
|---|---|
| `description` | The text of a first line `<!-- description: <text> -->`. Empty when the file has none. Show it as plain text. |
| `size` | The bytes of the file. |
| `source` | `default`: an unedited packaged default. `config`: any other file. |
| `path` | The file in the config repo: `system-prompts/<name>.md`, or `prompts/<name>.md` in an older repo. |
| `builtin` | `true` for `leader` and `worker`. They cannot be deleted. |
| `over_limit` | `true` when the file has more bytes than `TERRARIUM_SYSTEM_PROMPT_MAX` (default 32 KiB). An agent with this persona does not start (§28.3, "Size at the start"). |
| `updated` | The time of the last change of the file. |

One entry (`GET /api/system-prompts/{name}`): the fields above, `text`
(the prompt text without the description line), and `users`, the agents
and templates that select it:

```json
{"users":[{"kind":"agent","id":"01K…","name":"p/w1","project":"p"},
          {"kind":"agent","id":"01K…","name":"p/w2","project":"p","by_role":true},
          {"kind":"template","id":"01K…","name":"rev"}]}
```

`by_role` is `true` for an agent with no `persona` that gets `leader` or
`worker` by its roles. A global template has no `project`.

### 28.2 Endpoints

| Endpoint | Body | Answer | Needs |
|---|---|---|---|
| `GET /api/system-prompts` | | `200` entries by name | project_admin, sysadmin, or the operator |
| `GET /api/system-prompts/{name}` | | `200` entry; `404` when missing | project_admin, sysadmin, or the operator |
| `POST /api/system-prompts` | `{"name","text","description"?,"replace"?}` | `201` entry; `200` with `replace: true` over an entry that exists; `409` when it exists without `replace`; `422` (below) | project_admin, sysadmin, or the operator; `replace` as `PUT` |
| `PUT /api/system-prompts/{name}` | `{"text"?,"description"?}`: only the fields given change; `"description": ""` removes it | `200` entry; `404` when missing | see "Rights" |
| `DELETE /api/system-prompts/{name}?force=` | | `204`; `404` when missing; `409` when in use or `builtin` | see "Rights" |

### 28.3 Rules

- **Names.** 1 to 63 of `a-z`, `0-9`, `-`, and `_`, starting with a
  letter or a digit (the rules of a template name). `none` is not a
  name: it means the harness's default prompt.
- **Checks.** The text is not empty, and the file is at most
  `TERRARIUM_SYSTEM_PROMPT_MAX` bytes (default 32 KiB, the limit of
  `system_prompt`). The description has no `-->`. The secret
  scan of §25 finds nothing. A write that fails is `422 invalid`; a
  finding of the scan is in `details.findings` and names the file, the
  line, and the rule, never the value. Nothing is written.
- **Rights.** The operator, a sysadmin, and a project_admin read the
  library and add to it. A change or a delete needs sysadmin or the
  operator when the entry is `leader` or `worker`, or when an agent or a
  template outside the caller's project selects it (a global template
  counts as outside). Else a project_admin may.
- **Size at the start.** The start of an agent checks the same limit: a
  persona file over it (for example a file put in `system-prompts/` by
  hand), or a `system_prompt` saved under a higher limit, stops the start
  with an error that names the file, its size, and the limit. An entry in
  the list has `over_limit: true` when its file is over the limit.
- **Delete in use.** A delete of an entry that agents or templates select
  is `409 conflict` with `details.users` (the shape of `users` above).
  With `?force=true` the file goes; the next start of such an agent
  stops with an error that names the missing file. `leader` and `worker`
  cannot be deleted (`409`): edit them instead.
- **Events and audit.** The audit rows `system_prompt.create`,
  `system_prompt.update`, and `system_prompt.delete` hold the name, the
  path, and the size, never the text. The event `system_prompt` (§9) has
  `{"change","name","path"}`, with `change` = `created`, `updated`, or
  `deleted`.

## 29. Test runs

The results of a test run (PLAN §15.5). An agent or the operator uploads
a standard test result file. The daemon reads the file and keeps the run
and each test case. A run links to cards and to labels, such as the
version label `version:1.4.0` (TER-58). The operator reads the runs on the
Tests tab of the Board screen and on the card drawer. The code is in
`crates/terrariumd/src/test_results/` (the parsers) and
`crates/terrariumd/src/store/test_runs.rs`.

### 29.1 Formats

| `format` | The file | Made by |
|---|---|---|
| `junit` | JUnit XML: `<testsuites>` or `<testsuite>`, with `<testcase>` and `<failure>`, `<error>`, `<skipped>`, `<system-out>`, `<system-err>`. Nested suites and `<properties>` are read. `time` is in seconds. | cargo-nextest (`--profile` with `[profile.<name>.junit]`), Vitest (`--reporter=junit`), jest-junit, pytest (`--junitxml`), go-junit-report, Maven Surefire, `dotnet test --logger junit` |
| `trx` | .NET VSTest XML: `<TestRun>` with `<UnitTestResult>` (`outcome` `Passed`, `Failed`, `NotExecuted`, and the others), `duration` as `hh:mm:ss.fffffff`, `<Output>` (`<StdOut>`, `<StdErr>`, `<ErrorInfo>` with `<Message>` and `<StackTrace>`), and `<TestDefinitions>` for the class and the method names. | `dotnet test --logger trx` |
| `ctrf` | CTRF JSON (<https://ctrf.io>): `results.tool`, `results.summary`, `results.tests[]` with `name`, `status`, `duration`, `message`, `trace`, `suite`. | the CTRF reporters (Playwright, Jest, Cypress, and others) |
| `libtest-json` | Lines of JSON from libtest: `{"type":"test","event":"started"\|"ok"\|"failed"\|"ignored",...}` and `{"type":"suite",...}`, with `stdout` and `exec_time`. | `cargo test -- -Z unstable-options --format json --report-time`, `cargo nextest run --message-format libtest-json` |
| `vitest-json` | The JSON reporter of Vitest: `testResults[].assertionResults[]` with `status`, `title`, `ancestorTitles`, `duration`, `failureMessages`. | `vitest --reporter=json` |
| `jest-json` | The `--json` output of Jest. The same shape as `vitest-json`. | `jest --json` |
| `tap` | TAP 13 and 14: `ok` and `not ok` lines, `# SKIP` and `# TODO`, and the YAML diagnostics (`---` to `...`). | `node --test --test-reporter=tap`, prove, and other TAP producers |

**Auto-detect.** `format=auto` (the default) finds the format from the
content:

- XML with the root element `testsuites` or `testsuite` is `junit`. The
  root element `TestRun` is `trx`.
- A JSON object with `results.tests` is `ctrf`. A JSON object with
  `testResults[].assertionResults` is `vitest-json` or `jest-json`. Vitest
  and Jest share the shape; the tool comes from the content.
- Lines of JSON objects with `"type":"test"` or `"type":"suite"` are
  `libtest-json`.
- Text that starts with `TAP version`, a plan line `1..N`, or `ok` and
  `not ok` lines is `tap`.

Content that fits no format is `422 invalid`, and the message names the
formats. Give `format` to skip the detection. A file that does not parse
as the given format is `422 invalid` with the reason (for example the
line and the column of an XML error).

**Statuses.** A test case has the status `passed`, `failed`, `errored`,
or `skipped`. JUnit `<error>` and a TRX `Error`, `Timeout`, or `Aborted`
give `errored`. Skipped, ignored, pending, todo, and not executed tests
give `skipped`. The status of a run is `failed` when `failed + errored >
0`, `passed` when it has tests and none failed or errored, and `empty`
when it has no tests.

**Limits and safety.**

- A file is at most 20 MiB (`413 too_large`) and 100 000 test cases
  (`422 invalid`).
- Each `message`, `trace`, `stdout`, and `stderr` is cut to 64 KiB, with
  the line `[truncated: N bytes more]` at the end. The run gets a note in
  `notes`.
- The XML reader rejects a `<!DOCTYPE>` (`422 invalid`). It reads no DTD,
  resolves no external entity, and expands no entity other than the five
  predefined ones and character references.
- The daemon keeps the original file (compressed) for
  `GET /api/test-runs/{id}/raw`. Known secret values are replaced with
  `[secret:<name>]` in the cases and in the original file.

### 29.2 Shapes

A run (the list items, and the answer of an upload and a link change):

```json
{"id":"01JB7…","project":"demo","name":"unit","format":"junit","tool":"cargo-nextest",
 "status":"failed","counts":{"passed":120,"failed":2,"errored":1,"skipped":3,"total":126},
 "duration_ms":5321,"started_at":"2026-11-02T01:02:03Z","commit":"abc123","branch":"main",
 "file_name":"junit.xml","raw_size":12345,"notes":[],
 "cards":["DEMO-12"],"labels":["version:1.2.0"],
 "created_by":"01M5PA0T…","created_by_name":"demo/worker",
 "created_at":"2026-11-02T01:03:00.000Z","url":"/p/demo/tests/01JB7…"}
```

| Field | Meaning |
|---|---|
| `name` | The name of the upload. Else a name in the file (the `name` of a JUnit root suite, the TRX test assembly, the CTRF `appName`), else the tool, else the file name without its extension. The compare finds the previous run by this name. |
| `tool` | The tool that wrote the file, when the file says it. Else `null`. |
| `duration_ms`, `started_at` | From the file. `null` when the file does not give them. |
| `commit`, `branch` | From the upload. `null` when not given. |
| `file_name`, `raw_size` | The name and the size in bytes of the original file. |
| `notes` | Notes of the parse, for example `3 texts were cut to 64 KiB`. Usually empty. |
| `cards`, `labels` | The links: card IDs, and labels as strings, in the order they were linked. |
| `url` | The dashboard page of the run. |

A suite:

```json
{"name":"terrarium::store","duration_ms":120,"counts":{"passed":10,"failed":1,"errored":0,"skipped":0,"total":11}}
```

A test case:

```json
{"seq":17,"suite":"terrarium::store","name":"creates_a_run","classname":"store::tests",
 "file":"src/store.rs","line":42,"status":"failed","duration_ms":12,
 "message":"assertion failed: left == right","trace":"thread 'creates_a_run' panicked at …",
 "stdout":"…","stderr":null}
```

- `seq` is the position of the case in the file, from 0.
- `classname`, `file`, `line`, `duration_ms`, `message`, `trace`,
  `stdout`, and `stderr` are `null` when the file does not give them.
- The text fields are plain text as the tool wrote it. They can hold
  ANSI colour codes. Show them as text, never as HTML.

One run (`GET /api/test-runs/{id}`): the run fields, and:

| Field | Meaning |
|---|---|
| `suites` | Every suite of the run, in file order, with the full counts. |
| `cases` | One page of the cases that match the filter, in file order. |
| `cases_total` | The number of cases that match the filter (all pages). |
| `next_cursor` | The `cursor` of the next page (a `seq`), or `null`. |

A compare:

```json
{"run":{…run…},"base":{…run…},
 "counts":{"newly_failing":1,"fixed":2,"still_failing":1,"new_tests":4,"removed_tests":0},
 "newly_failing":[{"suite":"terrarium::store","classname":"store::tests","name":"creates_a_run",
                   "status":"failed","base_status":"passed","message":"assertion failed"}],
 "fixed":[…],"still_failing":[…],"new_tests":[…],"removed_tests":[…]}
```

- The key of a case is its suite, its classname, and its name.
- A case fails when its status is `failed` or `errored`.
- `newly_failing`: it fails in the run, and it did not fail in the base.
  `fixed`: it failed in the base, and it passed in the run (a skip is not a fix).
  `still_failing`: it fails in both. `new_tests`: only in the run.
  `removed_tests`: only in the base.
- `status` is the status in the run, and `base_status` the status in the
  base. Each is `null` when the case is not there. `message` is from the
  run, else from the base.
- `counts` has the full numbers. Each list has at most 1000 items.
- `base` is `null` when there is no previous run. Then each list is
  empty.

### 29.3 Endpoints

| Endpoint | Body | Answer |
|---|---|---|
| `POST /api/projects/{p}/test-runs` | the upload (below) | `201` run |
| `GET /api/projects/{p}/test-runs?card=&label=&name=&status=&limit=&cursor=` | | `200` runs, newest first; `X-Next-Cursor` (§2.4) |
| `GET /api/test-runs/{id}?status=&q=&limit=&cursor=` | | `200` one run with its suites and a page of cases |
| `GET /api/test-runs/{id}/raw` | | `200` the original file |
| `GET /api/test-runs/{id}/compare?with=<id>\|previous` | | `200` compare |
| `PATCH /api/test-runs/{id}/links` | `{"add_cards"?, "remove_cards"?, "add_labels"?, "remove_labels"?}` | `200` run |
| `DELETE /api/test-runs/{id}` | | `204` |

**Upload.** Send the file in one of three ways:

- JSON (the CLI sends this): `{"content": "<the file text>"}` or
  `{"data_base64": "…"}`, with the optional fields `file_name`, `name`,
  `format`, `cards` (a list of card IDs), `labels` (a list of labels, such
  as `version:1.4.0`), `commit`, and `branch`. A JSON body is this envelope
  only when it has the key `content` or `data_base64`.
- `multipart/form-data`: the part `file`, and the text parts `name`,
  `format`, `card` (repeat it), `label` (repeat it), `commit`, and
  `branch`. The file name of the part is the `file_name`.
- Any other body is the file itself. Give the fields in the query:
  `?name=&format=auto&card=DEMO-1&card=DEMO-2&label=version:1.0.0&commit=&branch=&file_name=`.

A `versions` field, a `version` part, or a `version` query field is
`400 bad_request` with a message that names `label` (TER-58).

`format` is `auto` (the default) or a format of §29.1. An unknown format
is `422 invalid`. A link that the caller may not make (§29.4) fails the
upload: nothing is stored.

**Lists.** `card` (a card ID), `label` (the exact label), `name`
(the exact run name), and `status` (`passed`, `failed`, or `empty`)
filter the runs. `limit` is 100 by default, at most 1000.

**One run.** `status` filters the cases: `passed`, `failed`, `errored`,
or `skipped`; `failed` gives the failed and the errored cases. `q`
finds text in the suite, the classname, or the name (no case). `limit`
is 500 by default, at most 5000. `cursor` is the `next_cursor` of the
last page.

**Raw.** The answer has the original file with
`Content-Disposition: attachment` and its file name,
`X-Content-Type-Options: nosniff`, and the content type
`application/xml`, `application/json`, `application/x-ndjson`
(`libtest-json`), or `text/plain` by the format.

**Compare.** `with=previous` (the default) is the newest older run with
the same name in the project. `with=<id>` is any run of the same
project; an unknown ID is `404`.

**Links.** Each list in the body holds card IDs or labels. A link that exists already, and an unlink of a link that does not
exist, change nothing.

### 29.4 Rules

- **Rights.** The members of the project (its agents) and the operator
  upload, list, read, and compare runs. An agent of another project needs
  sysadmin.
- **Links.** A link to a card follows the edit rights of the card
  (§8.2): its assignee or its creator, a project admin, or the operator;
  not an archived card; a `done` card only for a project admin or the
  operator. The same rights apply to an unlink. A card of another project
  is `422 invalid`. A label is free text (1 to 200 characters, trimmed,
  no control characters): any member that uploads links and unlinks it.
- **Deletes.** The creator of the run, a project_admin of the project, a
  sysadmin, or the operator deletes a run. A delete by another actor
  writes the audit row `test_run.delete` with the name and the counts.
- **No automatic actions.** A failing run moves no card and changes no
  label. The agents and the operator decide.
- **Events.** The event `test_run` (§9) has `{"change","project","id",
  "name","counts","cards","labels"}`, with `change` = `created`,
  `linked`, `unlinked`, or `deleted`.

## 30. Monitors and the project dashboard

A monitor is a shell script that the daemon runs on a target on a
schedule. The script gives a value, a status, text, or JSON. The daemon
keeps each result. The dashboard of a project shows the monitors as
blocks. With the triggers (§19.4), the results become alerts. The code is
in `crates/terrariumd/src/monitors/` (the runs, the output parse, the
layout) and `crates/terrariumd/src/store/monitors.rs` (the rows).

### 30.0 Project pages

Project pages use the rights in §30.1. `GET /api/projects/{name}/pages`
returns `{"items": [page, ...], "can_edit": true, "dashboard_slug": "dashboard"}` ordered by position, then slug (at most 10, or 11 immediately after dashboard migration).
`can_edit` reports the caller's project-page write right. Clients hide edit controls when false.
`GET /api/projects/{name}/pages/{slug}` returns one page. `PUT` at that
path creates or completely replaces it. `DELETE` removes it (204).
Unknown projects or pages return 404. A PUT body is:

```json
{"title":"Overview","position":0,"widgets":[
  {"id":"intro","kind":"markdown","x":0,"y":0,"w":12,"h":2,"text":"# Welcome"},
  {"id":"guide","kind":"wiki","x":0,"y":2,"w":6,"h":4,"wiki":"guide"},
  {"id":"load","kind":"stat","x":6,"y":2,"w":6,"h":4,"monitor":"MONITOR_ID","options":{"sparkline":true}}
]}
```

A returned page adds `project`, `slug`, `updated_at`, and `updated_by`.
Slugs are 1–64 lowercase ASCII letters, digits, hyphens or underscores,
starting with a letter or digit. Titles are nonempty, at most 120 characters.
Position is a nonnegative integer. Widgets (at most 104, allowing 100 legacy blocks plus four system panels) have unique IDs
(1–40 ASCII letters, digits, hyphens or underscores). Optional `title` is
at most 120 characters. Grid coordinates are integers: x 0–11, y 0–9999,
w 1–12 with x+w <= 12, h 1–10000 with y+h <= 10000. Rectangles cannot
overlap. Optional `auto_height` (default true for markdown/wiki, false for
monitors) lets markdown/wiki content
grow beyond its minimum h. Otherwise h is the scrolling height limit.
The daemon stores cells, not pixel heights.

Kinds are `markdown`, `wiki`, `stat`, `status`, `chart`, `text`, `table`,
`actions`, `monitor_summary`, `job_queue`, `eta_accuracy`. The last four
are system widgets with no source fields.
Markdown requires `text` (at most 16384 UTF-8 bytes). Wiki requires `wiki`,
a wiki path in this project (the wiki's path rules: up to 200 bytes,
lowercase letters, digits, hyphens, underscores, dots and slashes, no empty,
`.` or `..` segments). It is always resolved
relative to this project, never another project. Missing wiki
pages remain valid references so clients can show “page not found”. Monitor
kinds require `monitor`, an existing monitor ID in this project compatible
with the block kind, and accept dashboard `options` (§30.4). Other source
fields are rejected. Unknown fields are rejected. Invalid input returns 422.
Writes produce `page.set` or `page.remove` audit rows and a `page.updated`
event with `{project, slug, deleted}`. Reading never drops stale references.

The former Dashboard is migrated to a custom page, with actions, monitor
summary, job queue and ETA accuracy widgets first, followed by saved blocks
in order. S/M/L widths become 3/6/12 grid columns. New projects receive the
same four system widgets. If `dashboard` is taken, the migrated page uses
`dashboard-legacy`, then `dashboard-legacy-2`, and so on, without changing
user pages. `dashboard_slug` records this choice. Migration may add one
page beyond the limit, but creates remain refused until below 10.
The old Dashboard route redirects to that page. `?tab=monitors` redirects
to project Settings, Monitors. GET/PUT `/api/projects/{name}/dashboard`
and the dashboard CLI remain compatibility aliases for at least one release.
The alias reads an empty layout if the mapped page was deleted. Saving it
replaces monitor/Markdown widgets and preserves system and wiki widgets.

#### Project page layout v2 (default OFF)

`GET /api/projects/{name}/pages` additionally returns
`capabilities: {project_pages_v2: boolean}`. Only exact `true` from the
current project's current instance enables the v2 UI. Missing, failed or
stale responses are OFF. `can_edit` remains independent. OFF retains the
shipped custom-page CRUD, limits and navigation, rejects new widget kinds,
options and frames, and returns 404 for authorized v2 calls. Write rights
are checked first, so unauthorized writers receive 403 even while OFF.
There is no frontend flag or GET-triggered initialization.

The new collection URL deliberately differs from `/pages/{slug}` so an
existing custom page named `layout` remains accessible:

| Method and path under `/api/projects/{name}` | Body / result |
|---|---|
| GET `/page-layout` | `{revision,pages}` snapshot |
| PUT `/page-layout` | `{base_revision,pages}` replaces the whole layout |
| POST `/page-layout/reset` | `{base_revision,builtin}` resets or re-adds one built-in |
| POST `/page-layout/copy` | `{base_revision,source_project,confirm:true}` replaces target with source layout |

All writes return the canonical snapshot. Input pages contain exactly
`{slug,title,position,widgets,builtin?}`. `position` must equal the array
index. At most 30 pages, unique slugs and unique non-null built-in identities
are allowed. Response pages retain `project`, `updated_at`, `updated_by`
and optional `builtin`. Built-in identity is independent of the slug:
`board`, `tests`, `schedules`, `rules`, `agents`, `repos`, `knowledge`,
`pipelines`, `calendar`, `logs`. Settings is excluded.

The required nonnegative `base_revision` is compared under the same
IMMEDIATE transaction as validation and replacement. A stale base returns
409 `conflict`, with `error.details.revision` containing the current revision.
Clients retain their draft and can GET the current snapshot. Validation or
conflict changes no rows, revision, audit or events. Successful bulk writes
increment once and emit one `page.layout` audit and one `page.updated` event
with `{project,revision,operation}`. Operation is `replace`, `copy`, `reset`
or `initialize`. Enabled legacy slug saves/deletes, dashboard-alias saves
and dashboard monitor removal also increment the revision and add
`revision` and `operation` to their existing event payloads.

V2 widgets retain stable IDs and all existing grid/content bounds. Optional
`frame` is `grid`, `full_width` or `viewport`. Absent means legacy grid.
The latter two require `x=0,w=12`. The client owns viewport sizing, document
scrolling and phone stacking, without changing saved cells. Duplicate kinds
are valid independent instances with distinct IDs and options.

Existing kinds retain only `range`, `sparkline`, `decimals` options.
New kinds have no `text`, `wiki` or `monitor` source and accept only optional
`search` (at most 256 UTF-8 bytes) and `view` from this table. Absent `view`
means the first listed value. Unknown fields/values and project overrides
are rejected. Options are validated per kind, never arbitrary JSON.

| Kind | Allowed view values |
|---|---|
| `board` | `lanes`, `done`, `columns` |
| `tests` | `runs` |
| `schedules` | `schedules`, `triggers`, `event_sources` |
| `rules` | `flow`, `lint`, `table`, `last_runs` |
| `agents` | `agents`, `card_agents` |
| `repos` | `list`, `files` |
| `wiki_index` | `browser` |
| `memory` | `list`, `graph` |
| `db` | `types`, `records` |
| `pipelines` | `state`, `runs` |
| `calendar` | `month`, `week`, `agenda` |
| `mail` | `messages` |
| `events` | `events` |

The explicit idempotent initializer appends exactly ten built-in pages after
existing custom pages and the migrated Dashboard, preserving their content,
metadata and order. It uses the first free `name`, `name-1`, `name-2`, … slug.
Each view above is a separate default widget, except Calendar defaults to
month. Knowledge contains wiki browser, memory list/graph and DB types/records.
Logs contains mail and events. Reset uses the same definitions and affects
only the selected built-in. A stored initialized marker prevents restart
from restoring deleted built-ins. Schema migrations add only empty metadata.
With the capability ON, the daemon runs the initializer for every project
without the marker at each start, before it serves a request (one
transaction per project, a refusal is logged and the other projects go on),
and in the transaction of each project creation. OFF never runs it, and a
new project then gets only the Dashboard page as before. GET never runs it.

Copy requires source read plus target administration rights and confirmation.
It copies neither grants nor source audit actor metadata. References remain
target-local, including missing wiki paths/monitor IDs. A foreign monitor ID
is treated as missing in the target, not as permission to read source data.
Renderers must resolve IDs against the target project's monitor inventory,
not fetch a global ID because the caller also has source-project rights.
Existing resource endpoints retain their authorization checks. Compatible
existing local monitor references are validated normally.

The page list also returns `rights`, the caller's existing rights in the
project, for the controls of the page widgets:

| Field | The check it reports | Widget controls |
|---|---|---|
| `write_project` | `write_project` (own project, sysadmin, operator) | Board New card, moves and the card drawer |
| `admin_project` | `admin_project` (same as `can_edit`) | Board columns, New agent, project event sources, rules, pipelines |
| `manage_schedules` | `schedule_scope` of the project | New schedule or trigger, edit of a project schedule |
| `manage_global` | `sysadmin` | global schedules and event sources |
| `operator` | the operator | agent pins |

They describe the endpoints and give no right: each endpoint still checks
every write and answers 403. A missing or failed rights read shows the
widgets read-only. `can_edit` stays the layout right only. The Repos,
Knowledge and Logs widgets keep the controls of their routes, and a refused
write shows the endpoint's 403. The mail widget sends `project` to
`GET /api/mail`, so the daemon returns that project's mail only.

Dashboard (TER-1077), with the capability ON: the stored pages replace the
ten fixed project tabs, in their order. A cog next to Edit pages opens the
project settings (General, Secrets, Templates, Monitors) at the unchanged
`/p/{name}/settings/...` URLs, and the cog of the project row in the
sidebar stays. Settings is no page and has no widgets. The old routes
(`/p/{name}/board?card=…`, `/tests/{id}`, `/repos/{repo}`,
`/knowledge/wiki/…`, `/logs/mail?open=…`, the redirects of nav.ts) still
open their old views with their queries, and the built-in page of that
route is the active tab. A project click opens the Board built-in page,
else the first page, else `/board`. In Edit pages, "Restore default" adds a
deleted built-in again or resets one (`/page-layout/reset`), and "Copy
layout" replaces the whole layout after a confirmation (`/page-layout/copy`).
Both need a draft without unsaved changes. With the capability OFF the
tab row is the old one, and built-in pages that an ON period left are
stored but hidden. An OFF save of a page with v2 widgets returns 422 and
changes nothing.

Switch-on step for a deployment: set `TERRARIUM_PROJECT_PAGES_V2=1` in the
daemon's environment (the `terrarium` service in `compose.yaml` passes
`TERRARIUM_PROJECT_PAGES_V2: ${TERRARIUM_PROJECT_PAGES_V2:-0}`, so set it in
the `.env` next to `compose.yaml` or in the service environment), then
restart the daemon. At the first start with it ON, each existing project
gets the ten built-in pages appended after its stored pages, once (the
marker survives restarts and rollbacks). Each project's layout revision
increases by one, with one `page.layout` audit row (`initialize`) and one
`page.updated` event. Rollback: set it to `0` and restart. The built-in
pages stay stored and hidden, and switching on again shows them without a
second initialization.

CLI: `page layout`, `page save-layout --from-file FILE`,
`page reset BUILTIN --base-revision N`, and
`page copy SOURCE --base-revision N --confirm`. Save accepts `-` for stdin.
No CLI command enables the capability or initializes defaults.

### 30.1 Rights

| Action | Who |
|---|---|
| Read the monitors, the results, and the dashboard | The members of the project and its agents, a sysadmin, the operator |
| Create, change, delete, run now, test run; save the layout | project_admin of the project (a lead), a sysadmin, the operator |
| The same for a system-owned monitor (§30.7) | a sysadmin, the operator |

A monitor has an owner (§30.7): `actor` (the default), `project`, or
`system`. An actor-owned monitor runs a script on a target with the
rights of an actor, so each write that sets the script or the target also
needs the caller to use the target: the target is in the caller's
`policy.targets`, or the caller holds a lease on it (§17.7). The operator
uses any target. A project-owned monitor uses the targets of the
project, and a global target only with a grant (§30.7). The target is a
global target or a target of the project; another project's target is
`422 invalid` with `details.field` `target`. A target that takes no
commands (`vnc`) is `422 invalid`.

`run_as` is the actor whose rights each run of an actor-owned monitor
checks again: the creator, or the last caller that changed the script,
the target, or the owner. When the `run_as` agent lost its role or the
target, the run records `unknown` with the reason. When the `run_as`
agent is deleted, the daemon disables the monitor (§30.7).

The audit log has `monitor.create`, `monitor.update` (with `changed`,
the `owner`, and the `run_as`), `monitor.delete`, `monitor.disable` (the
daemon's disable, with the `reason`), `monitor.run` (a run now),
`monitor.test` (a test run), and `dashboard.update`. A run writes no
`target.exec` row: a monitor every 15 s would add 5 760 rows a day to an
audit log with no prune. The result of each run (§30.4, with the
retention of §30.3) has `actor` `monitor:<project>/<name>`. A result
whose status differs from the one before (`ok` → `crit`, and the first
result) also writes a `monitor.status` audit row with that actor and the
detail `id`, `from`, `to`, `value`, `error`, `result_id`, and `owner`.
The `monitor.result` and `monitor.status` events have the same actor.

### 30.2 Output kinds

The target runs the script with `sh -c`, in the target's default dir.
The daemon reads at most 64 KiB of stdout and of stderr.

| `kind` | The script gives | The status |
|---|---|---|
| `number` | The first number on stdout: an optional sign, digits with optional `,` thousands separators, an optional fraction and exponent. `42%`, `3.5GB`, `load: 0.42`, `1,234,567`, and `-12` give their number. A sign counts only at the start of a word. | From the thresholds: `crit` when the value is at or past `crit`, `warn` when at or past `warn`, else `ok`. `direction` `above` (the default) or `below` says which side is bad. No number: `unknown`, with the first line of stdout as `text`. |
| `status` | An exit code. The first line of stdout that is not blank is the message (`text`, at most 500 characters). | Nagios: 0 `ok`, 1 `warn`, 2 `crit`, any other code `unknown`. |
| `text` | Stdout, without the trailing white space, cut to 8 KiB on a character boundary (`json.truncated` then is `true`). | The exit code, as for `status`. |
| `json` | One JSON object: `value` (a number, or text with a number), `unit` (at most 32 characters), `status` (`ok`, `warn`, `crit`, `unknown`), `message` (at most 500 characters), `rows` (an array of objects). | `status` when given; else the thresholds when the object has a `value` and the monitor has a threshold; else the exit code. A bad `status`, a `value` that is not a number, or `rows` that is not an array of objects is `unknown` with the error. |

`rows` keeps at most 200 objects of at most 20 keys each; more sets
`truncated`. A cell that is an object or an array becomes its JSON text.
A cell or a key is cut to 500 (key: 64) characters.

These runs are `unknown` with an `error`:

- The run passed `timeout_secs`: `timed out after N s`.
- The target is down: a docker or vmlab target that is `stopped` or
  `absent`, or a target whose host agent is offline. A monitor does not
  bring a target up: `target down: ...`.
- The rights check failed, or the command did not start (the error of the
  executor).

Known secret values in the text, the JSON, and the error are replaced
with `[secret:<name>]`, as in the event log.

### 30.3 Runs

The scheduler task (§19) checks the monitors every 5 s. A monitor is due
when it is enabled and its `next_run_at` passed; the tick moves
`next_run_at` on and starts the run. The time math is the schedules':

- `{"kind": "interval", "every": "30s"}`: 15 s or more. The runs are at
  the start of the interval and each `every` after it. A new monitor, a
  new schedule, and a monitor that is turned on run at once and start a
  new interval.
- `{"kind": "cron", "expr": "*/5 * * * *", "tz": "UTC"}`: as §19.

Limits:

- **One run at a time for each monitor.** A tick that finds the previous
  run still running starts nothing and adds 1 to `skipped`. A run now of
  a running monitor is `409 conflict`.
- **At most 8 runs at once in the daemon** (`TERRARIUM_MONITOR_PARALLEL`).
  A run waits for a free slot.
- **Resource limits.** On a `host` target with `limits` (§17.2), the runs
  of a project use the budget of the holder `<project>/monitors`.
- **Leases.** A monitor is not an agent and holds no lease. An
  actor-owned monitor uses the target with the rights of `run_as`: the
  policy of that agent, or a lease that the agent holds. When the lease
  ends, the runs are `unknown`. A project-owned monitor does not depend
  on a lease (§30.7).

**Retention.** A monitor keeps its results `retention_days` (default 7,
1-90) and at most 10 000 results; older ones go at each new result and at
the daily prune.

**Events.** Each result emits `monitor.result`:
`{"project","monitor","monitor_id","status","value","unit","message","error","at","duration_ms","result_id"}`.
A result whose status differs from the result before it (or the first
result) also emits `monitor.status`:
`{"project","monitor","monitor_id","from","to","value","unit","message","error"}`;
`from` is `null` for the first result. Both are events of the project:
its members see them, and the triggers of the project take them. A saved
layout emits `dashboard.updated` `{"project","blocks"}`.

Example: mail the lead when monitor `disk-root` goes critical, at most
once in 30 minutes:

```json
POST /api/schedules
{"name": "disk-alert", "project": "demo",
 "spec": {"kind": "on", "kinds": ["monitor.status"],
          "match": [{"path": "data.monitor", "value": "disk-root"},
                    {"path": "data.to", "value": "crit"}],
          "dedupe": {"key": "{{event.data.monitor}}", "window": "30m"}},
 "action": {"type": "mail", "to": "lead", "subject": "disk-root is critical",
            "body": "{{event.summary}}"}}
```

### 30.4 Endpoints

#### `GET /api/projects/{name}/monitors`

The monitors of the project, by name:

```json
[{"id":"01JC…","project":"demo","name":"disk-root","description":null,
  "target":"build-box","script":"df -P / | awk 'NR==2 {print $5}'",
  "schedule":{"kind":"interval","every":"5m"},"timeout_secs":30,
  "kind":"number","unit":"%","warn":80.0,"crit":90.0,"direction":"above",
  "enabled":true,"retention_days":7,"period_secs":300,
  "created_by":"operator","owner":"actor","run_as":"operator",
  "created_at":"…","updated_at":"…","next_run_at":"2026-11-02T10:05:00.000Z",
  "skipped":0,"running":false,
  "last":{"id":412,"monitor":"01JC…","at":"2026-11-02T10:00:00.120Z",
          "status":"warn","value":84.0,"duration_ms":38}}]
```

`period_secs` is the interval, or for a cron spec the time between its
next two runs. A dashboard block is stale when its last result is older
than `3 × period_secs + timeout_secs`. `next_run_at` is `null` when the
monitor is off. `last` is `null` before the first run. `owner` is
`actor`, `project`, or `system` (§30.7). `disabled_reason` is there when
the daemon disabled the monitor.

#### `POST /api/projects/{name}/monitors`

Body: `name` (1-63 of `a-z`, `0-9`, `-`, `_`; unique in the project),
`description?` (at most 500 characters), `target`, `script` (at most
64 KiB), `schedule`, `timeout_secs?` (1-600, default 30), `kind`,
`unit?`, `warn?`, `crit?`, `direction?` (`above`), `enabled?` (`true`),
`retention_days?` (1-90, default 7), `owner?` (`actor`, the default;
`project`; `system`; §30.7). Answers `201` with the monitor.
Thresholds need kind `number` or `json`, and `warn` comes before `crit`
in the direction. A project has at most 100 monitors (`409 conflict`).
A name in use is `409 conflict`.

Errors: `422 invalid` with `details.field` (`name`, `target`, `script`,
`schedule`, `schedule.every`, `schedule.expr`, `schedule.tz`,
`timeout_secs`, `unit`, `warn`, `retention_days`); `403 forbidden` for
the rights of §30.1.

#### `GET /api/monitors/{id}`, `PATCH /api/monitors/{id}`, `DELETE /api/monitors/{id}`

`id` is the ULID or `project/name` (`%2F` in the path). The patch is a
JSON merge into the fields: a key replaces the field, and `null` clears
an optional field (`{"warn": null}`). An unknown key is `422 invalid`.
`owner` moves the monitor (§30.7): `{"owner": "project"}` gives it to the
project, and `{"owner": "actor"}` makes the caller its `run_as` (a
take-over). The delete answers `204`, and removes the results and the
blocks of the monitor on the dashboard.

#### `POST /api/monitors/{id}/run`

Run now, with the owner's rights (as a timed run), and keep the result.
For an actor-owned monitor, an agent that is not the `run_as` must be
able to use the target. A system-owned monitor needs a sysadmin or the
operator. Answers when the run ends:

```json
{"result":{"id":413,"monitor":"01JC…","at":"…","status":"crit","value":95.0,"duration_ms":41},
 "output":{"exit_code":0,"stdout":"95%\n","stderr":"","end":{"type":"exit","code":0,"reason":"…"}}}
```

`output` (at most 8 KiB of each stream) is `null` when the command did
not start (target down, refused).

#### `POST /api/projects/{name}/monitors/test`

Run a monitor that is not saved, once, as the caller, and keep nothing.
The body is the body of a create; `name` and `schedule` may be absent.
With `owner` `project` or `system`, the run has the rights of that owner
(and the caller needs the rights to make such a monitor).
The answer has the shape of a run now, without `result.id`.

#### `GET /api/monitors/{id}/results`

Query: `since`, `until` (§2.1: RFC 3339 or a duration back from now),
`limit` (default 500, at most 10 000), `bucket`. The results, oldest
first; with more than `limit`, the newest `limit`. Each is
`{"id","monitor","at","status","value?","text?","json?","duration_ms","error?","actor?"}`.
`actor` is `monitor:<project>/<name>` (absent on a result from before
migration 62).

With `bucket` (seconds, or a duration such as `5m`; 1 s to 7 d), one
point for each bucket of time that has results:
`{"at","status","value?","min?","max?","count"}`. `at` is the start of
the bucket, `value` the mean of the values, `min` and `max` their range,
`status` the worst status (`crit`, then `warn`, then `unknown`, then
`ok`), and `count` the results. The charts of the dashboard use it.

#### `GET /api/projects/{name}/dashboard`

```json
{"project":"demo","updated_at":"…","updated_by":"operator",
 "blocks":[{"id":"head","kind":"note","size":"l","text":"## Host","options":{}},
           {"id":"disk","kind":"stat","monitor":"01JC…","size":"s","title":"Root disk",
            "options":{"decimals":0}}],
 "monitors":[ … every monitor of the project, as in the list … ]}
```

A block whose monitor is gone, or whose kind no longer fits the
monitor, stays out of the answer. `terrarium dashboard show` reads this.

#### `PUT /api/projects/{name}/dashboard`

Body: `{"blocks": [...]}`, at most 100 blocks, in order. Answers with the
dashboard as `GET` does. A block:

| Field | Meaning |
|---|---|
| `id` | 1-40 of `A-Z`, `a-z`, `0-9`, `-`, `_`; unique in the layout |
| `kind` | `stat` (the last value, big, with a 24-hour sparkline), `status` (the badge and the message), `chart` (a line chart of the range), `text` (the last text), `table` (the `rows` of the last `json` result), or `note` (Markdown, no monitor) |
| `monitor` | The monitor ID, of this project. Not for a note. |
| `size` | `s` (one column of four), `m` (two), or `l` (the full width). Default `s`. |
| `title` | Optional, at most 120 characters. Default: the monitor name. |
| `text` | The Markdown of a note, at most 4000 characters |
| `options` | `range` of a chart (`1h`, `6h`, `24h` default, `7d`); `sparkline` of a stat (default `true`); `decimals` (0-6) |

`stat` and `chart` need a `number` or `json` monitor; `table` needs a
`json` monitor. Several blocks can show the same monitor. A bad layout is
`422 invalid` with `details.field` such as `blocks[2].kind`.

#### `GET /api/projects/{name}/eta-accuracy`

How well the ETAs of the workers matched the real work time, for the
latest 30 distinct completed cards of the project. The read right of
§30.1 applies. An unknown project is `404 not_found`.

```json
{"raw":{"samples":30,"on_time_pct":56.7,"within_half_to_double_pct":73.3},
 "calibrated":{"samples":24,"on_time_pct":66.7,"within_half_to_double_pct":83.3}}
```

| Field | Meaning |
|---|---|
| `raw` | The ETA that the worker set, against the real work time |
| `calibrated` | The calibrated ETA of each card, computed only from the work that ended before the card started. A card without enough earlier history is left out, so `samples` can be lower than in `raw`. |
| `samples` | The number of cards counted, at most 30 |
| `on_time_pct` | The percent of cards whose work time was at most the ETA |
| `within_half_to_double_pct` | The percent of cards whose work time was between half and double the ETA |

With no samples, `samples` is `0` and both percentages are `null`. The
dashboard loads it once when the dashboard page mounts, apart from the
monitors, and does not refresh it.

### 30.7 Owners and target grants

Monitors and schedules (§19.6) are often meant to outlive the agent that
wrote them. So each has an `owner`:

| `owner` | Its rights | Who makes it, changes it, or takes it over |
|---|---|---|
| `actor` (the default) | Those of `run_as`, checked at each run: the policy or a lease of that agent (the operator: any target) | As §30.1 |
| `project` | Those of the project, checked at each run: the targets of the project, and a global target that is granted to the project for monitors (below). No agent is behind it, so a lease that ends or a lead that goes changes nothing. | project_admin of the project, a sysadmin, the operator |
| `system` | Those of the operator: a global target or a target of the project | a sysadmin, the operator |

- **Move an item in one command.** `terrarium monitor edit <m> --owner
  project` (`PATCH {"owner": "project"}`). `--owner actor` takes it over:
  the caller becomes `run_as`, and must be able to use the target. The
  store audits each move (`monitor.update` with `owner` in `changed`).
- **The runs show the item as the actor.** Each result of a monitor has
  `actor` `monitor:<project>/<name>`, and so do the `monitor.result` and
  `monitor.status` events and the `monitor.status` audit row of a status
  change (§30.1). The definition changes are audited with the caller
  (`monitor.create`, `monitor.update` for an edit, an owner move, a
  take-over, an enable, and a disable; `monitor.run` for a run now). The
  runs of a project- or system-owned schedule keep their `target.exec`
  row (§19.6).

**Target grants.** A monitor on a `host` target runs a shell as the host
agent's user. So one agent's policy does not give a project-owned
monitor a global target: a sysadmin or the operator grants the target to
the project, for one use. The grant is visible in `terrarium project
show`, `GET /api/projects/{name}/target-grants`, and the Monitors tab of
the Dashboard page.

| Endpoint | Body | Answer |
|---|---|---|
| `GET /api/projects/{name}/target-grants` | | `200`, `[{"target","project","for","granted_by","granted_at"}]`. The members of the project read it. |
| `POST /api/projects/{name}/target-grants` | `{"target": "<global target>", "for": "monitors\|schedules"}` | `201`, the grant (`200` when it was there). A sysadmin or the operator. A target of a project is `422 invalid`: the project's items use their own targets with no grant. Audited as `target.grant`; emits `target.grant`. |
| `DELETE /api/projects/{name}/target-grants/{target}/{for}` | | `204`. A sysadmin or the operator. Audited as `target.ungrant`. The next run of a project-owned item on the target records the refusal (`unknown` with `not granted`). |

`for` is `monitors` (project-owned monitors) or `schedules` (the `exec`
action of project-owned schedules, §19.6). A grant goes with its target
and with its project.

**A deleted `run_as` agent.** When an agent is deleted, the daemon
disables each enabled actor-owned monitor and schedule whose `run_as` is
that agent, in the same write. Each gets `disabled_reason` (`its run_as
agent <project>/<name> was deleted; ...`); a monitor also gets a
`monitor.disable` audit row, a schedule a `schedule.update`. The daemon
mails a notice from `system` to the operator (one item of the operator's
Inbox) and to the leads (project_admin agents) of each item's project,
with the list of the items. A project admin takes an item over with an
edit, then enables it: `terrarium monitor edit <m> --owner project
--enable`, or `--owner actor` to run it as the caller. An enable of an
actor-owned item whose `run_as` agent is gone makes the caller its
`run_as`. A disabled item stays disabled; its runs (a run now) record
`unknown` with the reason. A project- or system-owned item does not
change.

## 31. Pipelines

A pipeline shows the runs of a CI/CD system in a project: the builds of
GitHub Actions, the deploys of Octopus Deploy. The daemon polls each
pipeline on a schedule through an adapter, and keeps the recent runs.
The Pipelines page of a project shows them; the triggers (§19.4) turn
their events into alerts. The code is in `crates/terrariumd/src/pipelines/`
(the polls, the adapters, the script contract) and
`crates/terrariumd/src/store/pipelines.rs` (the rows).

There are two adapters:

| `source.kind` | Reads the runs from |
|---|---|
| `github` | The GitHub Actions REST API (§31.3), from the daemon. |
| `script` | A user script on a target (§31.2): the way to any other system (Octopus Deploy, Jenkins, GitLab, Azure DevOps). The script asks the system for its runs and prints them as JSON. |

### 31.1 Rights

| Action | Who |
|---|---|
| Read the pipelines and the runs, open a run | The members of the project and its agents, a sysadmin, the operator |
| Create, change, delete, refresh, test | project_admin of the project (a lead), a sysadmin, the operator |

- A `script` source needs a target that the caller may use, as a monitor
  does (§30.1): in `policy.targets`, or a lease. The target is global or
  of the project. Otherwise `422 invalid` (`details.field`
  `source.target`) or `403`.
- Each secret that a source names (`source.token_secret`,
  `source.env_secrets.<VAR>`) must be one that the caller may use, as for
  a webhook (§19.5): a secret of the project for its project_admin, or a
  global secret with a `use` grant to the project. The operator and a
  sysadmin use any secret. A missing secret is `422 invalid` with the
  field; no right is `403`.
- `run_as` is the actor whose rights each poll checks again: the creator,
  or the last caller that changed the adapter, the target, the script,
  the token secret, the API URL, or the env secrets. A gone `run_as`, a
  lost role, a lost target, or a revoked grant makes the next poll fail
  with the reason.

The audit log has `pipeline.create`, `pipeline.update` (with
`changed`), `pipeline.delete`, `pipeline.refresh`, and `pipeline.test`.
The rows show the secret names, never a value. The timed polls write no
audit row.

### 31.2 The script contract

The target runs `source.script` with `sh -c`, with these variables in
its environment (never on a command line):

| Variable | Value |
|---|---|
| `TERRARIUM_PROJECT` | The project |
| `TERRARIUM_PIPELINE` | The pipeline name |
| each key of `source.env_secrets` | The main value of the named secret (the project's secret first, then the global one). A secret with more than one secret field is an error. |

The script prints one JSON document on stdout and exits with 0. The
document is `{"runs": [...]}` or the array of runs alone, **newest run
first** (the first run gives the status of the pipeline):

```json
{"runs": [
  {"id": "Deployments-123",
   "name": "1.4.2 to Production",
   "status": "Executing",
   "url": "https://octopus.example/app#/Spaces-1/deployments/Deployments-123",
   "environment": "Production",
   "branch": "main",
   "commit": "9fceb02",
   "actor": "wil",
   "event": "manual",
   "started_at": "2026-11-03T10:00:00+10:00",
   "finished_at": null,
   "summary": "Deploys release 1.4.2",
   "steps": [{"name": "Deploy web", "status": "Success",
              "started_at": "2026-11-03T00:00:05Z", "finished_at": "2026-11-03T00:01:10Z"},
             {"name": "Smoke test", "status": "Executing"}]}
]}
```

| Key of a run | |
|---|---|
| `id` | Necessary. A string (1-200 characters) or a number: the ID of the run in the system, unique in the pipeline. A repeated ID keeps the first. |
| `status` | Necessary. A status of the table below, or one of its aliases, in any case. |
| `name` | The title of the run (at most 200 characters). Without it, `number` gives `#<number>`. |
| `number` | A run number, for `name`. |
| `url` | A link to the run: `http` or `https` only. |
| `branch`, `commit`, `actor`, `event`, `environment` | Text, at most 200 characters each. |
| `started_at`, `finished_at` | RFC 3339 times. The daemon keeps them in UTC. |
| `summary` | Text, at most 2000 characters. |
| `steps` | An array of at most 100 steps: `{"name", "status", "started_at"?, "finished_at"?, "url"?, "summary"?}`. Absent: the daemon keeps the steps it has. |

| `status` | Aliases | Meaning |
|---|---|---|
| `queued` | `pending`, `requested`, `created`, `scheduled`, `not_started`, `waiting_for_resource`, `preparing` | Waits to start |
| `running` | `in_progress`, `executing`, `started`, `cancelling` | Runs now |
| `waiting` | `manual`, `action_required`, `approval`, `paused`, `blocked` | Waits for a person (an approval, a manual intervention) |
| `success` | `succeeded`, `successful`, `passed`, `ok`, `completed`, `done` | Ended well |
| `failed` | `failure`, `error`, `errored`, `timed_out`, `timedout`, `broken`, `startup_failure` | Ended badly |
| `cancelled` | `canceled`, `aborted`, `stopped` | Stopped by a person or the system |
| `skipped` | `neutral`, `stale`, `not_run` | Did not run |
| `unknown` | | Any other state |

A space or a `-` counts as `_` (`In Progress`, `not-started`). An
unknown key, a missing `id` or `status`, a bad status, a bad time, a URL
that is not `http` or `https`, or text that is not JSON is a poll error
that names the path (`runs[2].status: \`meh\` is not a status`). At most
100 runs are read. The daemon reads at most 1 MiB of stdout.

An exit code that is not 0 is a poll error: `the script exited with code
N: <the last line of stderr>`. A timeout (`source.timeout_secs`, default
60, at most 600) is `timed out after N s`. A target that is down (as for
a monitor, §30.2) is `target down: ...`; a pipeline does not bring a
target up. On a `host` target with `limits`, the polls use the budget of
the holder `<project>/pipelines`.

Known secret values in the stored runs, the error, a refresh answer,
and a test answer are replaced with `[secret:<name>]`.

### 31.3 GitHub Actions

`{"kind": "github", "repo": "owner/name", "workflow"?: "ci.yml",
"branch"?: "main", "token_secret"?: "github-token", "api_url"?: "..."}`.

- The list: `GET {api_url}/repos/{owner}/{repo}/actions/runs`, or
  `.../actions/workflows/{workflow}/runs` with a `workflow` (its file
  name or ID), with `per_page` = `max_runs`, `branch`, and
  `exclude_pull_requests=true`. `api_url` is `https://api.github.com` by
  default; GitHub Enterprise Server is `https://<host>/api/v3`.
- A run: `id` is the run ID; `name` is `<workflow name> #<run number>`;
  `summary` is the title of the commit or the PR; `status` is the
  GitHub `status`, or its `conclusion` when it is `completed`
  (`failure`, `timed_out`, and `startup_failure` are `failed`;
  `neutral` and `skipped` are `skipped`; `action_required` is `waiting`).
- The steps of a run are its jobs (`.../actions/runs/{id}/jobs`). A poll
  reads them for at most 5 runs that are new, that changed, or that have
  not ended. `GET /api/pipelines/{id}/runs/{run}` reads them for a run
  that has none.
- The token is the main value of the secret `token_secret`, sent as
  `Authorization: Bearer`. It needs read access to Actions. Without a
  token, GitHub gives public repos only, and 60 requests an hour from
  one address: a poll is 1 request, and 1 more for each run whose jobs
  it reads.
- The requests keep the address rules of the webhooks (§19.5,
  `TERRARIUM_WEBHOOK_PRIVATE`): one DNS answer for each request, no
  proxy, no redirect (a renamed repo answers `301`: give the new name).
  A time limit of 20 s, and at most 4 MiB of each answer.
- The errors are sentences: `GitHub refused the token (401)`, `GitHub
  rate limit (403), it resets at ...`, `GitHub has no such repo or
  workflow (404)`.

### 31.4 Polls

The scheduler task (§19) checks the pipelines every 5 s. A pipeline is
due when it is enabled and its `next_run_at` passed. The schedule is
`{"kind": "interval", "every": "5m"}` (the default; 30 s or more) or a
cron spec (§19). A new pipeline, a new schedule or source, and a
pipeline that is turned on poll at once.

- **One poll at a time for each pipeline.** A tick that finds a poll
  still running adds 1 to `skipped`. A refresh of a polling pipeline is
  `409 conflict`.
- **At most 4 polls at once in the daemon** (`TERRARIUM_PIPELINE_PARALLEL`).
- **The runs.** A good poll writes each run it got (an upsert by `id`)
  and keeps the newest `max_runs` (default 20, 1-100), by `started_at`
  (else the time the daemon first saw the run). `status` of the pipeline
  is the status of the first run of the poll. A failed poll keeps the
  runs and the status, and sets `error`. A change of the adapter, the
  repo, the workflow, the branch, or the API URL deletes the runs.

**Events.** All three are events of the project; its members see them,
and its triggers take them:

| Kind | When | `data` |
|---|---|---|
| `pipeline.run` | A run is new, or its status changed. Not on the first good poll of a pipeline: that poll only learns the history. | `{"project","pipeline","pipeline_id","run_id","name","from","to","url","branch","commit","environment","summary"}`; `from` is `null` for a new run |
| `pipeline.status` | The status of the pipeline changed (the first good poll too) | `{"project","pipeline","pipeline_id","from","to","run_id","run_name","url","branch"}` |
| `pipeline.error` | A poll failed, and the poll before it did not | `{"project","pipeline","pipeline_id","error"}` |

Example: mail the lead when a deploy to production fails:

```json
POST /api/schedules
{"name": "deploy-failed", "project": "demo",
 "spec": {"kind": "on", "kinds": ["pipeline.run"],
          "match": [{"path": "data.to", "value": "failed"},
                    {"path": "data.environment", "value": "Production"}]},
 "action": {"type": "mail", "to": "lead", "subject": "A deploy failed",
            "body": "{{event.summary}} {{event.data.url}}"}}
```

### 31.5 Endpoints

#### `GET /api/projects/{name}/pipelines`

The pipelines of the project, by name:

```json
[{"id":"01JD…","project":"demo","name":"ci","description":null,
  "source":{"kind":"github","repo":"o/r","workflow":"ci.yml","token_secret":"github-token"},
  "schedule":{"kind":"interval","every":"5m"},"enabled":true,"max_runs":20,
  "period_secs":300,"created_by":"operator","run_as":"operator",
  "created_at":"…","updated_at":"…","next_run_at":"2026-11-03T10:05:00.000Z","skipped":0,
  "status":"failed","polled_at":"…","ok_at":"…","error":null,"running":false,
  "latest":{"id":"9123456789","pipeline":"01JD…","name":"CI #412","status":"failed",
            "url":"https://github.com/o/r/actions/runs/9123456789","branch":"main",
            "commit":"9fceb02…","actor":"wil","event":"push","environment":null,
            "started_at":"…","finished_at":"…","summary":"Fix the build",
            "steps":[{"name":"build","status":"success"},{"name":"test","status":"failed"}],
            "first_seen":"…","changed_at":"…"}}]
```

`status` is `null` before the first good poll, or when the system has no
run. `polled_at` is the last poll, `ok_at` the last good one, `error` the
error of the last poll (`null` when it was good). `latest` is the newest
run. A run's `steps` is `null` when the steps were not read yet.

#### `POST /api/projects/{name}/pipelines`

Body: `name` (1-63 of `a-z`, `0-9`, `-`, `_`; unique in the project),
`description?` (at most 500 characters), `source`, `schedule?`,
`enabled?` (`true`), `max_runs?` (1-100, default 20). Answers `201` with
the pipeline. A project has at most 50 pipelines (`409 conflict`). A name
in use is `409 conflict`.

`source` of `kind` `github`: §31.3. `source` of `kind` `script`:
`target`, `script` (at most 64 KiB), `timeout_secs?` (1-600, default 60),
`env_secrets?` (at most 20 of `VAR: secret`; `VAR` is `A-Z`, `0-9`, `_`,
not a digit first, not `TERRARIUM_…`).

Errors: `422 invalid` with `details.field` (`name`, `description`,
`source.repo`, `source.workflow`, `source.branch`, `source.token_secret`,
`source.api_url`, `source.target`, `source.script`,
`source.timeout_secs`, `source.env_secrets`,
`source.env_secrets.<VAR>`, `schedule`, `schedule.every`,
`schedule.expr`, `schedule.tz`, `max_runs`); `403 forbidden` for the
rights of §31.1.

#### `GET /api/pipelines/{id}`, `PATCH /api/pipelines/{id}`, `DELETE /api/pipelines/{id}`

`id` is the ULID or `project/name` (`%2F` in the path). The patch is a
JSON merge into the fields: a key replaces the field, and `null` clears
an optional field. `source` merges one level down: `{"source":
{"branch": "main"}}` keeps the other fields of the source, and
`{"source": {"workflow": null}}` clears one. A `source` with another
`kind` replaces the source. An unknown key is `422 invalid`. The delete
answers `204` and removes the runs.

#### `POST /api/pipelines/{id}/refresh`

Poll now, as `run_as`, and keep the runs. An agent that is not the
`run_as` must be able to use the target and the secrets. Answers when
the poll ends:

```json
{"pipeline": { … the pipeline, as GET … },
 "poll": {"runs": 3, "new": 1, "changed": 1, "status": "queued", "error": null},
 "output": {"exit_code": 0, "stdout": "{\"runs\": …", "stderr": "", "end": {…}}}
```

`output` (at most 8 KiB of each stream) is `null` for `github`, and when
the script did not start.

#### `GET /api/pipelines/{id}/runs`

Query: `limit` (default and highest 100). The kept runs, newest first.

#### `GET /api/pipelines/{id}/runs/{run}`

One run, with `project`, `pipeline_name`, and `adapter`. A `github` run
with no steps reads its jobs now, as `run_as`, and keeps them; when that
fails, `steps` stays `null` and `steps_error` says why. An unknown run
is `404`.

#### `POST /api/projects/{name}/pipelines/test`

Poll a pipeline that is not saved, once, as the caller, and keep
nothing. The body is the body of a create; `name` may be absent.
Answers:

```json
{"at": "…", "duration_ms": 420,
 "runs": [{"id": "Deployments-123", "status": "running", "name": "…", "steps": […]}],
 "error": null,
 "output": {"exit_code": 0, "stdout": "…", "stderr": ""}}
```

A failed poll answers `200` with `error` set and no runs. Use it to
write a script: `terrarium pipeline test` prints the same.

## 32. Backups

A backup is one zip file of the daemon's data. The code is in
`crates/terrariumd/src/backup/`. The files go in the backup dir:
`TERRARIUM_BACKUP_DIR`, else `<state>/backups`.

**A backup holds every secret of the daemon.** The state root has
`secrets.key`, the database with the sealed secrets, the plan logins of the
accounts, the git credentials, and the session key. Anyone with an unsealed
backup can read all of them. Seal a backup that leaves the server with a
passphrase (§31.2).

### 32.1 What a backup holds

| Root | Dir | Content |
|---|---|---|
| `state` | `TERRARIUM_STATE_DIR` | The database (as a `VACUUM INTO` copy), the agent dirs, the artifacts, the credentials, `secrets.key`, `session.key`, `operator.password`. |
| `config` | `TERRARIUM_CONFIG_REPO` | The config repo, with its `.git`. |
| `work` | `TERRARIUM_WORK_DIR` | The agent workspaces. Only with `work` (`--work`). |

The database is never a raw file copy: the daemon copies it with
`VACUUM INTO`, which gives one consistent snapshot while the daemon writes.
The walk skips the backup dir, `<state>/trash` (deleted agents and
workspaces retained for cleanup, not included in full or diff backups),
the live database files (`terrarium.db`,
`-wal`, `-shm`, `-journal`), the restore dirs (§32.3), sockets, and the
dirs named `target`, `node_modules`, and `.cache`, at any depth. The tools
dir is not a root. Symlinks stay symlinks. The manifest records the mode of
each file and dir and the time of each file.
The manifest's optional `excluded_paths` metadata records the `state/trash`
policy exclusion. It does not change the ZIP format or restore procedure;
older readers ignore this field and older manifests omit it.

The entries of the zip:

| Entry | Content |
|---|---|
| `README.txt` | A warning that the file holds secrets. Always plain. |
| `backup.json` | The header: `format` (`terrarium-backup`), `version` (1), `id` (a ULID), `kind`, `created_at`, `base` (a diff: `{id, name, created_at}`), `terrarium_version`, `db_user_version`, `contains_secrets` (`true`), `roots`, `encryption`, `entries`, `stored_files`, `stored_bytes`. Always plain. |
| `manifest.json` | `{id, entries: [{path, kind, mode, size, mtime_ns, sha256, link, stored_in}]}`: every file, dir (`kind: dir`), and symlink (`kind: symlink`, `link`). `path` is `<root>/<relative path>`. `stored_in` is the ID of the backup whose zip has the content. |
| `files/<root>/<path>` | The content of each file that this backup stores. |

**Full and diff.** A `full` backup stores every file. A `diff` backup
names a full backup as its `base` (default: the newest full backup in the
dir) and stores only the files that changed since that base. A file with
the same size, time, and mode as in the base, or with the same SHA-256, is
not stored again: its entry has `stored_in` = the base ID. The manifest of
a diff still lists every file, so a restore of the diff gives the exact
tree at the time of the diff. Files that were deleted after the base do not
come back. Each backup has the whole database. A diff needs its base: do
not delete a full backup while you need a diff of it.

### 32.2 Sealed backups

With a passphrase, the daemon seals the manifest and each file. The key is
Argon2id of the passphrase and a random salt (the default parameters of
the `argon2` crate, in `backup.json` `encryption`). Each entry is a stream
of XChaCha20-Poly1305 chunks of 64 KiB (the files are deflated first). The
associated data is `<backup id>|<entry name>`: an entry that is changed,
cut, or copied to another name or backup does not open. A wrong passphrase
fails at once (`encryption.check`). The header and `README.txt` stay
plain, so `ls` shows a sealed backup. The file names and sizes are in the
sealed manifest, not in the header; the zip directory shows the entry
names. There is no recovery of a lost passphrase.

### 32.3 Restore

A restore is offline only: `terrariumd backup restore <file>`, with the
daemon stopped (it refuses while a daemon answers on the socket). The API
has no restore, because a restore replaces the database under the daemon.

1. Open the backup. For a diff, open its base: `--base`, else the backup
   with the base ID in the dir of the diff.
2. Check each manifest path (a known root, no `..`, no absolute path).
3. Write every entry into `<root>/.terrarium-restore-<id>/` and check the
   size and SHA-256 of each file. Any error removes the staging dirs and
   stops: the old data does not change.
4. In each root, move the old content to
   `<root>/.terrarium-pre-restore-<time>/`, then move the new content into
   place. The backup dir and the restored file stay where they are. A
   root that the backup does not have (`work`, without `--work`) does not
   change; `--skip-work` leaves the work root too.

**The restore drill.** Test a backup before you need it:

```sh
terrariumd backup drill <file> [--base <full>] [--passphrase-file <file>] [--into <dir>] [--keep] [--json]
```

The drill restores the backup into an empty scratch dir (default: a new
`.drill-<id>` dir in the backup dir), with the roots `state`, `config`,
and `work` in it. It then checks that each manifest entry is in place
with its size, that the restored database passes `PRAGMA
integrity_check` and has the schema version of the header, and that
`secrets.key` is back. The live data does not change, so the daemon can
run. The scratch dir holds the secrets in plain form: the drill deletes it
at the end unless `--keep`. Only an absent or empty dir is taken: a dir
with content, one that cannot be read (for example permission denied),
or a symlink is refused and not deleted. Run a drill
after the first scheduled backup, after a key rotation, and after an
upgrade of terrariumd.

**The restore, step by step.**

1. Pick the backup: `terrariumd backup ls`. For a diff, its base must be in
   the same dir (or give `--base`).
2. Get the passphrase: the `backup-key` secret is in the backup itself, so
   use your off-server copy. A backup older than the last rotation needs
   the old passphrase.
3. Drill it: `terrariumd backup drill <file> --passphrase-file <file>`.
4. Stop the daemon.
5. Restore: `terrariumd backup restore <file> --passphrase-file <file>`.
6. Start the daemon. Check the board, the agents, and `terrarium secret
   ls`.

Delete the `.terrarium-pre-restore-*` dirs when the daemon runs well. The
restore writes the files as the user that runs it: run it as the user of
the daemon. A database from an older terrariumd migrates at the next
start. A database from a newer one (`db_user_version` larger than the
migrations of this build) does not start.

### 32.4 Scheduled backups

The settings are daemon env vars:

| Variable | Default | Meaning |
|---|---|---|
| `TERRARIUM_BACKUP_SCHEDULE` | `off` | `daily`, `weekly`, or a duration of `1h` or more. |
| `TERRARIUM_BACKUP_FULL_EVERY` | `7d` | The longest time between two full backups. |
| `TERRARIUM_BACKUP_KEEP` | `5` | Legacy setting, accepted but ignored. Retention uses the operator-approved rule below. |
| `TERRARIUM_BACKUP_WORK` | `off` | `on`: add the workspaces. |
| `TERRARIUM_BACKUP_KEY_SECRET` | `backup-key` | The global secret with the passphrase. |
| `TERRARIUM_BACKUP_PASSPHRASE_FILE` | none | A file whose first line is the passphrase. It wins over the secret. The daemon user must own it, and only the owner may read it (mode `0600` or `0400`); else each run fails. |

A bad value stops the daemon start. With a schedule, a daemon task makes a
backup one interval after the newest backup in the dir, and not before 10
minutes after the start. A failed run waits one interval too.

**Kind.** The backup is `full` when the dir has no full backup, when the
newest is older than `TERRARIUM_BACKUP_FULL_EVERY`, or when the key does
not open the newest full backup (it is not sealed, or the key was
rotated). Else it is a `diff` of the newest full backup. A diff and its
base so always have the same key. A run holds the one-backup lock of the
dir from the choice of the kind and the base to the prune: a manual
backup (§32.5) during the run gets `409 conflict`; it does not wait.

**The key.** A scheduled backup holds every secret, so it is always
sealed (§32.2). The passphrase is the `passphrase` field of the global
secret `backup-key`, else its `value` field (`terrarium secret set
backup-key`; an `api_key` or a `generic` secret). With no key the run
fails and the operator gets a mail. The key never goes into a log, an
event, an audit row, or an error. Keep a copy of the passphrase off the
server: the secret store is in the backup, so a lost server takes the
secret with it.

**Rotation.** Set a new value on the secret. The next run is a full backup
with the new key. The older backups keep the old key: keep the old
passphrase until retention deletes the last of them. Restore or verify an old
backup with the old passphrase (`--passphrase-file`).

**Verify and record.** Each run reads the new backup back and checks each
file against the manifest (§32.3 `verify`). The audit actor is
`system:backup`. The rows are `backup.create`, `backup.verify` (`name`,
`id`, `kind`, `checked_files`), `backup.prune`, `backup.failed` (`kind`,
`error`), `backup.recovered`, and `backup.missed` (`due`). Each run also
writes a global event of the kind `backup` with `change`: `verified`,
`pruned`, `failed` (`level: error`), `recovered`, or `missed`
(`level: error`). A backup that does not verify moves to
`.unverified-<name>` in the backup dir: it is not a base and not a kept
backup. When the move fails the run deletes the file; when that fails
too, the error mail says to delete it by hand.

**Retention.** The operator's rule is to keep the newest **5**
daemon-written full ZIPs plus the latest backup on each of the last **14
UTC calendar days**, including today. Deletion stays off until explicit
operator initialization: `terrarium admin backup retention init` shows the
first bounded pass's exact names, dates, byte sizes and total. Re-run with
`--confirm` to initialize and enable passes. The operator-only API is
`POST /api/backups/retention/init` with `confirm` and
`no_rollback_reference` booleans (both default false). Missing or unreadable
`history.json` or `previous.json` refuses initialization and names the file
needing repair. Only a never-updated installation may explicitly establish
an empty baseline with `--no-rollback-reference --confirm`. Initialization
and that flag are durably audited. No fresh installation is inferred from
missing files. Later missing/unreadable safety files, or a missing initialized
marker in `retention.json`, always prevent deletion. Missing retention state
also prevents update admission until repaired. An independent initialization
epoch in `history.json` survives run rotation. Ordinary init cannot replace
lost initialized state. `terrarium admin backup retention repair` previews
conservative recovery holds and the deletion list. `--confirm` audits and
writes a new epoch. Its operator-only endpoint is
`POST /api/backups/retention/repair` with `confirm` (default false).
Repair inspects every Docker ExecID and refuses while an exact retention
helper is running. Every leftover `.hot-swap.sqlite` and its ZIP becomes a
named hold until explicit recovery release. The response prints release keys.
Repair requires readable history and previous state, not an inferred empty
rollback baseline. Init and repair atomically store confirmed names in the
initialized state. The first pass only deletes confirmed names still eligible
under the directory lock. New backups never expand that first-pass list.

Helper authorization expires after ten seconds. The updater renews only
after strict rereads of all three safety files, a matching initialization
epoch and current exec/token interlock. Failure revokes the token and stops
renewal. A paused helper checks expiry before each unlink, so missing state
stops deletion within ten seconds, within the 32-file pass cap.

The updater is the only deletion
owner and serializes retention with updates and rollbacks. API, CLI and
scheduled creates request a pass after backup creation. The daemon also
requests a pass at startup and every minute, including when scheduling
is off. With no reachable updater, or while an update is running, no
retention occurs and a warning reports the deferral.

An active update (even before its backup path is recorded), the rollback
backup in `previous.json`, and failed-recovery ZIPs and snapshots are
protected. An interrupted updater leaves a durable hold in
`<updater-state>/retention.json`, blocking deletion and new updates until
the operator has recovered the run and releases it. Recovery holds survive
history rotation. Full backups referenced by retained diffs are also kept.
Unknown names, invalid headers, and future-dated backups are not deleted.

Before helper launch the updater persists a `retention_running` interlock
with the Docker exec ID and a unique token, then records the observed host
PID and helper PID/start time. Updates refuse while that interlock remains.
Startup and uncertain execution revoke the token, terminate the exact helper
using its container PID/start time and pidfd, then inspect the exact Docker
exec to prove it stopped before clearing the interlock. An inspection or
cancellation failure keeps admission blocked. A delayed start cannot reuse
a revoked token. The helper re-reads the mirrored token and protections
under the directory lock immediately before each unlink. Private control,
identity, receipt and permanent revocation files live in the backup directory.

A paused `<ZIP>.hot-swap.sqlite` belongs to its ZIP and is deleted with
it, unless already removed by hot-swap commit. Each pass deletes at most
32 files, by exact names inside the backup directory. A cross-process
directory lock serializes creation and deletion. The durable
`<backup-dir>/.retention-audit.jsonl` records each deletion's intent and
completion, exact name and byte size. No deletion proceeds if journaling
fails. These journals are not included in backups.

After manual recovery, the operator can run
`terrarium daemon backup release-recovery <run-id>`
(`POST /api/backups/recovery-release`, body `{"run":"<run-id>"}`). Agents,
including sysadmins, cannot release a hold. Release is irreversible and
audited in `<updater-state>/retention-audit.jsonl` before it takes effect.
It permits retention, not necessarily immediate deletion: the five/daily
rule and the previous rollback target still apply.

**Alerts.** The first failed run of a streak mails the operator from
`system` (subject `Scheduled backup failed`, with the error). The next
failures write events only, and the first good run writes `recovered`. A
missed run (no backup and no try for one interval and one hour, for
example after the daemon was down) mails the operator once per due time
(subject `Scheduled backup missed`). A new backup then runs 10 minutes
after the start. The schedule keeps its marks in `.schedule.json` in the
backup dir: the time it was turned on, the last try, and the due time of
the last missed mail. So a restart does not mail the same missed run
again, and a schedule that is turned on over an old manual backup mails
no missed run at once. The file goes when the schedule is off.

This task is not a project schedule (§19): a project schedule starts agent
turns in one project, and a backup is of the whole daemon.

### 32.5 The API

The operator only (`403` for an agent, also a sysadmin). One backup runs at
a time per backup dir: a create while one runs is `409 conflict`. A scheduled run waits
for a running backup instead.

#### `POST /api/backups`

Make a backup now. The body is optional:

| Field | Meaning |
|---|---|
| `kind` | `full` (the default) or `diff` |
| `work` | `true`: add the workspaces. Default `false`. |
| `passphrase` | Seal the backup (§32.2). Empty is `422 invalid`. |
| `base` | The base of a diff: a file name in the backup dir. Default: the newest full backup. |

Answers `201` with `{name, path, size, header}` (`header` is
`backup.json`). A diff with no full backup is `422 invalid`. The audit row
is `backup.create` (target: the file name; detail: `id`, `kind`, `base`,
`roots`, `encrypted`, `entries`, `stored_files`, `size`). The database
copy holds the database lock while it runs.

#### `GET /api/backups`

The backups in the dir, oldest first: `[{name, size, ...header}]`. A file
whose header does not read has `error` and no header fields.

#### `GET /api/backups/{name}`

The zip file (`application/zip`, `Content-Disposition: attachment`,
`Cache-Control: no-store`). `name` is a file name from the list; any other
name is `404 not_found`. The audit row is `backup.download`.

## 33. The calendar

The calendar holds the planned work of a project: maintenance windows,
change freezes, deploy windows, and tasks. Agents put events on it, so
that a human can see the plan easily. Each project has a calendar. The
global calendar holds the events of every project (for example a host
maintenance window): each project view shows them, marked `global`. The
operator also has a combined view of every project. The code is in
`crates/terrariumd/src/store/calendar.rs` (the rows, the checks, the
notices), `crates/terrariumd/src/calendar/` (the recurrence rules and the
tick), and `crates/terrariumd/src/api/calendar.rs`.

### 33.1 The event

| Field | Meaning |
|---|---|
| `id` | The ULID. |
| `project` | The project; `null` for a global event. `global` is `true` then. |
| `title` | 1 to 200 characters. |
| `description` | Markdown, at most 20000 characters. Default empty. |
| `kind` | `window`, `maintenance`, `freeze`, `deploy`, `task`, or `other` (the default). |
| `start`, `end` | The first occurrence, as UTC instants (RFC 3339 with milliseconds). |
| `all_day` | An all-day event: it starts at local midnight in `tz` and lasts whole days. `start_date` and `end_date` are its first and its last local day. |
| `tz` | The IANA zone of the event (default `UTC`). |
| `recurrence` | The repeat rule (§33.2), or `null`. `rrule` is the rule as RRULE text and `repeat` in a few words. |
| `links` | `{card?, target?, repo?, url?}`. The daemon does not check that they exist. `url` is `http://` or `https://`. |
| `assignees` | `[{id, name, project}]`: the agents that get the start and end notices (§33.5). An event of a project takes the agents of the project; a global event takes any agent. At most 50. |
| `exceptions` | The starts of the skipped occurrences. |
| `created_by`, `updated_by` | Actor labels (`operator`, `agent:<ulid>`). `created_by_name` is for display. |
| `created_at`, `updated_at` | Times. |
| `can_edit` | In the answers: `true` when the caller may change and remove the event (§33.3). |

The time fields of a create or a patch take three forms:

- an RFC 3339 time with an offset: `2026-11-10T09:00:00Z`;
- a local time in `tz`: `2026-11-10T09:00` (seconds optional);
- a date: `2026-11-10`. A date makes an all-day event (unless `all_day`
  is `false`: then it is `422`). The `end` of an all-day event is its
  **last day**, not the day after.

Without `end`, a timed event lasts one hour and an all-day event one day.
The `end` is not before the `start`. An occurrence lasts at most 366
days. A calendar holds at most 1000 events. A bad field is
`422 invalid` with `details.field`.

### 33.2 The repeat rule

The rule is a JSON object, or the same rule as RRULE text (RFC 5545,
a subset). The API stores the object.

| Field | RRULE | Meaning |
|---|---|---|
| `freq` | `FREQ` | `daily`, `weekly`, or `monthly`. |
| `interval` | `INTERVAL` | Every N days, weeks, or months: 1 to 999 (default 1). |
| `weekdays` | `BYDAY` | A weekly rule only: `["mo", "th"]` (`MO,TH`). Empty: the weekday of the first start. |
| `month_day` | `BYMONTHDAY` | A monthly rule only: 1 to 31, or -1 for the last day. Absent: the day of the first start. |
| `until` | `UNTIL` | A local date (`2026-12-31`, that day included; RRULE `20261231`) or an RFC 3339 time (no occurrence starts after it; RRULE `20261231T235959Z`). |
| `count` | `COUNT` | The number of occurrences: 1 to 10000. Not with `until`. |
| `tz` | - | The zone whose wall clock repeats. Absent: the zone of the event. |

`WKST=MO` is accepted (weeks start on Monday). Each other RRULE part
(`FREQ=YEARLY`, `BYSETPOS`, `BYHOUR`, `BYDAY=1MO`, more than one
`BYMONTHDAY`, ...) is `422 invalid`, with a message that names the part.

The expansion:

- The rule repeats the local time of the first start in its zone. A
  09:00 event stays at 09:00 local time across a DST change, so its UTC
  time moves. A local time that the zone skips (the spring gap) moves on
  by the gap; a local time that comes twice (the autumn overlap) takes
  the first. A timed occurrence keeps the length of the first one; an
  all-day occurrence keeps its number of local days.
- The first start need not match the rule: the first occurrence is the
  first date on or after it that matches.
- A monthly rule skips a month that has no such day (31 in April).
- `count` counts the skipped occurrences too.

A skip (an exception) names one occurrence by its start. A change of the
rule or of the start drops each skip that names no occurrence any more.

### 33.3 Rights

| Action | Who |
|---|---|
| Read a project calendar | The agents of the project, a sysadmin, the operator |
| Read the global calendar | Every agent, the operator |
| Add, change, remove, skip, assign in a project calendar | A lead (project_admin of the project), a sysadmin, the operator |
| Add events to a project calendar; change, remove, skip, and assign the own ones | An agent of the project with the calendar grant |
| Write the global calendar | A sysadmin, the operator |
| Grant and revoke the calendar write right | A lead of the project, a sysadmin, the operator |

A worker reads its project calendar only, by default. The grant is the
field `policy.calendar_write` of the agent's policy (PLAN §10): a lead
sets it with `PUT /api/projects/{name}/calendar/grants/{agent}`, with
`agent edit --policy`, or in an agent file. A worker without the grant
that writes gets `403` with the advice to ask its lead. A global event is
read-only in a project view (`can_edit` is `false` for a project user).

The audit log has `calendar.create`, `calendar.update` (with `changed`),
`calendar.delete`, `calendar.skip`, `calendar.unskip`,
`calendar.assign`, `calendar.unassign`, `calendar.watch`,
`calendar.unwatch`, `calendar.grant`, and `calendar.revoke`.

### 33.4 Watchers and events

An agent, or the operator Inbox, can watch a calendar (a project or the
global one). A watcher gets a mail from `system` for each add, change
(with a short diff of the changed fields: title, kind, when, zone,
repeat, links, assignees, skips, "description changed"), and removal.
The actor of a change gets no notice of its own change. Mail to the
operator opens an Inbox item.

Each write emits an event on the event log, for the triggers (§19.4):

| Kind | When | `data` |
|---|---|---|
| `calendar.event_created` | An add | `{project, global, event_id, title, kind, start, end, all_day, rrule, assignees}` |
| `calendar.event_updated` | A change, a skip, an assignee change | the same, with `changed` (the field names) and `diff` (`{field: "old → new"}`) |
| `calendar.event_deleted` | A removal | the same as created |
| `calendar.event_started` | An occurrence starts (§33.5) | the same, with `occurrence`, `start`, `end` of the occurrence, `late`, `reason` |
| `calendar.event_ended` | An occurrence ends, or an edit, a skip, or a removal ends it early (`reason`) | the same |

A global event emits its events with no `project`.

### 33.5 Start and end notices

The scheduler task runs the notice tick with the schedule tick (every
10 s). Each assignee of an event gets a mail from `system` when an
occurrence starts and when it ends: a normal mail (the agent reads it as
`[Mailbox:system]`), not a turn. The start mail quotes the description.
The tick also emits `calendar.event_started` and `calendar.event_ended`,
for every event (also one with no assignee).

The table `calendar_notices` keeps one row for each occurrence and edge
(`start`, `end`), so each notice goes out exactly once, also across a
daemon restart. The daemon keeps the time of the last tick. A tick looks
at each occurrence that started by now and ended after the last tick:

- No start notice yet, and the occurrence has not ended: the start
  notice. When it started more than 60 s ago (the daemon was down), the
  notice says that it is late (`late: true`).
- No start notice yet, and it ended in the last 60 s (a short occurrence
  between two ticks): the start and the end notice.
- No start notice yet, and it ended before that: it started and ended
  while the daemon was down. No mail and no event; both rows are kept
  with the state `skipped`.
- A start notice and no end notice, and it ended: the end notice, once.

An occurrence that ended before an event was made gets no notice.

**An edit of an occurrence that started.** When a change (or a skip)
touches an event with an occurrence whose start notice went out and
whose end notice did not:

- when the changed event has an occurrence that runs now, the open
  notice moves to it: no second start notice, and the end notice comes at
  its new end (a longer or shorter end, a new title);
- else (the edit moved the occurrence away, or skipped it) the assignees
  get the end notice at once, with the reason `an edit moved the
  occurrence`, and `calendar.event_ended` has the same `reason`.

A removal of an event with a running occurrence sends its end notice at
once, with the reason `the event was removed`. An occurrence that the
edit puts into the past gets no start notice.

### 33.6 The API

#### `GET /api/projects/{name}/calendar`

The occurrences of the project's events and of the global events in a
window, by start. Query:

| Field | Meaning |
|---|---|
| `from` | An RFC 3339 time or a date (UTC midnight). Default: the start of today (UTC). |
| `to` | The end of the window (not included). Default: `from` + `days`. |
| `days` | The days of the window when `to` is absent: 1 to 400 (default 31). |

The window is at most 400 days (`400 bad_request`). The answer:

```json
{"from": "2026-11-01T00:00:00.000Z", "to": "2026-12-02T00:00:00.000Z",
 "occurrences": [{"event_id": "01JD…", "start": "2026-11-10T22:00:00.000Z",
                  "end": "2026-11-10T23:30:00.000Z", "all_day": false,
                  "title": "Weekly patch window", "kind": "maintenance",
                  "project": "terra", "global": false, "recurring": true,
                  "assignees": ["terra/lead"]}],
 "events": [{"id": "01JD…", "title": "Weekly patch window", "...": "§33.1"}],
 "truncated": false, "can_write": true}
```

`events` holds each event with an occurrence in the window. At most 5000
occurrences; `truncated` is `true` when the list is cut. `can_write`
says whether the caller may add an event to this calendar.

#### `GET /api/calendar`

The combined view. The operator and a sysadmin get every project and the
global events; another agent gets its own project and the global events.
`project=<name>` narrows it to one project (with the global events). The
query and the answer are those of the project view; `can_write` is for
`project` (`false` without it).

#### `GET /api/calendar/global`

The global events only. `can_write` is for the global calendar.

#### `POST /api/projects/{name}/calendar/events`, `POST /api/calendar/global/events`

Add an event. The body has the fields of §33.1 that a client writes:
`title` and `start` are required; `description`, `kind`, `end`,
`all_day`, `tz`, `recurrence` (an object or RRULE text), `links`, and
`assignees` (agent IDs, `project/name`, or names in the project) are
optional. Answers `201` with the event.

#### `GET /api/calendar/events/{id}`

One event, with `can_edit`.

#### `PATCH /api/calendar/events/{id}`

Change an event: a key replaces the field; `null` clears an optional one
(`recurrence`, `links`, `end`). A new `start` of a timed event with no
new `end` keeps the length. `all_day` alone turns the times into the
local days of the instants, or the days into one hour from local
midnight. `assignees` replaces the list. Answers with the event. The
rule of §33.5 applies to a running occurrence.

#### `DELETE /api/calendar/events/{id}`

Remove the event with each occurrence. Answers `204`.

#### `POST /api/calendar/events/{id}/skip`, `POST /api/calendar/events/{id}/unskip`

`{"occurrence": "2026-11-17T22:00:00Z"}` or `{"occurrence":
"2026-11-17"}` (the local date in the event's zone, when one occurrence
starts on it). Skip one occurrence of a repeating event, or take the
skip back. An event that does not repeat, or a value that names no
occurrence, is `422 invalid` (`details.field` `occurrence`). Answers with
the event.

#### `PUT /api/calendar/events/{id}/assignees/{agent}`, `DELETE ...`

Add or remove one assignee. Answers with the event.

#### `GET /api/calendar/events/{id}/notices`

The notice rows of the event: `[{occurrence, edge, state, at}]`, with
`state` `sent`, `late`, `skipped`, or `closed` (an early end).

#### `GET /api/projects/{name}/calendar/watches`, `GET /api/calendar/global/watches`

The watchers: `[{watcher, name, project, created_by, created_at}]`.
`watcher` is the agent ULID or `operator`.

#### `POST /api/projects/{name}/calendar/watches`, `POST /api/calendar/global/watches`

Start to watch. The body is optional: `{"watcher": "..."}`. Without it,
the caller watches. `operator` (the Inbox) is for the operator only. An
agent watches for itself with the read right; to make another agent
watch, the caller is a lead of the project (a sysadmin for the global
calendar). Answers `{calendar, watcher, watching: true, changed}`.

#### `DELETE /api/projects/{name}/calendar/watches/{watcher}`, `DELETE /api/calendar/global/watches/{watcher}`

Stop. `me` is the caller. Answers `{calendar, watcher, watching: false,
changed}`.

#### `GET /api/projects/{name}/calendar/grants`

The agents of the project with the calendar grant: `[{id, name,
project}]`. The read right of the project.

#### `PUT /api/projects/{name}/calendar/grants/{agent}`, `DELETE ...`

Grant or revoke the calendar write right (`policy.calendar_write`) of an
agent of the project. A lead, a sysadmin, or the operator. An agent from
an agent file is `409 file_managed`: change the file. Answers `{agent,
name, project, calendar_write, changed}`.

## 34. Self-update

Terrarium updates itself from release images:
`ghcr.io/wiltaylor/terrarium:<version>`, `:latest` (the newest stable
release), and `:pre` (the newest pre-release). CI makes them
([release.md](release.md)). The design is in PLAN §23.8. The code is in
`crates/terrarium-updater` (the updater) and
`crates/terrariumd/src/self_update.rs` with
`crates/terrariumd/src/api/system_update.rs` (the daemon).

**The daemon never holds `docker.sock`.** A sandbox escape into the
daemon must not give root on the host. A separate service,
`terrarium-updater` (compose.yaml, profile `updater`), holds it. The
updater runs no agents and opens no port. It listens only on the updater
socket, which it shares with the daemon (§34.6). The daemon asks it for an
update or a rollback; the updater does the work and keeps the record.

### 34.1 Set it up

1. Set these lines in `.env` (a deploy writes `TERRARIUM_DOCKER_GID` in a
   new `.env`):

   ```
   COMPOSE_PROFILES=updater
   TERRARIUM_DOCKER_GID=<the group of /var/run/docker.sock: stat -c %g /var/run/docker.sock>
   ```

2. Run `docker compose up -d` (a deploy: `<dir>/terrarium-compose up -d`).
   The updater starts from the image of the daemon, with the entrypoint
   `terrarium-updater`. `TERRARIUM_UPDATER_IMAGE` sets another image.
3. Run `terrarium admin update status`. The line `updater:` shows its build
   and the signature check.

The updater runs as `TERRARIUM_UID` with the docker group added. It makes
`<data>/updater/run` mode `0700` and the socket mode `0600`, and it refuses
a peer with another uid (`SO_PEERCRED`). The daemon binds the same dir at
`/run/terrarium-updater`. The agent sandboxes do not bind it.

**An install from source** (`just deploy::server`, `just up`) runs a local
image (`terrarium-<dir>:<commit>`, `terrarium:dev`). The first update
switches it to the GHCR image: the updater makes the new container with the
settings of the old one and the release image. After that, the image file
`<data>/updater/state/image` names the release image, and the deploy
wrapper `<dir>/terrarium-compose` uses it in place of `src/.deployed-ref`,
so a later `terrarium-compose up -d` keeps it. A source deploy
(`just deploy::update`) or a deploy rollback removes the file: the source
wins again. In a dev checkout (`just up`), set `TERRARIUM_IMAGE` in `.env`
to the release image, or `just up` builds and starts `terrarium:dev` again.

**The updater itself** is not moved by an update: it moves only the
terrarium container, and goes on with the image that it started with. To
give the updater a new build, start it again from the release image after
an update: `<dir>/terrarium-compose up -d --no-deps terrarium-updater`.
The wrapper reads the image file, and the updater image defaults to the
daemon image, so the updater gets the release image of the image file.
Set `TERRARIUM_UPDATER_IMAGE` in `.env` to pin another image. Start no
update while the updater restarts. `terrarium admin update status` shows
the build of the updater (`updater:`).

### 34.2 Settings

The daemon:

| Variable | Default | Meaning |
|---|---|---|
| `TERRARIUM_SELF_UPDATE` | `notify` | `off`: no background check and no mail (the API still checks and updates on request). `notify`: a check of the releases every 6 hours, and one Inbox mail for each new version. `auto`: `notify`, and an update inside a global calendar event of kind `maintenance` (§33), at most once for each occurrence. |
| `TERRARIUM_UPDATE_CHANNEL` | `stable` | `stable`: releases only. `pre`: pre-releases too. |
| `TERRARIUM_UPDATE_REPO` | `wiltaylor/terrarium` | The GitHub repo of the releases (`GET https://api.github.com/repos/<repo>/releases`, cached for 1 hour). |
| `TERRARIUM_UPDATER_SOCKET` | `/run/terrarium-updater/updater.sock` | The updater socket. |

The updater:

| Variable | Default | Meaning |
|---|---|---|
| `TERRARIUM_UPDATE_VERIFY` | `cosign` | The image check (§34.4): `cosign`, `gh`, `cosign-keyless`, `off`, or `command:<shell command>`. |
| `TERRARIUM_UPDATE_COSIGN_KEY` | `/opt/terrarium/release/cosign.pub` | The public key of the `cosign` check: the release key of the image (`docker/release/cosign.pub`). |
| `TERRARIUM_UPDATE_WORKFLOW` | `.github/workflows/ci.yml` | The workflow in `TERRARIUM_UPDATE_REPO` that signed the releases (`gh`, `--signer-workflow`). |
| `TERRARIUM_UPDATE_IMAGE` | `ghcr.io/wiltaylor/terrarium` | The image repository of the releases. |
| `TERRARIUM_UPDATE_REPO` | `wiltaylor/terrarium` | The GitHub repo: the identity of the signing workflow. |
| `TERRARIUM_UPDATE_DRAIN_WAIT` | `600` | Seconds that the drain waits for the running turns. |
| `TERRARIUM_UPDATE_HEALTH_TIMEOUT` | `900` | Seconds that the new container has to become healthy. The first start installs the harnesses only when the tools dir has none; an update finds them. |
| `TERRARIUM_UPDATER_TARGET` | the service `terrarium` of the compose project of the updater | The name of the terrarium container. compose.yaml passes it from `.env`; empty is the default. Set it when the compose labels of the container are lost. |
| `TERRARIUM_UPDATER_STATE` | `/updater` | The state dir: `history.json` (the last 20 runs), `previous.json` (the rollback target), and `image`. |

A bad `TERRARIUM_SELF_UPDATE` or `TERRARIUM_UPDATE_CHANNEL` stops the start
of the daemon. A bad `TERRARIUM_UPDATE_VERIFY` stops the start of the
updater.

### 34.3 An update

The updater does these steps, and records each one (`steps` of a run):

| Step | What it does |
|---|---|
| `inspect` | Finds the terrarium container and its image. |
| `resolve` | Asks the registry for the digest of the tag (anonymous pull token). The container runs that digest already: the run ends as `current`. |
| `verify` | Checks the signature of `<repo>@<digest>` (§34.4). A failure stops the run. With `off`, the step is `skipped`. |
| `pull` | Pulls the image by digest. Before the drain, so a slow or failed pull stops no work. |
| `drain` | Runs `terrarium admin update drain --wait <s>` in the container (§34.5). |
| `backup` | Runs `terrariumd backup create --json` in the container: a full backup in the backup dir (§32). Migrations only go forward; the way back needs it. |
| `stop`, `remove`, `start` | Stops the container (its stop timeout), removes it, and makes a new one with the same settings (binds, ports, networks, labels, the seccomp profile, the env of compose) and the new image. The env, command, user, and labels of the old image do not carry over: the new image brings its own. Each compose label (`com.docker.compose.*`: the project, the service, the working dir, the config files) stays, also when the old image has it: an image that `docker compose build` made carries the project and service labels, and without them the next `inspect` finds no container. |
| `health` | Waits until the health check passes, then runs `terrarium daemon status` in the new container. |

A failure before `stop` changes nothing: the updater ends the drain
(`resume`) and the old container runs on. The outcome is `failed`.

A failure from `stop` on goes back by itself (`rollback`): the updater
removes the new container, restores the backup of the `backup` step with
the old image (`terrariumd backup restore`, offline, in a one-off container
with the same binds and no network), starts the old image again, and waits
for its health. The outcome is `rolled_back`, or `rollback_failed` when the
way back fails too. Then the operator must act: the mail names the backup,
and §32.3 tells how to restore it.

After a good update, the updater writes the image file and
`previous.json`: the old image and the backup. The drain lives in the
memory of the old daemon, so the new daemon starts without it.

**A rollback** (`POST /api/system/update/rollback`) is the same switch to
the image in `previous.json`. The image is not checked again (it ran here
before); a local image that is gone is pulled by its digest. A rollback
also restores the backup that the last update made, because the newer
daemon can have migrated the database: **the data written since the update
is lost**. It stays in the backup that the rollback makes first. A second
rollback goes forward again.

One run at a time: a request while a run runs is `409 conflict`.

### 34.4 The signature check

`TERRARIUM_UPDATE_VERIFY` names a program, so that the check follows the
way the release signs its images ([release.md](release.md)):

- `cosign` (the default): `cosign verify --key <TERRARIUM_UPDATE_COSIGN_KEY>
  <repo image>@<digest>`. The local release build (`just release::publish`,
  [release.md](release.md), "The local release build") signs the image
  digest with the release key pair. cosign checks the signature and its
  transparency log entry with the public key, which the image has at
  `/opt/terrarium/release/cosign.pub` (`docker/release/cosign.pub`). The
  image has cosign 3.1.4 (pinned with its SHA-256 in the Dockerfile). A
  missing key file fails the check before cosign runs. A release that the
  GitHub workflow made has no cosign signature: this check refuses it (see
  release.md, "The switch from the workflow to the local build").
- `gh`: `gh attestation verify oci://<repo image>@<digest>
  --repo <repo> --signer-workflow <repo>/.github/workflows/ci.yml
  --bundle-from-oci`. The check of the releases that the GitHub workflow
  made: they sign only with `actions/attest-build-provenance` and push the
  Sigstore bundle to the registry ([release.md](release.md), "A release of
  the workflow"). gh reads the bundle from GHCR, and checks the signature, the
  transparency log entry, the digest, and that the certificate is of the
  release workflow of the repo. The image has gh 2.100.0 (pinned with its
  SHA-256 in the Dockerfile). gh asks for a token before it starts, also
  with `--bundle-from-oci`: when the updater env has no `GH_TOKEN` or
  `GITHUB_TOKEN`, the updater gives gh a placeholder. gh sends it to no
  GitHub API in this mode; a call that needs a real token fails, and so
  does the update. Set `GH_TOKEN` in the updater env to use a real one.
- `cosign-keyless`: `cosign verify --certificate-identity-regexp
  '^https://github\.com/<repo>/\.github/workflows/'
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
  <ref>`. For an image of your own that a GitHub workflow signs with
  keyless cosign (`TERRARIUM_UPDATE_IMAGE`, `TERRARIUM_UPDATER_IMAGE`).
  Before TER-613 this mode was named `cosign`.
- `command:<shell command>`: `sh -c <command>` with the image in
  `TERRARIUM_UPDATE_REF`. Exit 0 accepts the image.
- `off`: no check. The `verify` step is `skipped`, and the updater logs a
  warning at its start.

**No check works offline.** gh and cosign read the Sigstore trust root and
the bundle over the network (GHCR, and the Sigstore TUF repository). A check that cannot run (no
program, no network, a timeout of 3 minutes) fails, and the update stops
(fail closed). The registry digest pins the image that the check passed:
the pull and the new container use the digest, never the tag.

### 34.5 The drain

The updater drains the daemon before it stops the container. The drain:

- holds every wake: work for an agent that does not run waits
  (`lifecycle::Woke::Draining`);
- closes the stream gate for every agent: no new turn, mail pointer,
  recap, or compaction goes to a running agent (`stream_limit`);
- lets the running turns end. `terrarium admin update drain --wait <s>`
  waits until no agent is `working` or `starting`, at most `<s>` seconds.
  A turn that still runs then ends with the stop; the agent resumes its
  session in the new daemon, as after any restart;
- starts no job of the job queue: queued jobs stay queued, and running
  jobs go on (`drain.jobs` lists them). `terrarium admin update drain`
  also waits for them, in the same `<s>` seconds. A job that still runs
  at the stop is killed on its target by the new daemon and put back in
  the queue with its attempt and its place (TER-123).

The drain lives in memory. The new daemon starts without it, and the queued
work goes out at its start. `POST /api/system/update/resume` (or the
updater after an early failure) ends it and sends the work that waited.

### 34.6 The updater socket

`/run/terrarium-updater/updater.sock`: one connection carries one request
and one answer, each one line of JSON.

| Request | Answer |
|---|---|
| `{"cmd":"status"}` | `{"kind":"status", updater_version, repository, verify, current, previous, running, history}` |
| `{"cmd":"update","target":"0.3.0","actor":"operator"}` | `{"kind":"accepted","run":"<ulid>"}` |
| `{"cmd":"rollback","actor":"operator"}` | `{"kind":"accepted","run":"<ulid>"}` |
| a refused request | `{"kind":"error","message":"...","busy":false}` |

`current` and `previous` are images: `{reference, id, digest?, version?}`
(`version` from the `org.opencontainers.image.version` label). A run is
`{id, kind, actor, target, from, to, backup, outcome, error, steps:
[{name, state, detail, at}], started_at, ended_at}`. `outcome` is
`running`, `succeeded`, `current`, `failed`, `rolled_back`, or
`rollback_failed`; a step `state` is `running`, `ok`, `failed`, or
`skipped`.

On the host, the operator talks to the updater with `docker compose exec
terrarium-updater terrarium-updater status` (also `update <target>` and
`rollback`).

### 34.7 The API

The operator and a sysadmin, **agents with the `sysadmin` role too**
(`terrarium admin update` in the sandbox). Another agent gets `403`. The
drain and its end are for the operator only (the updater runs them through
the daemon's own Unix socket in the container).

#### `GET /api/system/update`

```json
{"current": {"version":"0.1.0+6e31754","cargo_version":"0.1.0", "...": "§4"},
 "policy":"notify","channel":"stable",
 "latest":{"version":"0.2.0","tag":"v0.2.0","name":"v0.2.0","prerelease":false,
           "published_at":"...","notes":"(Markdown)","url":"https://github.com/..."},
 "update_available":true,"checked_at":"...","check_error":null,
 "updater":{"reachable":true,"socket":"/run/terrarium-updater/updater.sock",
            "error":null,"status":{"...": "§34.6"}},
 "last_result":{"...": "a run"},
 "maintenance":{"key":"<event>@<start>","event":"<ulid>","title":"...","start":"...","end":"..."},
 "drain":{"on":false,"since":null,"by":null,"busy":[],"jobs":[]}}
```

- `latest` is the newest release of the channel (semver order; no
  drafts). `update_available` is true when it is newer than
  `current.cargo_version`.
- `updater.reachable` is false when no updater answers; `error` says why.
- `last_result` is the run in progress, else the newest run.
- `maintenance` is the global `maintenance` occurrence that runs now, or
  `null`.

#### `POST /api/system/update/check`

Reads the releases now, sends the new-release mail when it is due, and
answers as `GET`.

#### `POST /api/system/update`

`{"version": "0.3.0"}` (also `v0.3.0`, `latest`, or `pre`); no body: the
newest release of the channel, else the channel tag. Answers `202` with
`{run, target}`. A target that is no tag is `422 invalid`. No updater:
`503 unavailable`. A run in progress: `409 conflict`. The audit row is
`system.update` (target: the version; detail: `run`, `ok`, `error`,
`from`).

#### `POST /api/system/update/rollback`

Answers `202` with `{run}`. No previous switch: the run fails at once
(`failed`, "no switch to roll back"). The audit row is `system.rollback`.

#### `POST /api/system/update/drain`, `POST /api/system/update/resume`

The operator only. Answer `{on, since, by, busy, jobs, jobs_drain}`. The drain
writes the audit row `system.drain`.

With the body `{"jobs_only": true}` (TER-614, `terrarium admin update
drain-jobs`) only the job slots drain, for example once an update is
announced: no job of the job queue starts, the running jobs finish, and
the agents go on. `jobs` lists the running jobs; `jobs_drain` is `{since,
by}` while it is on, else `null`. The queued jobs wait. `resume` ends it,
and so does a restart (it lives in memory). A running job that must not
wait for its end is cancelled with the job cancel.

### 34.8 Mail

For a hot swap, the updater creates the full ZIP backup **before** pausing
the daemon. The backup CLI uses SQLite `VACUUM INTO` for a consistent online
database copy. The `backup` step records when backup creation began. This
ZIP is the manual pre-update restore point: writes after that time may
not be included (normally a few minutes). Other state files and logs are
copied while the service is still running, not inside the pause.

Once paused, only the database is copied to `.hot-swap.sqlite` for exact
automatic rollback to the moment of the swap. That temporary snapshot is
deleted on commit and is not part of the manual ZIP restore point. A failed
switch restores this database before restarting the old daemon. The pause
budget starts before `pause-daemon`: switching must finish before the
15-minute runner hold limit minus a one-minute recovery reserve. Exceeding
that safety deadline fails the update and attempts recovery of the old daemon.
Once admission is attempted, errors never rewind the database automatically.
During hot-swap health checks, the updater reads only startup-attempt-scoped
supervisor logs. A daemon child exit fails the health step and starts rollback
on the next poll (normally within two seconds), even if the container stays
healthy while its entrypoint restarts the child. The run records an allowlisted
error category from the last daemon error line, with all other values hidden.
It never copies raw daemon logs into update history.
Every Docker request has an absolute deadline covering connect, handshake,
request write and complete response read (at most 15 minutes outside the
shorter hot-swap budget). Recovery uses the original hold deadline, not a new
timer. The final five seconds are reserved for an emergency container stop
or ending jobs drain after verified recovery.

If recovery fails or its completion is uncertain, the update outcome is
`recovery_failed`. The updater requests an immediate manual Docker stop
(`stop?t=0`, which prevents automatic restart), records the paused SQLite
snapshot path and pre-update ZIP path, and never starts a daemon on an
unverified database. If Docker does not acknowledge the stop within the hold
budget, the run explicitly says the stop is unconfirmed and the operator must
stop the container. Verify any outstanding restore helper is stopped, then
verify/restore the database by hand before restarting. This outcome is a
failure and produces an ACTION NEEDED report, not a successful update.

The daemon sends the operator Inbox mail (from `system`):

- one mail for each new version of the channel, "Terrarium 0.2.0 is
  available", with the command (`notify`) or the next maintenance window
  (`auto`);
- one mail for each finished run of the updater, with the outcome, the
  error, the backup, and the steps. A failure that needs the operator says
  "ACTION NEEDED".

The table `self_update_state` (migration 63) records the mails that went
out and the window of the last auto-update, so a restart sends nothing
twice. At the first read of the updater history, the older runs get no
mail. The auto-update writes the window before it asks the updater: the
backup holds the mark, so a rollback does not start the same update again
in the same window.

## 35. The job queue

Each project can have a job queue (TER-71, `crate::jobs`). A job runs the
project's validation recipe on a clean clone, never in an agent's working
tree. The daemon moves the card on the result, so a worker moves on to its
next card at once.

### 35.1 The config

`GET /api/projects/{name}/delivery` (members) and `PUT` (a project admin
or the operator) read and set the config. The body is a JSON object:

| field          | meaning                                                         | default               |
|----------------|-----------------------------------------------------------------|-----------------------|
| `target`       | the target that runs the jobs                                   | none: no queue        |
| `recipe`       | the command, run with `sh -c` in the root of the clone          | none: no queue        |
| `slots`        | the most validate jobs that run at the same time                | 2                     |
| `next_repos`   | the repos (by name) that have a `next` branch for merge jobs    | none                  |
| `work_dir`     | the dir on the target for the work trees of the slots           | `/tmp/terrarium-jobs` |
| `timeout_secs` | the most time the recipe may run                                | 3600                  |
| `failure_patterns` | literal strings to extract from recipe output, with three following lines of context | `["FAILED", "panicked", "test result: FAILED", "failures:"]` |
| `merge_secrets` | `DOCKER_CI_KEY` and `DOCKER_CI_RESET_KEY` mapped to `@secret:<name>[.<field>]`, consumed only by the daemon for merge jobs | `{}` |
| `docker_ci` | public SSH settings: `destination`, `port`, `known_hosts`, `reset_destination`, `reset_port`, `reset_known_hosts` | none |
| `release_target` | the release client target of release jobs (§35.6); it runs release jobs only | none: no releases |
| `release_recipe` | the release command, run with `sh -c` in the root of the clone | `just release::publish` |
| `release_secrets` | env names mapped to same-project `@secret:<name>[.<field>]` references, the env of release jobs only; names follow the rule below | `{}` |
| `docker_release` | the release VM: `destination`, `port`, `known_hosts`, and `key` (`@secret:<name>[.<field>]`, used by the daemon only) | none |

`merge_secrets` uses the project's `delivery:<project>` principal, never
agent or project grants. A delivery grant requires `level: run`, named
targets, and no startup `env`. Worker runs, launches, secret reads, and
brokers do not consider delivery grants. No database migration is needed:
the existing grantee column stores principal strings without a type CHECK.
Both `DOCKER_CI_KEY` and `DOCKER_CI_RESET_KEY` must be present when enabled.
Literal values and arbitrary recipe
env injection are refused.
References resolve against current grants at each merge recipe, including
reruns, and fail closed on a missing grant, wrong target, or proxy-only
credential. Each use emits `secret.run` with the job, principal,
target, field, and reference name, never the value. The SSH key remains in
a private daemon run directory and is never sent to the target, recipe,
or their environment. Output is stream-redacted with the full key and
individual PEM lines before retention. The recipe retains its full raw target
log because credentials never reach the target process tree.

Configure `docker_ci` through `terrarium project delivery set`, the delivery
API, or fleet configuration before
adding merge secret references. For example, `destination` is `root@vm`,
`port` is its SSH port, and `known_hosts` is the administrator-verified
OpenSSH host-key line (including `[host]:port` for a nonstandard port).
`reset_destination` and `reset_port` identify the forced reset command;
`reset_known_hosts` supplies its administrator-verified OpenSSH pin, written
into a job-private reset directory. Neither SSH connection uses the daemon's
shared TOFU known_hosts file. No endpoint
or key pin is compiled into the daemon. Reset exit 75 retries twice with
a two-second delay; any other failure prevents opening the tunnel.
The daemon resolves these legacy-named merge secrets and opens the tunnel
only for cut-time `e2e` jobs. Merge and validate jobs never receive the
relay, regardless of their recipe's `TERRARIUM_DOCKER_CI` switch.

The Docker gate is off by default: merges continue to run the full
`ci::validate` without requiring Docker or a key. To roll it out in order:
deploy the job-runner support first, then
the secret administrator grants `terrarium/docker-ci-vm` and
`terrarium/docker-ci-reset` to
`delivery:terrarium` with `--level run --target terrarium-helios-build`.
Do not grant the key to `project:terrarium` or workers. The lead configures
`--merge-secret DOCKER_CI_KEY=@secret:docker-ci-vm` and
`--merge-secret DOCKER_CI_RESET_KEY=@secret:docker-ci-reset` on delivery.
Queue the release e2e endpoint (§35.4) after the daemon and grants are ready.
The Docker gate refuses every other job kind.
`--clear-merge-secrets` removes the references. The same
command sets `docker_ci` with `--docker-ci-destination`, `--docker-ci-port`,
`--docker-ci-known-hosts`, `--docker-ci-reset-destination`,
`--docker-ci-reset-port`, and `--docker-ci-reset-known-hosts`: a flag left
out keeps its stored value, the first set needs all six, and
`--clear-docker-ci` removes the whole object. `project delivery show`
prints the endpoints and both pins.
The daemon pins the Docker VM's public host key and owns its SSH local
forward. Trusted relay code embedded in the daemon release, never read
from the merged branch, creates a 0600 Unix socket in a job-private
directory on the target. Docker byte streams travel over its executor
session to the daemon's SSH forward. The daemon starts the relay only after
a Docker `GET /_ping` through its forward returned 200 (it retries for up to
90 seconds after the reset). The recipe gets only
`TERRARIUM_DOCKER_HOST=unix://<job socket>`, not `DOCKER_HOST`: on a
`docker` target the host agent's own `docker exec` client would read
`DOCKER_HOST` (TER-477). The gate script sets `DOCKER_HOST` from it when
`TERRARIUM_DOCKER_CI=1`. The daemon logs the reset start and result (exit
code and the last stdout line), the tunnel up or its failure, and the relay
socket path. Teardown closes the relay, socket, and
SSH child on success or failure. Other processes sharing the target OS
account can reach that socket during the job. This is an accepted risk:
it grants Docker control of the throwaway VM, not access to its SSH key.
Before opening the tunnel the daemon calls META-104's forced-command SSH
reset as `dockerci@10.3.0.2` on port 22, with the second job-only key and
strict checking against configured `reset_known_hosts` in its private RunDir.
It sends no remote command. Exit 0 permits the tunnel, exit 1 fails, exit
75 reports the reset lock busy, and a 180-second timeout per attempt fails closed.
Both keys remain daemon-side. The VM's LAN and Tailscale egress is blocked
outside the VM by vmlab switch rules, so branch code cannot undo it.
The generic stream relay
closes both directions when either peer closes. It does not preserve TCP
half-close semantics. Docker version/pull/run/log streaming is tested, but
clients requiring a write-half-close followed by a response are unsupported.
Half-close forwarding is outside this change's scope. The relay
does not filter privileged creates or host namespaces. A Docker API
authorization layer, not a byte-pattern filter, would be needed to
enforce that without breaking exec/attach upgrades and streaming APIs.
Do not enable rollout until both grants and the pinned port-22 host key
are installed and reset has been verified by the authorized operator.
The gate runs the full suite, then explicitly runs the lifecycle/workspace and
reservation/release Docker tests, rejecting ignored or unmatched results.
The create-only device-IO test remains manual: its host-side `/dev/block`
resolution requires a host agent colocated with Docker, not a remote
`DOCKER_HOST`. The separate reservation/release case runs remotely.

`failure_patterns: []` disables extraction. The patterns are literal, not
regular expressions, must not be empty or whitespace-only, and can also be
set in the project's `[delivery]` table.
Recipe stdout and stderr are merged into
`<work_dir>/<project>/logs/<job-id>-<attempt>.log` on the target. Logs are outside
the reusable slot and are not removed by later jobs. `job show` and the
card's failure block include the target and full log path. They show the
first 1800 bytes of matching lines (with context), then the last 1200 bytes
of output. A separate target session reads the excerpt after the recipe
ends, including after a timeout or cancellation. A cancelled job keeps its
cancelled state and receives its final output after draining. The recipe
keeps its inherited umask, while the log file is created privately (0600).
Each attempt has its own file, including reruns after a lost run. Logs have
no automatic retention limit.

The target must be a target of the project. A global target needs a
sysadmin or the operator. The audit row `project.delivery` keeps the old
and the new config. With no target or no recipe, the queue is off, and a
validate or a merge is a `409 conflict` that says how to set it.

### 35.2 Jobs

- `validate`: check out the card commit and run the recipe.
- `merge`: check out `next`, merge the card branch with `--no-ff`, run the
  recipe, and push `next` when the recipe passes. The push is not forced:
  when `next` moved, the job fails.
- A batch merge (TER-112) is one `merge` job for several cards of one
  project and one repo. It merges the card branches onto `next` in the
  given order, each with `--no-ff`, runs the recipe once on the result,
  and pushes `next` when it passes. `batch` in the job lists the cards
  (`[{card, branch, by_agent?, outcome?, note?}]`); `card`, `branch`, and
  `by_agent` are those of the first card. See 35.3 for the results.

The recipe gets this env (TER-100), plus any configured merge-only secrets
for a merge job. It never receives the repository credential:

| variable                  | value                                                                 |
|---------------------------|-----------------------------------------------------------------------|
| `TERRARIUM_JOB_KIND`      | `validate` or `merge`                                                 |
| `TERRARIUM_JOB_ID`        | the job ID                                                            |
| `TERRARIUM_HEAD`          | the tested commit: the card commit, or the merge commit               |
| `TERRARIUM_JOB_SLOT`      | the slot of the job (TER-190): `1` to `slots` for a validate job, `0` for a merge |
| `TERRARIUM_SLOT_LOCK`     | the absolute path of the slot's lock file (`<slot>.lock`, TER-448): the recipe holds it, with the clone step, for its whole run |
| `TERRARIUM_BASE`          | the base of the change: for a validate job the merge-base of the card commit and `next`, for a merge job `next` before the merge |
| `TERRARIUM_CHANGED_FILES` | the absolute path of a file in the clone (`.git/terrarium/changed-files`) with the paths that differ between the base and the head, one per line (`git diff --name-only --no-renames`, so a rename lists both paths) |

The prepare step in the daemon fetches `next` for a validate job too, and
the clone step on the target writes the file from that base. When the repo has
no `next`, or the commit has no merge-base with it, `TERRARIUM_BASE` and
`TERRARIUM_CHANGED_FILES` are not set, and a recipe that selects tests
must run all of them. The terrarium repo's recipe for this is `just
ci::validate-affected`: a validate job runs the checks that its changed
files reach (the rules are in `scripts/affected.py`), and a merge job runs
the full `just ci::validate` through `just ci::merge`, plus the Docker
gate only when `TERRARIUM_DOCKER_CI=1` is explicitly enabled.

Jobs start first in, first out across all projects. A job starts when:

1. its host is under its cap (the host cap, below), and
2. for a validate job, its project has fewer than `slots` validate jobs
   running.

Merge jobs have their own lane (TER-159). At most one merge job of a
project runs at a time, and the merges of a project start first in,
first out among themselves, so `next` moves one merge at a time. A merge
does not take one of the project's `slots`: validate jobs start beside a
running or queued merge, up to the slots, and never wait behind a merge.
The host cap counts the merge. Otherwise a job that must wait does not
hold back a later job that may start. A card has at most one queued or
running job. The `queue` answer of a validate or merge call gives the
place and the running count in the job's own lane; a merge gives
`slots: 1`.

The repo credential never goes to the target (TER-103). The steps that
need it run in the daemon, which runs no recipe, in a bare repo of the
job at `<state>/jobs/<project>/<job>.git` (0700 dir, removed after the
job, and all of them at daemon start). They use the daemon's own git
setup (§5.1): no user or system config, no hooks, the token through a
one-shot helper or the key in a 0700 run dir under
`<state>/credentials/.run/`, and the daemon's known hosts. A job is:

1. Prepare, in the daemon, with the credential: fetch the card branch
   (validate) or `next` and the card branches (merge). A validate job
   takes the card commit. A merge job merges each branch onto `next` with
   a merge commit, without a work tree (`git merge-tree --write-tree`,
   then `git commit-tree`), so no checkout, filter, or hook runs. The
   commit is recorded (`refs/job/head`, or `refs/job/merge`) and written
   to a git bundle.
2. On the target, with no credential: the bundle goes in on the stdin of
   the step, and the job ref is fetched from it into the work tree of the
   job's slot (below). The job ref must be the recorded commit; else the
   job fails (`error`). The tree has no remote. Then the recipe runs in
   the tree.
3. A merge job only, when the recipe passed, in the daemon, with the
   credential: push the recorded commit ID from the daemon's repo to
   `next`. The push is not forced.

Each slot has one work tree on the target, `<work_dir>/<project>/slot-<n>`
(TER-190), kept from job to job, so a build reuses the output of the
jobs before. A job takes a slot when it starts: the lowest of `1` to
`slots` that no running job of its project holds, and `0` for a merge,
so merges have a tree of their own. A cancelled job holds its slot until
its processes are gone, and no two jobs use one tree at once. The clone
step and the recipe also hold an exclusive lock on `<slot>.lock` next
to the tree (TER-448), so a second daemon on the same target with the
same project name and work dir (two daemons share the default
`/tmp/terrarium-jobs/<project>`) cannot replace the tree of a running
job: its job fails with a clear error, and the recipe checks the tested
head again under the lock. Only the job's own wrapper shell holds the
lock: the recipe runs as its child without the lock's file descriptor,
so a process that the recipe leaves running does not keep the slot
locked after the job. The lock needs `flock` on the target; without
it the steps run unlocked, as before TER-448, and each step prints
`no flock on the target: slot not locked` (the recipe also states that its
log is not tracked for retention). The job
row has `slot` while it runs. The step checks out the commit with
`git checkout -f` and removes every file that the commit does not track,
ignored ones too, except `target/` at the root (`git clean -ffdx -e
/target`). It prunes old commits with `git gc --auto`. A dir that is no
repo, a `.git` that is no plain dir or holds a symlink, or a tree that
git cannot fetch into or check out, is made new once: all but `target/`
is removed. No tree is removed after a job. A recipe that wants a clean
build removes `target/` itself.

The git state that a recipe leaves in the tree does not reach the next
job's checkout (TER-190 review). Before the fetch the step resets
`.git/config` and removes `.git/info` (with `grafts`), `.git/hooks`,
`.git/commondir`, `.git/shallow`, the alternates, the commit-graph, the
multi-pack-index and every `refs/replace/` ref, and git runs with
`GIT_NO_REPLACE_OBJECTS=1`. After the checkout the step hashes the work
tree again (a new index from the tree, `git add -A` but `target/`,
`git write-tree`) and compares it with the tree of the commit in the
daemon's repo, as it does `HEAD^{tree}`. On a miss all of the slot is
removed, `target/` too, and the step runs once more in a new tree. A
second miss fails the job (`error`, "refused: the checkout of ... is not
its tree") and removes the slot.

A lost job (one that ran when the daemon stopped) whose kill does not confirm that its
processes ended (the kill failed or left processes, the target has no
job PID files, the job ran on another host, or the delivery target is
gone) marks its slot before the slot is free. The next job of that slot
removes all of the tree first, `target/` too, and then clears the mark.
The step moves the old tree aside before it removes it, so processes
that still run there write into the old tree, not into the new one.

The recipe can change its clone: make commits, move HEAD, delete refs, or
write `.git/config` (for example `url.<base>.insteadOf`). None of that
reaches the push: the daemon's repo is the only source of the push, and
no target, sandbox, or recipe can write it. The `ls-remote` check of
`POST /api/cards/{id}/validate` also runs in the daemon.

Each step on the target runs at low priority: `ionice -c3 nice -n19
<command>`. The daemon checks one time per target (per daemon run) that
`ionice -c3` works. Without it, the steps use `nice -n19` only. The
output tail of the job ends with the wrapper (`[run as: ...]`).

The daemon must reach the forge of each repo in `next_repos`, as it does
for workspace checkouts (§5.1). A forge that only the target reaches does
not work with the queue.

#### Limits

The steps on the target run as the target's user (the ssh login user, the
`docker exec` user, or the host agent's user), as an agent's `exec` does.
They hold no credential, so a recipe, or an agent with exec on the
target, finds no key, token, or push repo there, and validate jobs can
run beside a merge (TER-159). What is left: the recipes of one target
share a user, so a recipe can change another job's clone on the same
target, or leave a process that does. A recipe can also leave files in
`target/` of its slot's tree for the next job of that slot. That can
change the verdict of that job, but not what a merge pushes: the push is the commit that the
daemon recorded before any recipe ran. A host agent must not run as the
daemon's user on the daemon's machine (§17), or a recipe can read the
state dir.

#### The host cap

The projects of one host share its disk and its CPUs. The host cap is the
most jobs of all projects that run on one host at the same time. The
daemon checks it before the slots of the project. The host of a job is
the stable ID of the delivery target's host agent (`host`), else the
resolved IP of its SSH `address` (sorted first address when there are
several, lowercase hostname when resolution fails), else the target name.
DNS is resolved outside the database lock when an API target is set or
its address changes, and at daemon startup for existing targets. Targets
from file or fleet changes and legacy cap rows are backfilled before use.
The first result,
including a failed lookup, is persisted by hostname and reused across
ticks and daemon restarts, so a changing DNS answer cannot split a cap.
SSH usernames and ports do not affect the key. An SSH target and a host
agent on the same machine count as two hosts, unless the address is an
alias of the host agent (below). Cross-transport machine identity is not
inferred. A host without a cap has no limit. The default is no cap.

A sysadmin or the operator lists the addresses of a host agent's machine
(TER-105). An SSH target whose address is one of them counts on the host
agent's key: one running count and one cap. An alias matches the target
address as written (no user, no port, lowercase), or by its pinned key
(an alias host name is pinned as a target address is, outside the
database lock). An alias belongs to one host agent, and it cannot be the
name of a host agent. The aliases go when the host agent is revoked.
Running jobs and caps stored under an alias count on the host agent at
once. When both keys had a cap, the stricter applies until a cap write
replaces them.

Builds and tests that agents start by hand (`terrarium exec`, `terrarium
run`) are not jobs of the queue. The host cap does not count them, and
they do not get the low priority.

| method and path                 | who                           | what |
|---------------------------------|-------------------------------|------|
| `GET /api/job-hosts`            | any agent, the operator       | `{hosts: [{host, name?, aliases?, heavy_jobs, running, projects, updated_by?, updated_at?}]}`: each host with a cap, a running job, or aliases. `name` is the host-agent name when the key is a host agent. `heavy_jobs` is `null` without a cap. Visibility is scoped as described below |
| `PUT /api/job-hosts/{host}`     | a sysadmin, the operator      | `{heavy_jobs}` (1 or more). The answer is the list above |
| `DELETE /api/job-hosts/{host}`  | a sysadmin, the operator      | take the cap away. The answer is the list above |
| `PUT /api/job-hosts/{host}/aliases` | a sysadmin, the operator  | `{aliases: [address]}` (at most 32): the addresses of the host agent `{host}` (name or ID). Replaces the list; `[]` takes all away. 404 for no host agent, 409 when another host agent has the alias or its pinned key, 422 for invalid aliases (including a host-agent name, case-insensitive). The answer is the list above |

The audit row `job.host_cap` keeps the old and the new cap. A job keeps
the host it started on (`host` in the job).

Host-cap writes accept a host-agent name or a resolved key. SSH names
are resolved in the same way as target addresses. `GET /api/job-hosts`
also includes `projects`, a map of project names to running counts.
The operator, sysadmin and architect see all project counts and cap audit
fields. Other agents see the total `running` count and only their own
project's count (including zero). Their response omits `updated_by`,
`updated_at`, and `aliases`. No job records are returned.

Previously stored host names and address aliases are normalized when
counting running jobs and reading caps. If several old caps resolve to
one key, the strictest applies until a cap write replaces them.

### 35.3 Results

New daemon recipe logs have a per-attempt ownership ledger. Unconfirmed legacy
logs and release-client logs are not adopted or deleted. Delivery config accepts
`log_retention_secs` (default 604800, seven days, capped at ten years) and
`log_retention_bytes` (default 1073741824, one GiB of completed logs per project).
The CLI sets these with `project delivery set --log-retention-secs N
--log-retention-bytes N`. Zero expires logs immediately or retains no completed
bytes respectively. Age is fixed when an attempt is first recorded. Changing
the byte budget affects the next cleanup pass.

Without `flock`, recipe log capture fails closed before running the recipe. Tracked logs
use a shared directory lock and a writer-held inode lock. Log roots must be
absolute physical paths without symlink components. Cleanup pins the recorded
directory and verifies its device/inode and the log inode. A private quarantine
protects a replacement pathname from deletion. Target creation identity,
definition (including address/container/VM), and lifecycle timestamp are bound
to ownership. Changed or recreated targets are skipped, not adopted.

Automatic cleanup visits one project per minute. Each pass first reconciles
an indexed page of at most 32 quarantining rows, then considers an indexed
page of at most 32 retained rows. Oldest recorded attempts are removed when their age
expires or the completed-log byte budget is exceeded. Queued/running jobs and
locked log writers are never removed. Missing or unreachable targets are
skipped and retried on later passes. Budgets are eventual, not a limit on an
active recipe's output. Cleanup never scans log directories.

`terrarium job cleanup [--after N]` (project admin) applies the same policy
explicitly, via `POST /api/projects/{name}/jobs/cleanup` with `{"after": N}`.
The response has `scanned`, `removed`, `skipped`, `conflicts`, and `next`. A nonzero `next`
continues the pass, and zero ends it. Concurrent cleanup returns a conflict.
`GET /api/jobs/{id}` includes `logs`, with attempt, target, path, written_at,
expires_at, bytes and expired. `job show` explicitly marks removed logs as
expired while preserving the job's stored output tail.

`terrarium admin job legacy-logs <target> [--older-than 30d] [--after JOB_ID]`
is an operator-only dry run for the selected project's current delivery log root.
Agents, including project admins, and link keys are refused. It probes exact
daemon filenames (`<ULID>-<attempt>.log`) from at most 256 indexed job records,
not a directory scan. Only terminal non-release jobs ended before the cutoff
and without a ledger row qualify. Unrelated filenames and files without a job
record are untouched and are not enumerated or included in skip counts.
The response lists paths, jobs, attempts, size, mtime, identities, total bytes,
skip counts by reason, and an independent `next` cursor. Missing, symlinked,
locked or unreachable files/roots are skipped. Log roots must be absolute,
physical paths without symlink components.

`--confirm DIGEST` is an explicit operator attestation of ownership, not an
inference from job output tails or current configuration. The server stores a
single-use listing digest, valid for 15 minutes, binding the observed paths,
root/file inodes, size, mtime and target identity. Confirmation rechecks these
observations and job states. Changed/new files are skipped. An expired, consumed
or unknown digest is refused. Only confirmed unchanged entries enter the
existing ownership ledger and durable lock/quarantine removal protocol.
Each adoption and confirmed removal is audited. Interrupted removals use the
existing recovery path, with bytes charged until confirmed expiry and changed
size/mtime retained as a conflict. Scheduled maintenance never lists or adopts
legacy logs. The API is `POST /api/projects/{name}/jobs/legacy-logs` with
`target`, optional `after`, `older_than_secs` (default 2592000), and `confirm`.

Before moving a log, cleanup commits its `quarantining` state and deterministic
`quarantine` path in the ledger. Interrupted moves are reconciled on later
passes, even when the original pathname is absent. Bytes stay charged until
the owned inode has been removed (or both recorded locations are already gone).
If a captured replacement cannot be restored because its pathname is occupied,
both files are preserved and the row becomes `conflict`. Such rows are never
retried automatically. `job show` and cleanup's bounded `conflicts` list expose
the paths for manual inspection and removal. Recovery has its own persisted
cursor, so inaccessible quarantines cannot starve later rows.

- Pass: the labels `validation:queued` and `validation:running` go away.
  A validate card moves to `review`, a merge card to `accepted`. The move
  comment gives the job, the commit, the tested head, the exit code, and
  the times.
- Fail: the job runs once more, at the front of the queue. A merge
  conflict is not run again and is never resolved. After the second
  failure, a block `Validation failed: ...` with the output tail goes to
  the top of the card body. The card moves to the top of `ready` with no
  assignee, and the worker of its `by:<agent>` label gets a mail from
  `system`.
- After a daemon restart, host-agent jobs retain their running slot and
  resume their recorded phase. A live recipe is reattached, not killed or
  requeued, with its exit code and output replayed. The slot's result files
  are not wiped. SSH and exe.dev noninteractive runs and queue recipes
  reattach to their detached spool. PTY/shell sessions and stdin-fed steps
  remain interrupted, with reason `interrupted (ssh transport)`.
- A host that cannot be reached is not a failure (TER-160). A job whose
  target's host agent is offline is not started and stays queued. A job
  whose target could not be reached before the recipe started (the host
  agent went away, or the ssh connect was refused, timed out, or the name
  did not resolve) goes back to the queue with `tail` `waits for its host:
  <why>` and the event change `requeued`. An ssh target is tried again
  after 60 s. Neither uses an attempt or moves the card, and the card
  keeps `validation:queued`. After 30 min of waiting the card gets one
  comment that says so (once per job in a daemon run).
- A recipe run lost to the infrastructure is not a failure (TER-196).
  Only an end of the recipe itself (an exit code, a signal, its timeout)
  is `red`. A run with no exit code (the host agent no longer holds the
  session, the host went offline, the run process died), a run that the
  host agent ended because its link to the daemon was down, or an ssh run
  whose link to the target broke (exit code 255 and an ssh client line
  that names the target host) is lost. An error end that a rerun gets
  again (`spawn_failed`, `path_denied`, `unsupported`, `bad_request`,
  `refused`) is `red`. What may still run of it on the target is killed first,
  and the slot's tree is made new when the kill is not confirmed. Then
  the same job goes back to the queue with its attempt and its place,
  with `tail` `the infrastructure lost the run after the recipe started
  (<why>); ...` and the event change `requeued`. A batch merge reruns
  whole and does not split. The card does not move. A job reruns this
  way at most 3 times in a row in one daemon run: the next lost run gets
  the same kill (and the slot mark), then it is `red`, and its `tail`
  starts with `the infrastructure lost the run 4
  times in a row, so this run counts as red: <why>`. A merge job ends
  `infra` instead (below).
- A merge job that failed on its host before its tests (TER-614) is not a
  test failure. It does not split, does not rerun at once, and uses no
  attempt. Two paths. An end with outcome `error`, no tested commit
  (`head`), and a failed step `receive`, `secrets`, or `recipe` (the
  recipe did not start) goes back to the queue (event change `requeued`)
  and runs once more after 5 minutes, and not while its host agent is
  offline (a host agent that comes back online ends the wait early); each
  card gets a comment that says so. A second such end ends the job with
  outcome `infra` (verdict `infra`). A run lost past the cap above ends
  `infra` at once, with no delayed retry. An `infra` end has no rerun, no
  split, and no failure block: its cards lose the job labels and stay in
  the merge queue with no job, with a comment. They go in the next batch
  when a card enters the merge queue or the lead queues them. The retry
  mark lives in memory: a daemon restart gives at most one extra run.
- A failure in the daemon before the tests (`setup`, `prepare`) stays
  `error`. A card branch that the daemon cannot fetch (deleted or
  rewritten after the accept) or merge leaves the merge as a conflict does:
  the failure block, the top of `ready`, a mail to its worker; the other
  cards of the batch go on.
- Each failed end has the step that failed as the last line of its `tail`:
  `[failed step: <step>]`, one of `setup`, `prepare`, `receive`, `secrets`,
  `recipe`, `push`. Each card gets a comment with the failed step and the
  last 20 lines of the tail, indented. The daemon writes it in both board flows, after each `error`
  or `infra` end and at the first infra end. A `red` end has the output in
  its failure block.

A batch merge:

- A card whose branch does not merge onto `next` and the cards before it
  leaves the batch: its merge is aborted, and the next card merges. A
  merge that fails without a conflict (for example an unrelated history)
  is the same: the tree is reset, and the card's note says why. The card
  gets `outcome: conflict` in `batch` and fails at once, as above. When
  no card merges, the job ends as a `conflict`.
- A merge conflict onto `next` also queues one durable repair request per
  card and job. After the existing conflict-return flow, an open bound
  card worker receives a turn with the conflict details. Otherwise the
  daemon spawns a bound worker using the card type's template (default
  `card-impl`) under a non-ephemeral project admin. Normal spawn rights,
  template grants and WIP checks apply, with no additional grants or targets.
  The task starts from the card's branch: merge `origin/next` with a merge
  commit, keep both sides' intended changes, run the card check and return
  to review. No rebase or force push. Duplicate verdicts do not duplicate
  workers. Requests survive daemon restarts and retry refusals after a minute.
  Release and e2e jobs, and merges onto release branches, do not queue repairs.
- Pass: each merged card moves to `accepted`.
- Red with two or more merged cards: the batch splits into two halves (a
  half of one card is a job of one card). Both are queued at the front,
  with `parent` set to the red job. The halves split again on red, until
  the failing card is found. The other cards merge. Each card gets a
  comment with the jobs of the split.
- The order holds over nested splits: `split_path` in the job adds `0`
  for a left half and `1` for a right half to the path of its parent,
  and the queue sorts on it after `created_at` and `attempt`. So the left
  half and all its splits run before the right half. One merge of a
  project runs at a time, so the right half merges onto a `next` that
  holds the cards of the left half that passed.
- A card that a split leaves alone and that is red fails without the
  rerun: the red batch was its first red. A batch that ends with one
  merged card and red follows the rule of one card.
- Any other failure (`error`) runs the batch once more with the merged
  cards, then fails each of them.

The failed tests of the full suite run once more in a release validation
(TER-683), in the recipe, so the run stays one run. The daemon takes no
part in it: it reads nothing of it from the job log, and files no card.

- The switch: `TERRARIUM_VALIDATE_CONTEXT=release` (the full suite at the
  version cut, [release.md](release.md)), or `TERRARIUM_JOB_KIND=merge` (a
  merge job). Either one makes `ci::validate` run the uid 1001 pass
  (`ci::test-unprivileged`) after `ci::test`, and makes both passes rerun.
  Without either (a card check, a validate job) nothing reruns.
- When `ci::test` (the root pass) or `ci::test-unprivileged` (the uid 1001
  pass) fails, `scripts/rerun_tests.py` takes the names of the failed tests
  from the libtest output (the console output, or the libtest JSON when
  `RESULTS_DIR` is set) and runs exactly those tests once more: the same
  cargo command with the test names and `--exact` (and `--tests`, so no
  doctest), or for uid 1001 the same test binary as the same uid, from the
  same directory, with the same env and a new private HOME. With
  `RESULTS_DIR` the rerun writes `rust-rerun.json` and
  `rust-unprivileged-rerun.json`; `rust.json` keeps the first run.
- Every rerun test passes: the step passes, and the suite goes on. One
  fails again, does not run, or the rerun ends otherwise: the step fails.
- No rerun, the step fails at once, unless every failure is a failed test.
  The script counts each test run that cargo starts (`Running`,
  `Doc-tests` on its stderr) or that it starts itself (uid 1001), and each
  end: cargo itself ending with a status other than 101 (a signal or any
  other code), a build error or any other cargo `error` line, a test run that ended
  with a signal or an exit code other than 101, a run that has no test
  result (a crash or a panic before `running`), a failed run with no failed
  test result, a failed doctest, or more than 5 failed tests in the step.
- With `RESULTS_DIR`, each step that failed writes its entry to
  `$RESULTS_DIR/reruns.json`: `{version: 1, context, steps: [{step,
  outcome, reason?, tests: [{name, first, rerun, tail}]}]}`, `step` `test`
  or `test-unprivileged`, `outcome` `green`, `red` or `no_rerun` (with the
  `reason`), `first` `failed`, `rerun` `ok`, `failed`, `missing`, or `null`
  with no rerun, `tail` the end (1500 bytes) of the first failure's output.
  The names and the results come from the script's own count of the runs
  it started, never from a line that a test prints. `ci::validate` removes
  the file when it starts.
- The flaky-test cards come from that file:
  `just ci::flaky-cards "$RESULTS_DIR/reruns.json" "<the run>"`
  (`scripts/flaky_cards.py`) prints, for each test with `rerun: ok`, the
  `terrarium` commands for its card: a comment on the open card with the
  label `flaky-test:<hash of the name>` (not in `done`), or a new card
  `Flaky test: <name>` in `backlog` with the labels `flaky`,
  `flaky-test:<hash>`, `version:vnext` and `type:small`. It runs nothing.

The card writes of the queue are made as the operator. Their comments
start with `Job queue:`.

### 35.4 Endpoints

`POST /api/cards/{id}/e2e` is project-admin-only. Its body is
`{branch:"release/X.Y.Z", repo?, eta?}`. The card must be an open version
group header. Docker CI delivery configuration and run grants are required.
It returns `201 {job}` with kind `e2e`, pinned to the remote branch head.
The job shares the merge lane but never merges or pushes. It runs
`TERRARIUM_DOCKER_CI=1 python3 scripts/docker_ci.py`, using the same
daemon-owned reset/tunnel handling. No key reaches recipe code. Merge and
validate jobs receive neither the relay endpoint nor Docker credentials.
The group's `version branch e2e` step records the job and its outcome.
Failure or cancellation marks the step failed without changing membership
or moving the header. Queue it again after a release fix. A card validate
job still cannot enter the Docker gate. See [release.md](release.md).

| method and path                    | who                                   | what |
|------------------------------------|---------------------------------------|------|
| `POST /api/cards/{id}/validate`    | the card's assignee, a project admin  | `{commit?, repo?, branch?, comment?, eta?}`. `eta` is the planned duration (§35.5). The commit must be the head of the branch on the remote, read with `git ls-remote` in the daemon. In one write: the labels `by:<agent>` and `validation:queued`, the move to `validating`, and the job. The worker stays assigned; the final move (a pass, or the second failure) clears the name. `201` `{job, queue: {position, running, slots, note}}` |
| `POST /api/cards/{id}/merge`       | a project admin                       | `{repo?, branch?, eta?, onto?}`. `eta` is the planned duration (§35.5). `onto` is `next` (the default) or a release branch `release/X.Y.Z` (see the release-blocker fix below). The repo must be in `next_repos`. The card stays where it is until the job ends. `201` as above |
| `POST /api/cards/{id}/release`     | a project admin                       | `{repo?, commit?, eta?}` (§35.6): a release job at the head of `main` on the remote; `commit`, when given, must be that head. The project needs `release_target`. The card does not move. `201` as above |
| `POST /api/merges`                 | a project admin                       | `{cards, repo?, eta?}`: a batch merge of one or more cards, in this order (no limit of the daemon; the batch rule of the merge queue sets its own, §19.11). All cards are of one project and one repo (the given one, else each card's, else the project's only repo), which must be in `next_repos`. The branch of each card is its branch, else its ID in lower case. Each card gets `validation:queued` and a comment, and stays where it is until the job ends. A card twice, or a card with an open job, is refused. One card is the same as `POST /api/cards/{id}/merge`. A batch of two or more cards with a `release-blocker` card is refused with `409`: that card merges alone. `201` as above |
| `GET /api/projects/{name}/jobs`    | members                               | `?state=`. The config, the `running` and `queued` counts, and the jobs, newest first (200 at most) |
| `GET /api/projects/{name}/jobs/stats` | members                            | `?since=`: an RFC 3339 time, or a duration back from now such as `7d` (the default); anything else is `400`. The jobs created at or after `since` (a rerun keeps the `created_at` of its first try): `{project, since, jobs, first_attempt, validate, merge, reruns, conflicts, errors, cancelled, open, splits, failed_steps: [{step, failed}], failed_targets: [{target, failed, first_attempt}], untargeted_test_fails}`. Each of `first_attempt`, `validate`, `merge`, and `reruns` is `{ended, passed, failed, pass_rate}`; `pass_rate` is `passed / ended` from 0 to 1, `null` with none ended. A first attempt is attempt 1 and no split half; a `conflict` counts in `conflicts`, not in `ended`. A job with outcome `error` could not run (host agent offline, prepare step failed, push to `next` failed): first attempts and reruns with it count in `errors`, not in `ended`, so an outage does not lower the rate. Split halves count in `splits` only. `failed_targets` (most first) come from the `error: N targets failed:` lines of the failed jobs' output tails, all attempts; `failed_steps` from the first ``error: recipe `<step>` failed`` line, or `timeout`. `untargeted_test_fails` counts the failed `test` steps whose tail names no target (the 4000-byte tail clip cut the header off) |
| `GET /api/jobs/{id}`               | members                               | one job with its output tail |
| `DELETE /api/jobs/{id}`            | the creator, the card's worker, a project admin | cancel. A running job is killed. The card stays in its column without the job labels. A batch merge: the creator or a project admin only, as it holds the cards of several workers; every card of the batch loses the job labels |

A held card (the GitHub author gate, §8.3) gets no validate or merge
job, from any caller (`403`).

A merge takes the reviewed commit of each card (TER-553): the head of the
card branch that the daemon recorded when the card moved into the merge
queue. A card in the merge queue without a reviewed commit for the branch
of the merge is refused with `409` (TER-580): the head is not recorded
yet, the commit is of another branch (a `branch` in the body, or a branch
changed after the accept), or the card has no round (a column got the
`merge_queue` role while it held the card). A card outside the merge
queue merges the head of its branch on the remote at queue time.

The daemon reads the head right before the move, and in the background
after it when that read failed. The background read fills a round only
within 10 minutes of the accept; after that the round needs a fresh
accept. The rounds that the TER-553 upgrade opened for the cards already
in the merge queue need one too (TER-580): they hold no reviewed commit.
For each such round the card gets one `Job queue:` comment with the
reason. To go on, review the head of the branch, then move the card out
of the merge queue and back in.

**Upgrade step (TER-580).** After the upgrade, each card that was in the
merge queue at the TER-553 upgrade and has not left it since stays out of
merges until it is moved out of the merge queue and back in.

**A release-blocker fix (TER-607).** A frozen version keeps its members.
A fix for it goes in a card with the label `release-blocker`, on a branch
made from the release branch `release/X.Y.Z`, not from `next` and not on
top of another card.

1. The batch rule of the merge queue takes the first card of the column
   with the label that could go, alone and at once, as a batch of one.
   The other cards wait for the next batch. There is no preemption: a
   merge job that is queued or running already, and the halves of a red
   batch that splits, still run before the blocker. A `card merge` (`POST
   /api/merges`) of two or more cards with a `release-blocker` card is
   refused with `409`.
2. After its merge onto `next` passed, `card merge <id> --onto
   release/X.Y.Z` (`onto` in the body; one card, a project admin) merges
   the commit that this merge took onto the release branch, and pushes
   only that branch. `main` then gets the release merge as usual. The
   job never takes the head of the card branch: a push after the merge
   onto `next` does not reach the release. A card without a passed merge
   of its branch onto `next` is refused with `409`.
3. The job checks what the fix brings into the release. The tested
   commit must be on `next`. Let M be the oldest commit of the
   first-parent line of `next` that holds the tested commit (the merge
   of the fix onto `next`). No commit of `release/X.Y.Z..tested` may be
   in `release/X.Y.Z..M^1`, the history of `next` before the fix landed
   that the release does not have. So a fix branch made from `next`
   after the cut is refused, and so is a fix stacked on, or merging, a
   card that merged onto `next` before the fix: both would take
   unreleased cards into the frozen version. The job then fails with
   outcome `error`, the release branch stays, and the tail names the
   commits. Limit: a fix that merges a card branch that is not on `next`
   yet passes, since its commits cannot be told apart from the fix's own
   commits. Those commits then reach `next` in the same merge as the fix.

The repo defaults to the card's repo, then to the project's only repo. The
branch defaults to the card's branch, then to the card ID in lower case.
Events `job` {change, project, id, card, kind, state, attempt, outcome,
cards?}; `cards` lists the cards of a batch.

### 35.5 The ETA of a job

TER-155 D2: the ETA of a job starts when the job runs, not when it is
queued.

- `eta` in a validate or merge body (`card validate --eta`, `card merge
  --eta`) is a planned duration such as `20m` or `1h30m`: more than zero,
  at most 30 days. An RFC 3339 time is `422 invalid`. The job stores it as
  `planned_secs`.
- While the job is queued, its cards have no ETA (queueing clears it), and
  the card's `job` gives `place` and `queued` ("queued n of m").
- When the job starts, each card of the job gets `eta_at` = `started_at`
  + `planned_secs`, set by the actor that queued the job. The card gets an
  `eta` event with `job: true`.
- A rerun, a split half, and a job that goes back to the queue (§35.3)
  keep `planned_secs`: they clear the ETA again, and their start sets it
  again.
- No `eta`: the job sets no ETA, and the card has none while it validates
  (the move to `validating` cleared it).
- An ETA duration that is given later for a card with an open job (`card
  eta`, `card move ... --eta`) sets the job's `planned_secs` (§8.2).
- The job's end moves the card, and the move clears the ETA (§8.2).
- Cancelling a job with a planned duration clears its cards' ETAs too.

Events `job.verdict` {project, job, card, kind, verdict, state, outcome,
attempt, by_agent, repo, branch, parent, batch, end_comment,
failure_block}: one per card when a job ends or is cancelled, in the same
write as the card flow above. `verdict` is `passed`, `red`, `conflict`,
`cancelled`, or `error` (the job could not run, or the daemon restarted).
In a batch each card has its own event: a card that left the batch is
`conflict`, the merged cards have the verdict of the batch. `batch` lists
the cards of a batch job, else `null`. `end_comment` is the `Job queue:`
line of the end. `failure_block` is the `Validation failed:` block for
`red`, `conflict`, and `error`, else `null`. The verdict says what the run
found, not what the queue does next: the rerun and the split stay as
above.

### 35.6 Release jobs

A release job (TER-623, kind `release`) is the lead's release step. The
release config (`release_target`, `release_recipe`, `release_secrets`,
`docker_release`) is the operator's: only the operator sets, changes or
clears it. Any other actor gets `403` for a change of these four fields and
may still set the other delivery fields. A project admin queues a release on the card of the release with `POST
/api/cards/{id}/release` or `terrarium job release <card>`. It takes the
head of `main` on the remote (the release merge commit), clones that commit
on `release_target` into a new tree, and runs `release_recipe` there
(default `just release::publish`, docs/release.md). Releases have their own
lane: one at a time per project, on the host of the release target, beside
the merge and validate jobs. The card does not move and gets no label: it
gets a comment when the job is queued and when it ends (with the output on
a failure). A release emits `job` events but no `job.verdict`, so the board
rules do not act on it. A release whose run is lost (the daemon stopped, or
the link to the target broke) ends as an error and never runs again by
itself: its push or GitHub release may be done, so the lead checks them and
queues a new one.

The release secrets:

- Names match `^[A-Z][A-Z0-9_]{0,63}$`. Job and tool environment names
  are reserved: `PATH`, `HOME`, `USER`, `SHELL`, `PWD`, `IFS`, `ENV`,
  `BASH_ENV`, `TMPDIR`, `LANG`, `NODE_OPTIONS`, `DOCKER_RELEASE_KEY`,
  `DOCKER_HOST`, `DOCKER_CONFIG`, and prefixes `LD_`, `DYLD_`, `TERRARIUM_`,
  `DOCKER_CI_`, `GIT_`, `SSH_`, `CARGO_`, `RUST`, `PYTHON`, `NPM_CONFIG_`,
  `JUST_`. The secret grant name restrictions also apply. Values must be
  references to secrets of this project, never literals or cross-project paths.
- `release_secrets` resolve through the `delivery:<project>` principal
  with a `run` grant that names the release target, only for a release job,
  only on that target. A grant that names another target fails the job
  before its recipe and before the tunnel. Merge and validate jobs never
  resolve them: they run on `target`, and the code path reads them for
  release jobs only.
- The values go into the env of the release recipe only. The executor sends
  the env on stdin, never on a command line. The recipe output, the
  retained log excerpt and the tail are redacted with each value. Each use
  emits `secret.run` with the job, `kind: release`, the target, the env
  name and the reference, never the value.
- `release_target` is not the `target` of any project, and not the release
  target of another project; a project's `target` is not another project's
  release target. The check compares machines too: a target on the same
  host agent, or with the same ssh host and port, counts as the same.
  Every process on that target that runs as the release user can read the
  secrets of a running release, so the target runs release jobs only:
  - No agent may use it: exec, shell, copy and the screen are refused, also
    for an agent whose policy lists it or that holds a lease.
  - No policy may list it, no lease, lease extension or reservation may
    name it, and a spawn may not take it, whoever asks (also the operator
    and a project admin). A fleet file that lists it has an error.
  - A target that an agent's `policy.targets` lists, or that has an active
    lease, cannot become a release target.
  - The operator keeps exec and shell on it, to set it up.
  - A release target is a global target. Only the operator creates,
    changes or deletes it, or a target on its machine.
  - The machine counts too: a target on the same host agent, or with the
    same ssh host and port, is refused for agents, policies, leases,
    reservations, spawns and tools as the release target is.
  - No tool may name the release target or its machine, a tool run there
    is refused, and a target that a tool names cannot become one.
  - The lifecycle verbs (`test`, `up`, `down`, `recreate`, `status`, the
    snapshots) on it are the operator's.
- The release recipe first removes the HOMEs that a killed release left
  (`terrarium-release-home.*` under `TMPDIR`, else `/tmp`, owned by the
  release user), and runs with the fixed `PATH`
  `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`: no dir
  under the release user's home. The release tools are installed
  system-wide on the client.
- The release recipe runs in a new HOME: a mode-0700 dir from `mktemp -d`
  (under `TMPDIR`, else `/tmp`), removed when the recipe ends, with the XDG
  config, cache, data and state dirs in it, `GIT_CONFIG_GLOBAL=/dev/null`,
  `GIT_CONFIG_NOSYSTEM=1`, and no `DOCKER_CONFIG`, `CARGO_HOME`,
  `RUSTUP_HOME` or `UV_CACHE_DIR`. So no `~/.docker`, `~/.gitconfig`,
  `~/.config` or cache in the release user's home is read. `XDG_RUNTIME_DIR`
  stays. The login shell of the ssh user still reads its own start files
  (for example `~/.bashrc`) before the job: they belong to the operator's
  setup of the target, which no agent can reach.
- `docker_release` is the release VM. The daemon resolves its `key` (with
  the same grant rule), keeps it in a private run dir, and opens the SSH
  forward with the pinned `known_hosts` line, as for `docker_ci` (§35.1),
  with no reset. The recipe gets only `TERRARIUM_DOCKER_HOST=unix://<job
  socket>` through the relay on the release target, never the key.

`terrarium project delivery set` sets them with `--release-target`,
`--release-recipe`, `--release-secret NAME=@secret:name[.field]` (repeat;
replaces the list; `--clear-release-secrets`), and
`--docker-release-destination`, `--docker-release-port`,
`--docker-release-known-hosts`, `--docker-release-key` (all four at the
first set; `--clear-docker-release`).

## 36. Linked instances: link keys

Another Terrarium daemon (the **home**) can show this daemon (the
**remote**) in its dashboard and act on it (TER-49, wiki page
`design/linked-instances`, approved 2026-11-06). This section is the remote
side (TER-61): the instance ID, the link keys, their pairing, and the rights
of a link. The home side (the proxy, `/api/remotes`) is TER-62.

The link goes one way: the home calls the remote. A link key works only on
the TCP listener. The remote's API port is plaintext: put a reverse proxy
with TLS in front of it, as for `terrarium --url https://... login`.

### 36.1 The instance

`GET /api/system/instance`: `{id, name, version}`. Every caller may read it.

- `id`: a ULID, made at the first start in `<state>/instance.id` and kept.
  A file that holds no ULID is an error; the daemon never replaces it, so
  the ID that a home pinned does not change. A restore onto a new host
  keeps the file with the state dir.
- `name`: the host of `TERRARIUM_PUBLIC_URL`, else the host name.
- `version`: the crate version (as `cargo_version` of `GET /api/system/version`).

### 36.2 Link rows and pairing

Remote artifact previews use `POST /api/remotes/{name}/artifacts/{id}/url`
with an operator session. The home returns a signed `/ra/...` capability on
its artifact origin, valid for at most 120 seconds and invalidated by logout
or password changes. Fetches send no home cookies, tokens, or link keys.
Targets must use the API's scheme and either its origin or the artifact
origin declared by `GET /api/system/instance` (`artifact_origin`). Redirects
are refused, responses are limited to 32 MiB and supported MIME types, and
served content has a sandbox CSP. Download/Open links stay on the home.

The remote artifact origin must present the linked API's pinned leaf
certificate. An independent certificate or missing HTTPS leaf pin fails
closed: remote artifacts do not load. Separate artifact-origin pins are
not supported yet. Private destinations are allowed only for a remote
linked with a private IP literal or `localhost`. DNS-named remotes have a
public-only artifact policy, independent of `TERRARIUM_WEBHOOK_PRIVATE`.
Each artifact connection resolves once, checks the IP address, and pins
the connection to that checked address. Private DNS labs must link using
their private IP literal to opt into private artifact destinations.

Only the operator manages links (`403 forbidden` for an agent). A link key
calls none of these routes except `DELETE /api/links/self` (§36.4).

| Route | Body | Answer |
|---|---|---|
| `GET /api/links` | `?revoked=true` adds the revoked links | The links, newest first. |
| `GET /api/links/{id}` | | One link. |
| `POST /api/links` | `{label, read_only?}` | `201` `{link, code, code_expires_at, instance, url}`, `Cache-Control: no-store`. |
| `POST /api/links/{id}/rotate` | | `{link, code, code_expires_at, instance, url}`. The old key stops now (§36.3). |
| `DELETE /api/links/{id}` | | `204`. The key and an open code stop, and the open streams of the key close. |
| `DELETE /api/links/self` | | `204`: a link key ends its own link (the home's Remove). Another caller gets `400`. |
| `POST /api/links/pair` | `{code, home_instance_id, home_name?}` | `200` `{link_id, label, read_only, key, instance}`, `Cache-Control: no-store`. |

A link: `{id, label, read_only, status, home_instance_id, home_name,
paired_at, paired_from, created_by, created_at, rotated_at, first_used_at,
last_used_at, last_used_from, pair_expires_at, revoked_at}`. `status` is
`pending` (no key: a code is open, ran out, or the link was rotated),
`active`, or `revoked`. A revoked link keeps its row without hashes, so the
audit rows of its actor keep their label. `label`: 1 to 64 characters, no
control characters. `url` is `TERRARIUM_PUBLIC_URL` or `null`.

Pairing, as the host agent enrol (§17.1):

1. On the remote, the operator makes a link (`terrarium admin link new
   <label> [--read-only]`, or Settings, Links). The answer has a one-time
   **pairing code**: `tlp_` and 64 hex digits, good for 10 minutes.
2. On the home, the operator gives the remote's URL and the code (TER-62).
3. The home calls `POST /api/links/pair` with the code and its own instance
   ID. The call needs no `Authorization`: the code is the proof. One
   database update swaps the code for the long-lived **link key** (`tlk_`
   and 64 hex digits), so of two calls with one code only one wins. The key
   is in this answer only; it never passes through the operator's
   clipboard.
4. The home compares `instance.id` (and the TLS certificate it saw) with
   what the remote's operator sees.

A wrong, used, or expired code, and a code without the `tlp_` prefix, all
get the same `401 unauthorized`. `home_instance_id` must be a ULID
(`422 invalid`), and not the remote's own ID. The daemon keeps only the
SHA-256 of the code and of the key; neither is in a log, an event, or an
audit row.

### 36.3 Rotate and revoke

- `POST /api/links/{id}/rotate`: a new pairing code. The old key stops at
  once and its open streams close, so a rotate after a leak cuts the key
  now. The home takes the new code with its Re-pair.
- `DELETE /api/links/{id}`: the key stops. A call that is in flight with the
  key (a long poll, an SSE stream, a WebSocket of the events, a PTY, or a
  target shell) ends at once: the request holds the key's revocation signal
  from before the key lookup. A pending answer becomes `401`, a stream ends,
  a WebSocket drops.
- A revoked or rotated key gets `401 unauthorized`, as a wrong token.

### 36.4 The rights of a link

The runtime link-access registry is the fail-closed boundary. Every admitted
method and path pattern has an explicit link access class declared with its
route. The outer TCP authentication layer checks this registry before API
routing or handlers. Unregistered methods/paths and operator-only entries
return `403 link_forbidden`. Plain axum routes, nested mounts, macros, helpers
and cfg'd routes do not gain link access without a registry declaration.
The shared operator gate and the deny list below are additional restrictions.
Link credentials are detected before the public-path bypass, including on
WebSocket upgrades. Installer, artifact-serving, static-file and SPA fallback
entries have the explicit `PublicDenied` class: they remain public for anonymous
and ordinary operator requests, but a link-key request gets the link 403.
This preserves the rule that a link key's authority covers only `/api`.

A link key is `Authorization: Bearer tlk_...` (on a WebSocket upgrade also
the subprotocol `bearer.tlk_...`). For the rules the caller is the operator,
minus a deny list. Operator-only gates deny link keys by default, except
the GET/HEAD remote-dashboard reads explicitly listed in
`links::operator_route_allowed`. Audit rows and events of its actions carry the actor
`link:<id>`; `GET /api/audit` adds `link_label` to those rows. Mail, card
authors, and pins stay `operator`. `GET /api/session` gives `kind: "link"`.

The deny list (`403 link_forbidden`):

- The credentials of the remote: `POST` and `DELETE /api/session`,
  `DELETE /api/sessions`, `/api/auth/...`, `POST /api/secrets/{scope}/{name}/reveal`,
  `/api/agent/secrets/...`, the writes of `/api/credentials`,
  `POST /api/hosts`, `DELETE /api/hosts/{name}`,
  `/api/hosts/{name}/rotate`, `/api/subscriptions/login...` (§16.4), and
  `/api/subscriptions/{name}/api-key`.
- `/api/links/...`, except `DELETE /api/links/self`: a link cannot make,
  list, or pair another link.
- The backups (§32): `POST /api/backups` and `GET /api/backups/{name}`. A
  backup holds `secrets.key`, `session.key`, and every sealed secret.
- The daemon itself: `/api/system/update...` (status, update, check,
  rollback, drain, resume).
- The config repo push (§25): `POST /api/config/push`,
  `POST /api/config/sync`, and the writes of `/api/config/remote`.
- Executable automation, which runs later as `system`, its owner, or the
  project inbox, outside the link's checks: `POST /api/schedules`,
  `PATCH /api/schedules/{id}`, `.../run`, `.../enable`, `.../test`, and
  `.../deliveries/{d}/redeliver`; the writes of `/api/projects/{name}/rules`
  (`PUT`, `DELETE`), `.../rules/migrate`, and `.../rules/{rule}/enable` and
  `/disable`. Reads, `DELETE /api/schedules/{id}`, `.../disable`, and the
  rule `check`, `dry-run`, and `lint` stay open.
- The shared text that agents load at start: every write (`POST`, `PUT`,
  `PATCH`, `DELETE`) of `/api/system-prompts...` (the system prompt
  library and its personas; also `replace: true`), `/api/skills...`, and
  `/api/templates...` (also `.../from-agent/{agent}`), and
  `POST /api/config/defaults/accept` and `/keep`. A sysadmin agent with no
  own prompt loads the library entry `leader` at start. Reads stay open.
- Any path outside `/api` (`/install/...`, `/hosts/connect`, `/a/...`).

A link also may not reach the denied routes through a sysadmin agent
(`403 link_forbidden` too): it may not grant or revoke `sysadmin` (`PUT
/api/agents/{id}/roles`, an agent create or a spawn with that role), and
may not drive an agent that holds `sysadmin`: no turn (`.../send`), keys,
answer, PTY attach, `PATCH /api/agents/{id}`, compact or new session with
`then`, spawn for it, or mail to it: `POST /api/mail`, a reply
(`/api/mail/{id}/reply`, `/api/projects/{name}/inbox/{id}/reply`), or the
note of `POST /api/mail/threads/{thread}/resume`, when one recipient is a
sysadmin agent. A refused reply or resume changes nothing. Beyond these
routes, every message that a link request writes is checked where it is
delivered (`store::mail::send`): a question answer, a notice ack text, a
retract reason, a to-thread message, a thread message or response, and any
notice that the request makes. The release of a held message
(`POST /api/mail/{id}/release`, and a resume with or without a note) is a
delivery too, checked in `store::mail::release` before any change. A message to a sysadmin agent (also through a
project inbox whose admin is one) fails the whole request with
`403 link_forbidden`, and nothing of it is kept. So a link may not move a
card or end a thread when that mails a sysadmin agent. Reads, start, stop, pause, and interrupt of such an
agent work.

The check decodes the path, drops empty and `.` segments, and ignores case,
so no other spelling of a denied route passes; a path with `..` is denied.
A deny list hit writes a `link.forbidden` audit row (`{label, method, path,
read_only, from, reason}`), at most one a minute per link, method, and path.

A **read-only** link (`read_only: true`) may only `GET` (and `HEAD`): reads,
SSE, and the WebSocket of an `.../events` route. No PTY, no target shell,
no turn, no write, and no `DELETE /api/links/self`.

### 36.5 The use record and the audit

- The remote writes `first_used_at`, `last_used_at`, and `last_used_from`
  (the peer address; a reverse proxy shows as itself) at the first call of
  a key, at a new address, and at most once a minute after that.
- Audit rows: `link.new`, `link.rotate`, `link.revoke` (`by_self` when the
  link ended itself), `link.pair` (actor `link:<id>`, with `home`,
  `home_name`, and `from`), and `link.forbidden`. No row holds a code or a
  key.

## Agent calls during daemon restarts

The agent CLI assigns a stable `request_id` to each tunneled HTTP call. The
runner holds new calls after a daemon `hold` control frame and resumes them
after the next daemon's `hello`. The default queue limit is 15 minutes. At
that limit the caller receives `unavailable` (the tunnel is down).

Graceful shutdown stops admission before draining calls. Reads and long
polls receive `retry`, which the CLI resends with the same request ID. Writes
already admitted finish, including saving their answers. After 30 seconds
the daemon logs and audits a restart deferral. It waits up to
the updater's `TERRARIUM_UPDATE_DRAIN_WAIT` (default 600 seconds),
measured from the start of the drain, before exiting. New calls remain held
during this interval. Keep this cap below the 15-minute runner queue. Long
`terrarium exec` calls may hold the drain until persistent exec sessions land
in TER-263, which is outside this change.

Write request IDs are scoped to the agent and bound to a fingerprint of the
request. A reused ID with different parameters is rejected. Completed write
answers are stored in SQLite and returned without executing the write again,
including after a daemon restart. Concurrent copies wait for the first call.
Legacy callers without request IDs are drained but cannot safely replay.
Secret reads (`POST /api/agent/secrets/env`, `/api/agent/secrets/{name}/get`,
and `/api/secrets/{scope}/{name}/reveal`) are also retried without journaling.
These are audited reads despite their HTTP verb. Their secret-bearing answers
must not be stored in the restart journal.
For other writes whose answer declares `Cache-Control: no-store`, the first
caller receives the answer, but the journal stores only an `indeterminate`
marker. Replays fail closed without repeating the write or retaining sensitive
answer content.

An unresolved (`started`, NULL answer) journal entry after a crash or the drain
cap yields `indeterminate`, never another execution. The error includes the
request ID and HTTP method and says "the call may or may not have happened;
check before you retry". A non-NULL answer marks `done`. Each indeterminate
outcome is logged and
audited. External effects and the answer journal are not one transaction:
this policy prevents duplicate execution, but cannot promise a successful
answer for an interrupted external write. Journal records have no automatic
expiry, so an old request ID cannot silently become executable again.

## 37. Linked instances: home remotes

Only the local operator may manage or use `/api/remotes`. Agents, including
sysadmin agents, and incoming link actors are refused before body parsing.

| Method | Route | Body or result |
|---|---|---|
| GET | `/api/remotes` | Remote rows, without keys or ciphertext |
| POST | `/api/remotes` | `{name,url,instance_id,code,pin_sha256?,ca_pem?,label?}` |
| GET | `/api/remotes/{name}` | One remote row |
| DELETE | `/api/remotes/{name}` | Best-effort upstream revoke, then local deletion. `{revoked,revoke_on_remote}` tells the operator whether to revoke manually |
| POST | `/api/remotes/{name}/pair` | `{code}` replaces a rotated link key |
| POST | `/api/remotes/{name}/repin` | `{pin_sha256?,ca_pem?}` replaces TLS trust settings after an instance check |
| GET/HEAD/POST/PUT/PATCH/DELETE | `/api/remotes/{name}/proxy/api/{rest}` | Forward an allowed remote API call |

The name matches `[a-z][a-z0-9-]{0,31}`. The URL is an origin, with no
userinfo, path, query or fragment. HTTPS uses system roots, supplied PEM
roots, or a SHA-256 leaf pin. A pin without a CA overrides certificate name
and expiry, as for host links. HTTP is restricted to loopback, including the
resolved address. Each operation resolves once, fixes that address, and
disables redirects and environment proxies. `TERRARIUM_WEBHOOK_PRIVATE=deny`
also refuses private addresses and the daemon's own listener ports.

Before pairing, compare the remote's instance ID and leaf fingerprint on
its Links screen and supply the confirmed values. Pairing consumes the
one-time code on the remote. A mismatched returned ID is refused, with no
subsequent calls. Keys are sealed with `secrets.key` and AAD `remote:<id>` in
the separate `remotes` table. They cannot be listed, revealed or granted as
normal secrets. Master-key rotation and interrupted-rotation recovery cover
these rows, including a database containing only remote keys.

The proxy applies the link deny list in §36 before forwarding. Encoded
separators, nested percent escapes, dot segments and ambiguous paths are
refused. It forwards only `Accept`, `Content-Type`, `Last-Event-ID` and
`If-None-Match`, adding the link bearer and `X-Terrarium-Link`. Home cookies,
tokens, CSRF, Origin and forwarding headers do not pass. WebSocket query
tokens are removed. The remote still enforces its link actor, read-only
scope, sysadmin restrictions and delivery guard.

Response headers are restricted to Content-Type, X-Next-Cursor, Retry-After,
ETag, Cache-Control and attachment-only Content-Disposition. Only JSON, SSE
and octet-stream bodies pass. Cookies, redirects and active content such as
HTML are refused or discarded. Responses carry `X-Terrarium-Remote`, the
home API CSP and `nosniff`. Connection errors use fixed text, with
`502 remote_unreachable` or `502 remote_mismatch`. Revoked keys give 401 and
set `auth_failed`. No upstream error containing credentials is logged.
Read-only links cannot call `DELETE /api/links/self` under the P1 policy.
Removing one therefore returns `revoke_on_remote: true` and leaves revocation
to the remote operator. The home does not widen that policy.

Limits per remote are 64 upstream requests and 8 upstream streams.
Each home admits at most 32 remote browser WebSockets,
with admission retained for the entire socket lifetime. A 33rd receives 429
with Retry-After. Historical event page reads take upstream request permits.
The home stores at most 64 remotes. Listings use bounded, name-cursor pages
with `limit` and `cursor`, and duplicate names use an indexed existence check.
Other proxy limits are a 30 s
connect timeout, 60 s ordinary request timeout, 5 min stream idle timeout,
16 MiB JSON responses and 256 MiB uploads or binary downloads. SSE streams
incrementally. Event WebSockets share one upstream firehose across browser
tabs, with independent historical cursors and filters. Reconnects resume at
the last upstream sequence with bounded backoff. Lagged browser queues
close so the browser reconnects using its own cursor. PTY sockets and scoped
agent event streams remain independent. Removing, re-pairing or re-pinning
closes existing home streams. Remote frames never enter the home event log.

Instance IDs are checked before upstream operations and every new stream,
and on a 10 min background interval. Remote rows include `status`,
`last_ok_at`, `last_error` and UTC `calls_today`. Status changes emit a local
`remote` event. Status values are `offline`, `online`, `version_skew`,
`auth_failed`, `pin_mismatch` and `mismatch`. Writes produce `remote.call`
audit rows with method, path, status and duration, never bodies or keys.

# Read positions

`POST /api/read-positions/{kind}/{item}` is operator-only, including refusal
of link keys. `kind` is `thread` (Inbox thread ID), `mail` (mail thread ID), or
`card` (card ID). The item must exist. Body: `{"through":"<RFC3339 timestamp>"}`,
the newest post timestamp in the displayed snapshot, not the browser clock.
Future and invalid timestamps return 400. Missing items return 404.
The response is `{"previous":null|"<timestamp>","read_at":"<timestamp>"}`.
The previous position is returned and the position is advanced atomically,
never backwards. Positions survive browser/device changes. The current
daemon has one operator identity (`operator`), shared by its operator sessions.
Storage is one indexed upsert row per operator, kind, and item, without
per-visit events or audit rows. This does not change mail delivery/read receipts.

The dashboard records the opening snapshot once per page opening, including
card side panels, and keeps the returned previous position across live reloads.
It inserts “New since your last visit” before the first post newer than that
position and scrolls there, unless an explicit permalink has a scroll target.
There is no line on a first visit or when no posts are newer. Empty conversations
have no cursor until their first post. Posts arriving after the opening snapshot
remain new on the next visit.


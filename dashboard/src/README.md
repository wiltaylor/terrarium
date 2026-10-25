# Terrarium dashboard

The operator dashboard for Terrarium. It is a SolidJS app that Vite builds.
The build goes to `dashboard/dist`. The daemon embeds that folder with
`rust-embed`. See `docs/PLAN.md` §17 for the API and the views.

## Run it

Install the dependencies once:

```sh
npm ci
```

Run the dashboard with fake data. You do not need the daemon:

```sh
VITE_MOCK=1 npm run dev     # or: npm run dev:mock
```

Open http://localhost:5173. A `mock` pill shows next to the logo.

Run the dashboard against a daemon:

```sh
npm run dev
```

Vite proxies `/api` (HTTP and WebSocket) and `/a/` to `http://localhost:8080`.
Set `TERRARIUM_DAEMON_URL` to use a different daemon. `just daemon::run`
listens on `127.0.0.1:8090`, so use:

```sh
TERRARIUM_DAEMON_URL=http://127.0.0.1:8090 npm run dev
```

Build-time settings (Vite reads them from the environment or `.env`):

| Variable | What it does |
|---|---|
| `VITE_MOCK=1` | Fake data in the browser, no daemon. Adds the `/dev/xss` page. |
| `VITE_API_BASE` | The daemon origin, when it is not the page origin. Also goes into the CSP. |
| `VITE_ARTIFACT_ORIGIN` | The artifact origin, for example `https://artifacts.example.com:8081`. Signed `/a/...` paths go on this origin, and the CSP allows it in `frame-src`, `img-src`, and `connect-src`. |
| `VITE_ALLOW_QUERY_TOKEN=1` | Dev only. A refused bearer socket retries with `?token=`. The daemon refuses `?token=` unless `TERRARIUM_WS_QUERY_TOKEN=1`. |
| `VITE_META_CSP=1` | Keep the CSP `<meta>` tag in a production build, for a static host that sends no CSP header. |

Other scripts:

| Script | What it does |
|---|---|
| `npm run build` | Builds to `dist/`. The mock is not in this build. |
| `npm run test` | Runs the unit tests with Vitest. |
| `npm run check` | Runs `tsc --noEmit` on the app and the config, then ESLint with the Solid plugin. |

## Authentication

The token is `TERRARIUM_TOKEN` from `.env`. The dashboard sends it to the
daemon once, at sign-in, and does not keep it (docs/api.md §1.1,
docs/security-review.md F3).

### Sign-in flow

1. With no credentials, the app shows the token screen.
2. **Sign in** sends `POST /api/session` with `Authorization: Bearer <token>`.
3. The daemon sets the `terrarium_session` cookie (`HttpOnly`,
   `SameSite=Strict`, `Path=/`, and `Secure` on HTTPS). It answers
   `{"csrf", "expires_at"}`.
4. The app keeps the `csrf` value and `expires_at`. It drops the token.
5. Each request after that goes with `credentials: "same-origin"`, so the
   browser sends the cookie. Each `POST`, `PUT`, `PATCH`, and `DELETE` also
   sends `X-CSRF-Token: <csrf>` (`api/http.ts`). No request carries
   `Authorization`.
6. **sign out** (sidebar footer, or the Settings page) sends
   `DELETE /api/session` with the CSRF header. Then the app removes every
   local copy and shows the token screen.

The state machine is in `stores/auth.ts`. Its modes are `none`, `session`,
and `bearer`:

- A 401 in `session` mode means that the cookie ended. The token screen then
  shows **over** the current page. The router stays mounted, so the route and
  the page state stay. After the sign-in, the event socket reconnects at
  once.
- The store also ends the session by itself at `expires_at`. On load it drops
  a stored session whose `expires_at` has passed.
- The Settings page (`/settings`) shows the mode and the time the session
  ends.

### What is stored where

| Mode | Memory | `sessionStorage` | `localStorage` |
|---|---|---|---|
| Session | CSRF value, `expires_at` | `terrarium.session`: `{"csrf", "expires_at"}` | `terrarium.session.remembered`, the same value, only on **Remember on this device** |
| Bearer fallback | token | `terrarium.token` | `terrarium.token.remembered`, only on **Remember on this device** |

- The cookie is the browser's business. Script cannot read it. The daemon
  sets its life (`Max-Age`), and the browser drops it at `expires_at`.
- **Remember on this device** keeps the CSRF value in `localStorage`. A new
  tab is then signed in until the session ends. Without the box, a new tab
  asks again. (The cookie is still in the browser, but a tab without the CSRF
  value cannot make a mutating call.)
- The CSRF value alone is not a credential. Without the cookie it does
  nothing. Script on the page can still read it and make calls while the page
  is open, but it cannot take the token or the cookie away.
- A token that an older build left in storage starts the app in bearer mode.
  On load, `upgradeToSession` trades it for a session, and the token leaves
  the browser.

### Bearer fallback

A daemon without sessions answers `POST /api/session` with `404` (or
`405`). Then the app keeps the token as before: in memory and
`sessionStorage`, and in `localStorage` on opt-in. The sidebar shows a
**bearer mode** chip, and the Settings page says why. Each request sends
`Authorization: Bearer`, and sockets offer the `bearer.<token>`
subprotocol. Each load tries the session again, so an updated daemon moves
the app to cookie mode without a new sign-in.

### WebSocket authentication

A browser cannot set an `Authorization` header on a WebSocket. Every socket
(agent PTY, login PTY, target exec console, events) opens through
`AuthSocket` in `api/sockets.ts`. The URL never carries a token.

- **Session:** the client offers no subprotocol. The browser sends the
  cookie and the page's `Origin`. The daemon takes the upgrade when the
  cookie is good and `Origin` is the dashboard's own origin. The socket mode
  is `cookie`.
- **Bearer fallback:** the client offers the subprotocol `bearer.<token>`
  (`new WebSocket(url, ["bearer." + token])`). The daemon echoes it.
  `AuthSocket` checks the echo on `open` and closes the socket if it does not
  match.
- Dev only: when `VITE_ALLOW_QUERY_TOKEN=1` and a bearer handshake is
  refused (closed with 1006 before `open`), the socket tries once more with
  `?token=`. The daemon refuses `?token=` by default
  (`TERRARIUM_WS_QUERY_TOKEN=1` turns it on).

The Vite dev proxy keeps the page's `Host` header (`changeOrigin: false` on
`/api`). The daemon compares `Origin` with `Host`, so a cookie upgrade
through the proxy works.

### What the daemon must do

- `POST /api/session`: take `Authorization: Bearer`, set the cookie, and
  answer `{"csrf", "expires_at"}`. A wrong token is `401`. A daemon without
  sessions answers `404`.
- `DELETE /api/session`: clear the cookie.
- Take the cookie on every `/api` route. Refuse a cookie `POST`, `PUT`,
  `PATCH`, or `DELETE` without a matching `X-CSRF-Token` (`403`).
- Take the cookie on a WebSocket upgrade only with a matching `Origin`.
- Answer `401` for a cookie that is wrong or ended. The app then asks for
  the token again.
- Keep `Authorization: Bearer` working, for the CLI and for the fallback.
- Serve the dashboard and `/api` on one origin. The client sends the cookie
  only to its own origin (`credentials: "same-origin"`), so cookie mode does
  not work with `VITE_API_BASE` on another origin.

### The mock

`VITE_MOCK=1` speaks the same contract (`api/mock/index.ts`, `api/mock/ws.ts`).
The token is `mock-operator-token`; the token screen shows it.

- `session.create` sets an in-memory "cookie". The mock keeps it in
  `sessionStorage` key `terrarium.mock.cookie`, so that a reload keeps the
  session as a browser does. This is mock only; a real cookie is `HttpOnly`.
- Every call needs the cookie or the bearer token, else `401`.
- A mutating call with the cookie and without the right CSRF value is `403`.
  The mock reads which calls mutate from the real client
  (`api/mock/verbs.ts`), so the two cannot drift.
- A socket without a subprotocol opens when the cookie is good. The mock
  refuses `?token=`.
- `createMockApi({ sessions: false })` acts as a daemon without sessions.
- Without the `client` option (the unit tests), the mock lets every call in,
  as the Unix socket does.

`api/mock/parity.test.ts` checks the contract against the real transport.

## Layout

```
src/
  api/            typed client, transport, socket protocols, mock
    types.ts      domain types (PLAN §5)
    http.ts       fetch transport: cookie + CSRF or bearer, JSON, errors (ApiError, AuthError)
    client.ts     the Api interface and the HTTP implementation (PLAN §17)
    sockets.ts    PTY and events protocols, reconnecting event stream, AuthSocket
    mock/         in-memory Api, seed data, fake PTY
    index.ts      picks mock or HTTP from VITE_MOCK
  stores/         all app-wide state (auth, projects, agents, events, board, ...)
  components/     shared UI: Terminal, UsageTile, StatusBadge, ArtifactViewer, SafeHtml, ...
  features/       one folder per view
  lib/            pure helpers: format, markdown and wikilinks, sanitize (DOMPurify),
                  artifact origin, force layout
  styles/         global.css with the theme variables
  app/            router, layout, token screen
```

## Conventions

- Put each view in its own folder under `src/features/<feature>/`.
- Keep global state in `src/stores` only. `src/stores/index.ts` creates the
  app instances. Do not keep mutable state at module level anywhere else.
- Write each store as a factory (`createXStore(api, ...)`). Tests build their
  own instance with the mock API.
- Put logic that has no UI in pure functions (`moveCard`, `applyAgentEvent`,
  `buildQueue`, `renderWiki`). Test those functions directly.
- Import the API through `api` from `src/stores`. Do not call `fetch` in a view.
- Add each new endpoint in three places: the `Api` interface, `createHttpApi`,
  and `createMockApi`. TypeScript fails the build when one is missing.
- Use the CSS variables in `styles/global.css`. Do not hard-code colours.
  The theme follows `prefers-color-scheme`. `data-theme` on `<html>` overrides it.
- Keep the layout usable at 400 px wide.

## The API contract

`docs/api.md` is the contract. The client in `api/client.ts` follows it, and
the mock in `api/mock/` answers in the same shapes. `api/mock/parity.test.ts`
checks the shapes that differ from PLAN §17.

- **Lists.** The daemon answers a JSON array and puts the next cursor in the
  `X-Next-Cursor` header. `Transport.page()` returns `{items, next}`. Methods
  that return `Page<T>` (mail, inbox, audit, events, sessions, card events,
  cards) give the cursor to the view, and the view shows **Load more**
  (`components/paged.ts`). Methods that return `T[]` follow every cursor with
  `collectAll` (`lib/paging.ts`).
- **Errors.** The body is `{"error": {"code", "message", "details"?}}`.
  `ApiError` carries `code` and `details`. `ErrorBanner` takes any thrown
  value and shows `details`, for example `blocked_by`, a WIP limit, or the
  wiki `stale_base` diff. A route that the daemon does not have yet
  (`404 no such API route`, or `405`) shows "Not available yet".
- **202 answers.** `start`, `stop`, and `restart` answer with the agent as the
  daemon accepted the call: the old status. Do not put that body into the
  store. The status change comes from the event stream.
- **Agent keys.** `{id}` is a ULID, `project/name` (sent as
  `project%2Fname`), or a unique name.
- **Replies.** `mail.reply` and `inbox.reply` answer an array: one message
  for each participant.
- **Artifacts.** An artifact has no URL. The viewer mints a short-lived
  signed URL with `POST /api/artifacts/{id}/url`.

## Live updates

`stores/events.ts` holds the one firehose socket. `createWsEventStream`
reconnects with backoff (0.5 s, doubling to 15 s, with jitter) and resumes
with `?since=<last seq>`, so a drop loses no event and a repeated frame is
dropped. Before the first connect, the store asks `GET /api/events/latest`.
When the daemon has it, the first connect uses `since = latest - 300`: the
backlog is the last 300 events only. When the daemon does not have it, the
first connect has no `since`, and the daemon sends the whole log. The agents
store uses the backlog for the last event of each agent.
While the socket is down, the layout shows a **Disconnected** strip with a
**Reconnect now** button.

Every view listens through `events.on(...)`: agent status badges, the
current card, and the self-report (`applyAgentEvent`), the board (`card` and
`board` events), the Mail attention count and the inbox counts in the sidebar
(`stores/attention.ts`, `stores/unread.ts`), the mail log (`features/mail/feed.ts`), the quota banner and the hold badges
(`stores/quota.ts`), the subscription list (`quota.*` events) and the detect
banner (`subscription.offer`), the home queue and usage strip, the Event
log, and the agent's Events tab. `lease` events keep the lease lists on the
Targets and agent pages live, and the "holds <target>" chips next to the
agent status badges (`stores/leases.ts`, `lib/leases.ts`).

## Quota alerts and holds

`stores/quota.ts` keeps the state of docs/api.md §16.6 from the event
stream. The logic is pure: `applyQuotaEvent`, `activeAlerts`, `holdsFor`.

- `quota.alert` shows a banner at the top of each page, one line for each
  window. `exhausted` replaces `alert`. The operator can dismiss a line. A
  new level or a new reset period shows it again.
- `quota.sample` removes an alert when its window reads under `alert_at`.
- `quota.blocked` puts a **quota hold** badge on the agent (agents grid,
  board, agent page). `quota.resumed` removes it.
- The agent page also reads `GET /api/usage/me?agent=`, for holds that are
  older than the event backlog.

A `self_report` event is the agent's own word (`terrarium status set`). The
agent cards and the agent page show it as **says ...**, apart from the runner
status badge. It does not change the runner status.

The terminal reconnects with backoff when its socket drops. It reconnects at
once when the agent starts again. The daemon's first frame after a connect is
a fresh snapshot of the screen; the terminal clears itself before it draws.

## Socket protocols

PTY sockets (`/api/agents/{id}/pty`, `/api/subscriptions/login/{id}/pty`,
and `/api/targets/{name}/sessions/{id}/ws`):

- The server sends raw terminal output. Binary frames are preferred.
- The client sends JSON text frames only:
  `{"type":"input","data":"..."}` and `{"type":"resize","cols":N,"rows":N}`.

`@xterm/addon-attach` reads the output. The `Terminal` component sends the
input and resize frames. The terminal is read-only until the operator turns
input on.

Event sockets (`/api/events`): the server sends one JSON `AgentEvent` in each
text frame. `agent` is the agent name. `data.agent_id` carries the ID.
A `status` event carries `data.status`. A `card` event carries `data.card_id`.

## The mail log

The operator reads mail only when something needs them. `/mail` is a log of
every message (agent to agent, agent to operator, operator to agent, `system`,
project inboxes), newest first. A row shows the time, from and to, the
projects, the subject, the first line, and chips: unread (by the recipient),
the state (`queued`, `delivered`, `read`, `replied`), read by, the notice
kind, held, and thread paused. A click opens the thread under the row. The
filter state is in the URL (`?project=&kind=&q=&range=&attention=0&thread=`),
so Home links to `/mail?thread=<id>`. A row to a project inbox links to
`/p/<project>/inbox?id=`.

**The attention rule** (`features/mail/log.ts`, `attentionReasons`). A message
needs the operator when:

- it goes to the operator and is `queued` or `delivered` (`unread`). A
  `system` notice adds its kind, from the daemon's subjects: `<CARD> blocked:`
  (`blocked`), `Quota:` or `Budget:` (`quota`), `Thread <id> needs a human`
  (`needs_human`);
- the loop guard held it, and the thread is still paused (`held`);
- it is the newest message of a paused thread (`paused`).

A read (`POST /api/mail/{id}/read`, which sets `read_by`) ends `unread`. A
reply from the operator on the thread ends the pause. The "Needs attention"
toggle (on by default), the sidebar count on Mail, and the mail items of the
Home queue use this one rule. Unread project-inbox mail is not in the rule:
the Inbox count in the sidebar shows it.

A message row does not say that the loop guard held it. The `mail` event
does (`held: true`); `stores/attention.ts` keeps those IDs. For older rows,
`heldIds` finds them: the daemon sends its "needs a human" notice right after
the first held message, and holds every later agent-to-agent message on the
thread until the operator or a project inbox writes on it.

**Server or browser.** `GET /api/mail` answers oldest first, so a newest-first
log reads the full list (`collectAll`, pages of 1000, at most 20 pages), and
the view shows 50 rows at a time (**Load more**). A `mail` event asks only for
the messages after the newest ID (`?cursor=<id>`, the list is `id > cursor`)
and reads the event's thread again (`?thread=`).

| Filter | Where |
|---|---|
| To operator | Server: `?to=operator` |
| From operator | Server: `?from=operator` |
| System | Browser. The daemon answers `?from=system` with `404 no agent \`system\`` (`store/mail.rs` `resolve`). |
| Agent to agent, project inbox | Browser: the daemon has no kind filter. |
| Project | Browser: a message has no project field. The project comes from `project/agent` and `project:<name>`, and from the card prefix or budget scope in a `system` subject. |
| Text search, time range | Browser |
| Needs attention | Browser: held and paused need the whole thread. |
| Thread (an open row) | Server: `?thread=` |

The browser filter also repeats the server part, so a thread read for a live
update cannot put other rows in the view. The mock answers the same params
(`api/mock/index.ts` `listMail`): `id > cursor`, `to`, `from`, `thread`,
`state`, and the 404 for `system`.

## Pages and the API

| Route | View | API calls |
|---|---|---|
| `/` | Home: usage strip, waiting-on-you queue; the tile detail has the usage split by project, agent, model, and card | `GET /api/subscriptions`, `GET /api/usage?bucket=hour&group=subscription`; tile detail: `GET /api/usage?subscription=&group=project\|agent\|model\|card`, `GET .../quota`; per project: `versions?status=in_review`, `wiki?status=ready_for_review`, `board`; mail items: the attention store (see "The mail log") |
| `/projects` | Projects: list, create | `GET`, `POST /api/projects` |
| `/p/:project/settings` | Project edit and delete, Repos panel | `GET`, `PATCH`, `DELETE /api/projects/{name}?confirm=true`, `GET`, `POST .../repos`, `POST /api/repos/{id}/checkout` |
| `/agents` | Agents grid, new agent | `GET`, `POST /api/agents`, events socket |
| `/agents/:id` | Agent detail: edit, delete, roles, quota, leases, terminal, events, sessions | `GET`, `PATCH`, `DELETE /api/agents/{id}`, `start/stop/restart/send`, `GET`, `PUT .../roles`, `GET /api/usage/me?agent=`, `GET .../leases?all=`, `WS .../pty`, `GET .../events?since=`, `GET .../sessions` |
| `/subscriptions` | Subscriptions, detect banner, add wizard | `GET`, `POST`, `DELETE /api/subscriptions[/{name}]`, `GET .../detect`, `POST .../{name}/probe\|verify\|logout\|api-key`, `POST /api/subscriptions/login`, `GET`, `DELETE .../login/{id}`, `POST .../login/{id}/input`, `WS .../login/{id}/pty` |
| `/mail` | Mail log: every message, newest first, filters, the attention view (default), the thread inline, mark read, reply, compose | `GET /api/mail?to=&from=&thread=&cursor=`, `POST /api/mail`, `POST /api/mail/{id}/read`, `POST /api/mail/{id}/reply` |
| `/p/:project/inbox` | Project inbox, make card | `GET .../inbox`, `POST .../inbox/{id}/read`, `POST .../inbox/{id}/reply`, `POST .../cards` (`from_mail`) |
| `/p/:project/board` | Board, drag and drop, card drawer | `GET .../board`, `PATCH /api/cards/{id}`, `POST .../comments`, `GET .../events`, `DELETE /api/cards/{id}` |
| `/p/:project/memory` | Memory list, detail, graph | `GET .../memories`, `GET /api/memories/{project/MEM-n}/graph` |
| `/p/:project/wiki/*path` | Page tree, render, edit, history, create | `GET .../wiki`, `GET`, `PUT /api/wiki/{project/path}`, `revisions`, `diff`, `restore`, `backlinks`, `review` |
| `/p/:project/artifacts/:id?` | Artifact viewer, upload, delete | `GET .../artifacts`, `.../artifacts/usage`, `POST .../artifacts` (multipart), `POST /api/artifacts/{id}/url` |
| `/p/:project/versions/:id?` | Version timeline, review, new version | `GET`, `POST .../versions`, `POST /api/versions/{id}/submit|approve|reject|release`, `compare` |
| `/p/:project/spend` | Gone: redirects to `/` (`app/nav.ts` `MOVED_ROUTES`). The daemon's budget alert mail still links it. | none |
| `/targets` | Targets, lifecycle, exec console, leases (grant, extend, revoke, history) | `GET /api/targets`, `POST /api/targets/{name}/up\|down\|snapshot\|restore\|status`, `POST .../exec`, `WS .../sessions/{id}/ws`, `DELETE .../sessions/{id}`, `GET`, `POST .../leases`, `PATCH`, `DELETE .../leases/{agent}` (docs/api.md §17.7) |
| `/hosts` | Host agents: status, capabilities, add with a one-time token, revoke | `GET`, `POST /api/hosts`, `DELETE /api/hosts/{name}` (new API, not in docs/api.md yet) |
| `/events` | Event log with history | `GET /api/events?since=&kind=&project=&agent=`, events socket |
| `/audit` | Audit log | `GET /api/audit?actor=&action=&project=` |
| `/settings` | Session: mode, end time, what the browser keeps; sign out; theme | `DELETE /api/session` |
| `/dev/xss` | Mock only: the hostile payloads of `lib/xss-payloads.ts`, rendered | none |

Artifact upload sends a multipart form: `file`, `name`, `kind`, and
`attach_kind` with `attach_ref`. A folder goes up as one `bundle`: the
browser builds a tar (`lib/tar.ts`) from the folder pick.

The Targets and Hosts pages end with a footer that names the calls that are
new or mock-only. A daemon without them answers "Not available yet".

## Security notes

See `docs/security-review.md` F3, F4, and F5.

### Untrusted HTML (F3)

- Wiki pages, version notes, and markdown artifacts come from agents.
  `lib/markdown.ts` escapes raw HTML and allows only safe URL schemes. Tests
  cover this.
- DOMPurify (`lib/sanitize.ts`, pinned in `package.json`) runs after the
  renderer, at the sink. `setSafeHtml` is the only way HTML reaches the DOM:
  the `Markdown` and `SafeHtml` components call it. It appends a
  `DocumentFragment`, so the browser does not parse the markup a second time.
- ESLint forbids the HTML sinks outside tests (F14): `innerHTML`,
  `outerHTML`, and `srcdoc` as an assignment (also `el["innerHTML"]`), in
  `Object.assign` or `Reflect.set`, and as a JSX attribute (also Solid's
  `prop:innerHTML` and `attr:srcdoc`); `setAttribute("srcdoc", ...)`;
  `insertAdjacentHTML`, `createContextualFragment`, `setHTMLUnsafe`,
  `parseHTMLUnsafe`, `DOMParser` and `parseFromString`; `document.write`; and
  `eval`. `lib/sink-lint.test.ts` lints one sample of each form and checks
  that the rule flags it.
- The DOMPurify config allows only the tags and attributes that the renderer
  emits. It forbids SVG, MathML, `style`, and every event handler. Links may
  be `http:`, `https:`, `mailto:`, or relative; the check reads the URL after
  it drops whitespace and control characters. An `<img>` may also take a
  base64 PNG, JPEG, or GIF `data:` URL. An external link gets
  `target="_blank" rel="noopener noreferrer"`; other links lose `target`.
  Class names must be ones the renderer makes.
- Memory bodies, card bodies and comments, and mail bodies render as text
  nodes. The browser does not parse them as HTML.
- `lib/xss-payloads.ts` lists hostile payloads. `lib/sanitize.test.ts`
  checks that each one comes out inert from the renderer plus DOMPurify, and
  from DOMPurify alone. In mock mode, `/dev/xss` and the wiki page
  `security/xss-payloads` show them, and a memory, a card comment, and a mail
  carry one, so you can look at them.

### Artifacts (F5)

- The viewer gets a signed URL from `POST /api/artifacts/{id}/url`. When the
  daemon answers a path (`/a/...`), the client puts it on
  `VITE_ARTIFACT_ORIGIN`. Previews then load from the artifact origin.
- When the URL is on the dashboard origin, the viewer shows the badge
  "artifacts share the dashboard origin — set TERRARIUM_ARTIFACT_LISTEN".
  When the URL is on an origin that the build's CSP does not allow, the
  viewer says which `VITE_ARTIFACT_ORIGIN` to build with.
- HTML artifacts, bundles, and PDFs render in `<iframe sandbox="allow-scripts">`
  without `allow-same-origin`. There is no `<object>`; the CSP has
  `object-src 'none'`. Some browsers do not draw a PDF in a sandboxed frame,
  so the viewer also links to it.
- Text and markdown previews `fetch` the signed URL with no credentials and
  no referrer. On a separate origin this needs CORS on the artifact routes.
- In mock mode, artifact URLs are `data:` URLs. A `data:` frame inherits the
  dashboard CSP, so scripts in the mock HTML artifact do not run.

### Content-Security-Policy

The daemon sends the policy as a `Content-Security-Policy` header on the
dashboard pages, with `frame-ancestors 'none'` and the artifact origin that it
knows at run time (docs/api.md §1.2, `dashboard_csp` in
`crates/terrariumd/src/api/mod.rs`).

`index.html` also has a `<meta http-equiv="Content-Security-Policy">`. The
`terrarium-csp` plugin in `vite.config.ts` fills it in on the dev servers
(mock and real), because Vite serves the page there and sends no header. A
production build drops the tag: next to the header it would also apply, and
without `VITE_ARTIFACT_ORIGIN` it would block the artifact frames that the
header allows. `VITE_META_CSP=1` keeps it in a build, for a static host that
sends no header. A `VITE_MOCK=1` build keeps it too. Without
`VITE_ARTIFACT_ORIGIN` the policy of the tag is:

```
default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline';
img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self';
worker-src 'none'; manifest-src 'self'; media-src 'none'; object-src 'none';
base-uri 'none'; form-action 'none'
```

With `VITE_ARTIFACT_ORIGIN`, the artifact origin goes into `img-src` and
`connect-src`, and it replaces `'self'` in `frame-src`. `VITE_MOCK=1` adds
`data:` to `frame-src`. `'self'` in `connect-src` also covers `ws:` and
`wss:` to the page's own host and port.

- No inline script and no `eval`.
- `style-src` keeps `'unsafe-inline'`: Solid templates carry static `style`
  attributes, and xterm.js adds a `<style>` element. The sanitizer drops all
  `style` attributes from untrusted HTML.
- A `<meta>` policy cannot set `frame-ancestors` or a report URL, and it
  does not cover the response that carries it. That is why the daemon's
  header is the policy in production.
- `<meta name="referrer" content="no-referrer">` keeps signed URLs out of
  `Referer` headers.

### Other notes

- The API-key form posts the key once (`POST .../api-key`) and clears the
  field. No endpoint returns a key. A login code goes to `.../input` and the
  daemon types it into the PTY; the page never shows it again.
- A host token shows once, in the add-host dialog only. Closing the dialog
  drops it.

### Daemon follow-ups

All of the earlier follow-ups are done in `terrariumd`:

1. **Cookie sessions**: `crates/terrariumd/src/api/session.rs` (see "Authentication").
2. **CSP as a header**: docs/api.md §1.2; the production build has no `<meta>` policy.
3. **Absolute signed URLs**: `POST /api/artifacts/{id}/url` answers a URL on the artifact origin (`TERRARIUM_ARTIFACT_PUBLIC_URL`); `/a/...` leaves the dashboard port when the artifact listener is set.
4. **CORS on the artifact routes**: the artifact origin answers the dashboard origin, never `*`.
5. **`?token=` refused by default** (`TERRARIUM_WS_QUERY_TOKEN=1` turns it on).
6. **`?from=system` and `?to=system` on `GET /api/mail`** work; the log still filters agent-to-agent, inbox, project, text, and time in the browser (docs/api.md §7).
7. **Budget alert links** go to `/p/<project>`; the old `/p/<project>/spend` route still redirects to Home for mail sent before that change.
8. **`held` and `held_reason`** are on the message row (migration 16); the log reads them and keeps the notice-based fallback for older rows.
9. **Names of removed agents** resolve through the tombstone table on mail as on cards.

Still open: `GET /api/mail?to=<ULID>` for a removed agent answers 404.

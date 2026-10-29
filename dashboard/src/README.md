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

The operator signs in with the operator password, or with the token
(`TERRARIUM_TOKEN` from `.env`) on **Use the token instead**. The dashboard
sends either one to the daemon once, at sign-in, and does not keep it
(docs/api.md §1.1, docs/security-review.md F3 and F15).

### Sign-in flow

1. On load, the app calls `GET /api/session` (mode `checking`). A `200`
   gives the CSRF value of the cookie, so no login screen shows. A `401`
   shows the login screen.
2. **Sign in** sends `POST /api/session` with `{"password": "..."}` and no
   `Authorization`. With the token, it sends `Authorization: Bearer <token>`
   and no body.
3. The daemon sets the `terrarium_session` cookie: a signed token (JWT),
   `HttpOnly`, `SameSite=Strict`, `Path=/`, `Max-Age` (30 days by default),
   and `Secure` on HTTPS. It answers `{"csrf", "exp", "expires_at"}`.
4. The app keeps the `csrf` value in memory only. It drops the password or
   the token.
5. Each request after that goes with `credentials: "same-origin"`, so the
   browser sends the cookie. Each `POST`, `PUT`, `PATCH`, and `DELETE` also
   sends `X-CSRF-Token: <csrf>` (`api/http.ts`). No request carries
   `Authorization`.
6. Past half its life, the daemon renews the cookie on a request. The new
   cookie keeps the same CSRF value, so the app changes nothing.
7. **Log out** (sidebar footer, or Settings, Account tab) sends
   `DELETE /api/session` with the CSRF header. **Log out everywhere**
   (Account tab) sends `DELETE /api/sessions`, which ends every browser and CLI
   session. Then the app forgets the CSRF value and shows the login screen.

The login screen shows the daemon's answers: the same text for every wrong
password, the `409` hint (no password set: run `terrariumd password set`, or
use the token), and the `429` wait (`details.retry_after`), with a
countdown. `loginError` in `stores/auth.ts` maps them.

The state machine is in `stores/auth.ts`. Its modes are `checking`, `none`,
`session`, and `bearer`:

- A 401 in `session` mode means that the cookie ended. The login screen then
  shows **over** the current page. The router stays mounted, so the route and
  the page state stay. After the sign-in, the event socket reconnects at
  once.
- The Account tab of Settings (`/settings/account`) shows the mode and the time the session
  ends. It also changes the operator password (`PUT /api/auth/password`);
  the change ends every session, so the page signs in again with the new
  password.

### What is stored where

| Mode | Memory | `sessionStorage` | `localStorage` |
|---|---|---|---|
| Session | CSRF value, `expires_at` | nothing | nothing |
| Bearer fallback | token | `terrarium.token` | nothing |

- The cookie is the cache. Script cannot read it. It stays through a browser
  restart until its end, and the daemon renews it while it is in use.
- The password and the session token never go into web storage. At load the
  app deletes the keys of older builds (`terrarium.session`,
  `terrarium.session.remembered`, `terrarium.token.remembered`).
- The CSRF value alone is not a credential. Without the cookie it does
  nothing. Script on the page can still read it and make calls while the page
  is open, but it cannot take the cookie away.
- A token that an older build left in storage starts the app in bearer mode.
  On load, `upgradeToSession` trades it for a session, and the token leaves
  the browser.

### Bearer fallback

A daemon without sessions answers `POST /api/session` with `404` (or
`405`). Then the app keeps the token in memory and in this tab's
`sessionStorage`. The sidebar shows a **bearer mode** chip, and the Account
tab of Settings says why. Each request sends `Authorization: Bearer`, and sockets offer
the `bearer.<token>` subprotocol. Each load tries the session again, so an
updated daemon moves the app to cookie mode without a new sign-in.

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

The sidebar (`app/Layout.tsx`, the item lists in `app/nav.ts`) has, from the
top: Home and Inbox; the project tree; the "Terrarium" section with the
global pages; and a Settings section at the bottom. The project tree
(`app/ProjectTree.tsx`) has a Projects root. A click on its name opens the
project cards (`/projects`) and opens the root; the chevron only opens or
closes it. Under the root, each project opens to show its pages. Settings
(`features/settings/SettingsPage.tsx`) is one page with a tab for each of
Account, Subscriptions, Harness config, and Hosts. Each tab is a route under
`/settings`.

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
  that return `Page<T>` (mail, project inbox, audit, events, sessions, card events,
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
- **Replies.** `mail.reply` and `projectInbox.reply` answer an array: one message
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
`board` events), the Inbox count in the sidebar and the Inbox items of the
Home queue (`stores/inbox.ts`, `inbox` and `question` events), the mail log (`features/mail/feed.ts`), the quota banner and the hold badges
(`stores/quota.ts`), the subscription list (`quota.*` events) and the detect
banner (`subscription.offer`), the home queue and usage strip, the Event
log, and the agent's Events tab. `lease` events keep the lease lists on the
Targets and agent pages live, and the "holds <target>" chips next to the
agent status badges (`stores/leases.ts`, `lib/leases.ts`).

`schedule` events keep the Schedules page live
(`features/schedules/model.ts`, `applyScheduleEvent`). The daemon sends IDs
only: `{change, schedule, name, project, run?, status?, trigger?}`.
`run_started` puts a spinner on the row. `run_finished` and the other
changes read the list again, and an open run history reads its newest page
again (`syncHead`). `POST .../run` answers with the run when it ends; that
answer also sets the last run. The Home queue shows each schedule whose
last run failed in the last 24 hours (`failedRecently`).

The dialog previews the next 3 runs of a cron expression with
`lib/cron.ts`, a small 5-field parser. The preview does not stop a send:
the daemon decides, and its `422` shows under the field that
`details.field` names. The words of a spec and an action
("every 15 min", "daily 09:00 UTC", "mail → lead: Standup") come from
`lib/schedule.ts`.

The page has two tabs, kept in the URL as `?tab=triggers`: Schedules
(`GET /api/schedules?kind=time`) and Triggers (`?kind=trigger`, docs/api.md
§19.4). A trigger uses the same dialog with the spec `on`: the kinds, the
match rules, the optional dedupe and escalation, and `max_per_hour`
(`features/schedules/trigger.ts`). A trigger offers only the actions `mail`,
`exec`, and `card`; a time schedule offers all six. The preset "CI failure
→ mail an agent" fills the example of docs/api.md §19.4. A trigger run shows
the event (`seq`, `kind`, `summary`) that started it.

The Triggers tab also lists the event sources (docs/api.md §20,
`features/schedules/EventSources.tsx`). A create and a rotate answer with
the secret. The secret shows one time in a panel, with the hook URL and the
setup steps. The list never keeps the secret (`withoutSecret`), and the
panel drops it when it closes. `event_source` events, and the events that a
source adds to the log, read the list again.

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

## Waits and watches

An agent is `waiting` when its turn ended and it has one or more open waits.
`idle` means that the agent has no open waits. `waiting` is a live status: the
agent takes turns and mail as an `idle` agent does. It is not a stalled status,
so the sidebar count of blocked agents does not include it.

- The agent JSON has `waits` (the number of open waits) and `waiting_on` (a
  short summary). The daemon leaves them out when there are no waits. A
  `status` event carries the same two fields. The status badge shows
  `waiting_on` in its title.
- The agent page has a **Waits** panel (`features/agents/Waits.tsx`). It lists
  the open waits with **Cancel**. A process wait also has **Log** and **Kill**
  (`DELETE /api/waits/{id}?kill=true`). The **closed** box adds the closed
  waits with their state and result. `wait` events read the list again.
- The log (`GET /api/waits/{id}/log`) is untrusted text. The page shows it as
  a text node in a `<pre>`, never as Markdown or HTML.
- The **Watches** panel lists the watches on the agent and by the agent, with
  **Remove**. Its form ("Notify me when <agent> is ...") sends
  `POST /api/agents/{id}/watches` for the operator. `watch` events read the
  list again.
- The pure helpers are in `features/agents/waits.ts`. The mock is in
  `api/mock/waits.ts`: the `planner` agent waits on `runner-dev`, a build, and
  a card. The mock closes an agent wait when the watched status matches, and
  it fires a watch (a `watch` event and a mail) on a status change. It does not
  run timers for time waits.

## The Inbox

Everything that needs the operator goes to the **Inbox** (`/inbox`,
`features/inbox/InboxPage.tsx`, `GET /api/inbox`). The old `/questions`
route redirects to it (`app/nav.ts` `MOVED_ROUTES`). An item has one of four
kinds:

- **question**: a leader agent asks for a decision (docs/api.md §6.8). The
  card has the choice buttons, a free-text answer, and **Retract**.
- **notice**: a leader agent tells the operator something, with no choices.
  The card has **Acknowledge**, an optional reply, and **Retract**. An
  acknowledged notice shows the reply.
- **mail**: a message to `operator`. The card shows the sender, the subject,
  and the body, and has **Acknowledge** with an optional reply on the thread
  (`POST /api/inbox/mail/{id}/ack`). A reply to a `system` mail with nobody
  else on the thread is a `422`.
- **system**: a thread that the loop guard paused. The card shows the
  subject, the participants, the agent run and the limit, and the held
  messages with **Release** each (`POST /api/mail/{id}/release`). **Resume**
  ends the pause and releases every held message, with an optional note to the
  participants (`POST /api/mail/threads/{id}/resume`). A system item shows only
  while it is open.

The page has kind tabs (All, Questions, Notices, Mail to you, System), Open
or Done, and a project filter, all in the URL (`?tab=&status=&project=`). The
daemon sorts: urgent first, then the newest.

- The sidebar badge on Inbox is the one count of the open items
  (`GET /api/inbox/counts`, `stores/inbox.ts`). It is red when one item is
  urgent, else it has the info style (`features/inbox/model.ts` `badge`).
- `inbox` events (`{item, kind, change, project?}`) and `question` events
  read the page, the badge, and the Home queue again.
- The question text, the options, the mail subject and body, and the retract
  reason are the sender's words: the page shows them as text nodes only. The
  context of a question or a notice is Markdown: it goes through
  `renderMarkdown` and the DOMPurify sink (`SafeHtml`) only.
- The agent page shows the questions and notices of a leader
  (`features/inbox/QuestionsPanel.tsx`), each with its kind.
- The pure helpers are in `features/inbox/model.ts` (tabs, order, counts,
  badge) and `features/inbox/questions.ts` (answer and acknowledge bodies).
  The mock is in `api/mock/questions.ts` and `api/mock/inbox.ts`: two open
  questions (one urgent, with a hostile context), one answered, one retracted;
  one open notice (with a hostile context) and one acknowledged notice; three
  open mails to the operator; and one paused thread with one held message.
  An answer sends one mail on the thread and resolves the `question` waits
  with no second mail, as the daemon does.

## The conversation view

The Conversation tab of the agent page shows the harness transcript as
blocks (docs/api.md §6, transcripts). The raw terminal stays on its own tab.
The code is in `features/agents/conversation/`. The logic is pure and in
`model.ts`.

- **Sessions.** The list on the left is `GET .../sessions`, newest first,
  with `title`, `blocks`, and `active`. The view opens the active session
  first. A new session or a start or stop reads the list again.
- **Blocks.** A user turn is a bubble on the right. A tagged turn (PLAN
  §8.5) shows one badge for each of its `parts`: `User`, `Mail from
  <sender> · <subject>`, or `System: <kind>`. The tag line does not show,
  and a quoted mail body shows as a quote, as plain text. An untagged turn
  has a chip for its origin: `you` (operator), `mail pointer`, `system`,
  or `terminal`.
  Assistant text is markdown. Thinking is collapsed; with no text it shows
  `redacted`. A `tool_call` is a card with the tool name and the summary,
  and the JSON input opens below it. Its `tool_result` goes under the card
  (`groupBlocks` pairs them by `tool.id`). A long output starts collapsed.
  An error has the danger colour. A truncated output has a note. Each turn
  ends with a quiet line: the model and the token usage. An equal usage on
  the next block counts once.
- **Paging.** The view loads the newest 500 blocks: `after = blocks - 500`,
  and it follows `next_after` until `complete`. **Load earlier** reads the
  500 before the window and keeps the scroll position. `mergeBlocks` keys
  every block by `seq`, so a page and a live block never show twice.
- **Live tail.** `GET .../transcript/stream?after=<newest seq>` is SSE.
  `EventSource` cannot send `Authorization`, so `Transport.stream` reads it
  with `fetch` and the normal credentials, and `api/sse.ts` parses it.
  `block` adds a block, `update` replaces the block with the same `seq`, and
  `status` with `ended` (or `inactive`, `stopped`, `closed`) ends the
  session. The stream reconnects with backoff and resumes after the highest
  `seq`. A `4xx` refusal (but `408` and `429`) does not retry.
- **Auto-scroll.** The view follows new blocks while it is at the end. When
  the operator scrolls up, it stops and counts the new blocks in a
  "new messages" pill. The pill scrolls back to the end.
- **Composer.** It is on only when the session is active and the agent is
  `idle`, `waiting`, `working`, or `blocked`. Enter sends (`POST .../send`) and
  Shift+Enter adds a line. When the agent is not idle or waiting, it says "queued —
  the agent is working": the daemon queues the turn. Otherwise it is off
  and gives the reason. The user block comes back through the stream when
  the harness writes it.
- **Safety.** Assistant text goes through `assistantHtml` and the
  `Markdown` component, so DOMPurify cleans it (`setSafeHtml`). User text,
  tool input, and tool output are text nodes only.

The mock has a transcript for each session (`api/mock/transcripts.ts`): the
four harnesses, a failed tool result, redacted thinking, a mail-pointer
turn, a long session for paging, and an old `lead` session with hostile
markdown. A send in the mock writes the user block, then thinking, a tool
call and its result, and a reply that grows with an `update` frame.
`api/mock/transcript-parity.test.ts` checks the shapes.

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

The mail log is a record, not a to-do list. It keeps no unread state for the
operator: no unread count, no mark read, no attention badge. Mail that needs
the operator is in the Inbox.

`/mail` is a log of every message (agent to agent, agent to operator,
operator to agent, `system`, project inboxes), newest first.
`/p/<project>/mail` is the same log for the messages that touch one project.
The old `/p/<project>/inbox` route redirects to it. A row of the table shows
the time, from, to, the subject with the first line and the state chips (the
recipient's state, read by, released, thread paused), and the thread. A click
opens the thread under the row, with reply and compose. On a project page, a
thread to the project inbox replies from `project:<name>`, and **Make card**
makes a card from its newest message.

The filters are from, to, project (`/mail` only), thread, time range (last
hour, day, week, all time, or from and to times), and free text. They are in
the URL (`?from=&to=&project=&thread=&range=&since=&until=&q=`). `open=<thread>`
opens a thread, so the Inbox links to `/mail?thread=<id>&open=<id>`.

**Live tail.** On (the default), a `mail` event pulls the new messages at
once. Off, the log stays still and shows **N new**; a click turns the tail on
and pulls the threads that the events touched (`features/mail/log.ts`
`tailEvent`, `setTail`).

**Server or browser.** `GET /api/mail` answers oldest first, so a newest-first
log reads the full list (`collectAll`, pages of 1000, at most 20 pages), and
the view shows 50 rows at a time (**Load more**). A `mail` event asks only for
the messages after the newest ID (`?cursor=<id>`, the list is `id > cursor`)
and reads the event's thread again (`?thread=`). A whole thread ID goes to the
server (`?thread=`). The other filters run in the browser: from and to match
part of a name there, and the project of a `system` notice comes from its card
prefix or budget scope. The daemon also has `project`, `since`, `until`, and
`q`; the mock answers them (`api/mock/index.ts`).

## Pages and the API

| Route | View | API calls |
|---|---|---|
| `/` | Home: usage strip, waiting-on-you queue; the tile detail has the usage split by project, agent, model, and card | `GET /api/subscriptions`, `GET /api/usage?bucket=hour&group=subscription`; tile detail: `GET /api/usage?subscription=&group=project\|agent\|model\|card`, `GET .../quota`; per project: `versions?status=in_review`, `wiki?status=ready_for_review`, `board`; Inbox items: `GET /api/inbox` (see "The Inbox") |
| `/projects` | Projects: list, create. The Projects root of the sidebar tree links it | `GET`, `POST /api/projects` |
| `/p/:project/settings` | Project edit and delete, Repos panel | `GET`, `PATCH`, `DELETE /api/projects/{name}?confirm=true`, `GET`, `POST .../repos`, `POST /api/repos/{id}/checkout` |
| `/agents` | Agents grid, new agent | `GET`, `POST /api/agents`, events socket |
| `/agents/:id` | Agent detail: edit, delete, roles, quota, leases, conversation (the default tab), terminal, events, sessions | `GET`, `PATCH`, `DELETE /api/agents/{id}`, `start/stop/restart/send`, `GET`, `PUT .../roles`, `GET /api/usage/me?agent=`, `GET .../leases?all=`, `WS .../pty`, `GET .../events?since=`, `GET .../sessions`, `GET .../sessions/{session}/transcript?after=&limit=`, `GET .../transcript/stream?after=` (SSE) |
| `/settings/subscriptions` | Settings, Subscriptions tab: subscriptions, detect banner, add wizard | `GET`, `POST`, `DELETE /api/subscriptions[/{name}]`, `GET .../detect`, `POST .../{name}/probe\|verify\|logout\|api-key`, `POST /api/subscriptions/login`, `GET`, `DELETE .../login/{id}`, `POST .../login/{id}/input`, `WS .../login/{id}/pty` |
| `/inbox` | Inbox: questions, notices, mail to the operator, paused threads; kind tabs, open or done, project filter | `GET /api/inbox?status=&kind=&project=`, `GET /api/inbox/counts`, `POST /api/questions/{id}/answer\|ack\|retract`, `POST /api/inbox/mail/{id}/ack`, `POST /api/mail/{id}/release`, `POST /api/mail/threads/{id}/resume` |
| `/questions` | Gone: redirects to `/inbox`. | none |
| `/mail` | Mail log: every message, newest first, filters, live tail, the thread inline, reply, compose | `GET /api/mail?thread=&cursor=`, `POST /api/mail`, `POST /api/mail/{id}/reply` |
| `/p/:project/mail` | The mail log of one project; reply from the project inbox, make card | `GET /api/mail`, `POST .../inbox/{id}/reply`, `POST .../cards` (`from_mail`) |
| `/p/:project/inbox` | Gone: redirects to `/p/:project/mail`. | none |
| `/p/:project/board` | Board, drag and drop, card drawer | `GET .../board`, `PATCH /api/cards/{id}`, `POST .../comments`, `GET .../events`, `DELETE /api/cards/{id}` |
| `/p/:project/memory` | Memory list, detail, graph | `GET .../memories`, `GET /api/memories/{project/MEM-n}/graph` |
| `/p/:project/wiki/*path` | Page tree, render, edit, history, create | `GET .../wiki`, `GET`, `PUT /api/wiki/{project/path}`, `revisions`, `diff`, `restore`, `backlinks`, `review` |
| `/p/:project/artifacts/:id?` | Artifact viewer, upload, delete | `GET .../artifacts`, `.../artifacts/usage`, `POST .../artifacts` (multipart), `POST /api/artifacts/{id}/url` |
| `/p/:project/versions/:id?` | Version timeline, review, new version | `GET`, `POST .../versions`, `POST /api/versions/{id}/submit|approve|reject|release`, `compare` |
| `/p/:project/spend` | Gone: redirects to `/` (`app/nav.ts` `MOVED_ROUTES`). The daemon's budget alert mail still links it. | none |
| `/p/:project/schedules` | Schedules of the project and the global ones, scope filter, run history, run now, enable, create, edit, delete | `GET /api/schedules?project=`, `POST /api/schedules`, `PATCH`, `DELETE /api/schedules/{id}`, `POST .../run\|enable\|disable`, `GET .../runs?limit=&cursor=` (docs/api.md §19) |
| `/schedules` | All schedules, grouped by scope | `GET /api/schedules` and the calls above |
| `/p/:project/schedules?tab=triggers`, `/schedules?tab=triggers` | Triggers (spec `on`) and their runs with the event; event sources: list, create, enable, rotate, delete, the one-time secret | `GET /api/schedules?kind=trigger`, `GET`, `POST /api/event-sources`, `PATCH`, `DELETE /api/event-sources/{id}`, `POST .../rotate` (docs/api.md §19.4, §20) |
| `/targets` | Targets, lifecycle, snapshots (list, take, restore, delete), exec console, leases (grant, extend, revoke, history) | `GET /api/targets`, `POST /api/targets/{name}/up\|down\|snapshot\|restore\|status`, `GET .../snapshots`, `DELETE .../snapshots/{snap}`, `POST .../exec`, `WS .../sessions/{id}/ws`, `DELETE .../sessions/{id}`, `GET`, `POST .../leases`, `PATCH`, `DELETE .../leases/{agent}` (docs/api.md §17.7) |
| `/settings/hosts` | Settings, Hosts tab: host agents: status, capabilities, add with a one-time token, revoke | `GET`, `POST /api/hosts`, `DELETE /api/hosts/{name}` (new API, not in docs/api.md yet) |
| `/events` | Event log with history | `GET /api/events?since=&kind=&project=&agent=`, events socket |
| `/audit` | Audit log | `GET /api/audit?actor=&action=&project=` |
| `/settings/account` | Settings, Account tab: session mode, end time, what the browser keeps; password; sign out, sign out everywhere | `GET`, `DELETE /api/session`, `DELETE /api/sessions`, `PUT /api/auth/password` |
| `/settings/harness` | Settings, Harness config tab: the config repo, its changes and diffs, commit, sync, pull, push, secret grants to approve | `GET /api/config/...` (docs/api.md §25) |
| `/settings`, `/subscriptions`, `/hosts`, `/config` | Gone: redirect to `/settings/account`, `/settings/subscriptions`, `/settings/hosts`, and `/settings/harness` (`app/nav.ts` `MOVED_ROUTES`). The redirect keeps the query. A quota mail of the daemon still links `/subscriptions`. | none |
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
6. **`?from=system` and `?to=system` on `GET /api/mail`** work; the log still filters from, to, project, text, and time in the browser (docs/api.md §7).
7. **Budget alert links** go to `/p/<project>`; the old `/p/<project>/spend` route still redirects to Home for mail sent before that change.
8. **`held` and `held_reason`** are on the message row (migration 16). The Inbox takes the held messages of a paused thread from `GET /api/inbox` (system items); the log does not compute holds.
9. **Names of removed agents** resolve through the tombstone table on mail as on cards.

Still open: `GET /api/mail?to=<ULID>` for a removed agent answers 404.

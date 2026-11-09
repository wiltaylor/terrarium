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
top: Home, Inbox, Logs, and Schedules; the project tree; and a Settings
section at the bottom. The project tree (`app/ProjectTree.tsx`) has a
Projects root. A click on its name opens the project cards (`/projects`) and
opens the root; the chevron only opens or closes it. Under the root there is
one row per project, with no page, agent, or repo sub rows. The project row
links `/p/:project`, whose tabs reach those views. It is marked on every
`/p/:project/...` path and on the page of an agent in that project.

Each project row has a health dot before the name (`lib/projectHealth.ts`,
`components/ProjectDot.tsx`). The dot is red when an agent is in error or
blocked. It is yellow when an agent works, starts, restarts, or compacts. It
is green when all agents rest: idle, waiting, stopped, or paused. A project
with no agents has a hollow grey ring. Red has the highest priority. The row
label and title give the counts, for example `terrarium: 1 blocked, 2
working, 3 idle`. The dot changes live with the agent events. The project
cards (`/projects`) show the same dot. The project row shows the number of
agents that are blocked or in error.

A cog at the right of each project row opens the project settings screen
(`/p/:project/settings`). The cog is muted until the row is under the pointer
or has the focus, and it is in the accent colour while the screen is open.
The cog is not a tree row, so it is not in the roving tabindex. The keyboard
gets to the screen through the Settings tab of the project page. An agent
page (`/agents/:id`) opens the Projects root and marks the agent's project.
The dashboard has no list of all agents: `/agents` redirects to `/projects`.
Above eight projects, the tree shows a filter. The root and Bookmarks node
use `app/TreeRoot.tsx`, one open set in `localStorage`, and the keyboard map
in `app/treeNav.ts`. The open set keeps folder and root state; its old
project, project Agents, and project Repos names are pruned. Settings
(`features/settings/SettingsPage.tsx`) is one page with a tab for each of
Account, Subscriptions, Secrets, Templates, System prompts, Skills, Targets,
Harness config, Hosts, and Software. Each tab is a route under `/settings`. The Skills tab also has the
editor of one skill at `/settings/skills/<name>`. A skill whose `SKILL.md`
has `metadata` `terrarium-default: "true"` shows a read-only "default" badge
there. The skills picker of the agent and template forms
(`features/skills/SkillsPicker.tsx`, model in `features/skills/model.ts`)
has a "Use default skills" box (`default_skills`), and shows each default-on
skill checked with a "default" badge; a clear of its box puts it in
`skills_off`. The picker shows the effective skills ("The agent gets"), and
the agent page lists `effective_skills` with the source of each. Logs
(`features/logs/LogsPage.tsx`) is the same: the top Logs item has a tab for
each of Mail, Events, and Audit, under `/logs`. The **Logs** item of a project has the Mail and Events tabs of that
project, under `/p/<project>/logs`. The project settings screen is the
same: it has a tab for each of General, Secrets, and Templates of
the project, under `/p/<project>/settings`. The screens use
`components/TabScreen.tsx`. The Logs item is active on each of its tabs.
The project items are Board, Repos, Logs, Knowledge, and Agents.
The **Board** item (`features/board/BoardScreen.tsx`) has a tab for each of
Cards, Artifacts, Tests, and Schedules (`BOARD_TABS`). The Cards tab is the board
at `/p/<project>/board`, so a `?card=` link keeps its path. The Cards tab
filters and groups the cards by their version label; there is no
Versions tab. The other tabs are under `/p/<project>/board/artifacts`,
`/p/<project>/board/tests`, and `/p/<project>/board/schedules`. The
Schedules tab keeps its Schedules and Triggers sub-tabs (`?tab=triggers`).
The **Knowledge** item (`features/knowledge/KnowledgePage.tsx`) has a tab
for each of Wiki, Memory, and DB (`KNOWLEDGE_TABS`), under
`/p/<project>/knowledge`. The Board and Knowledge items are active on each
of their tabs.
The **DB** tab (`features/db`, docs/api.md §27) shows the record types of
the project (`DbTypesPage`). A type opens its records
(`/p/<project>/knowledge/db/<type>`, `DbRecordsPage`): a query box that
starts with `type:<name>` (Enter runs it; an error marks its position), a
table with the first six fields and a column chooser, headers that write
`sort:` terms, and **Load more**. A row, or `?id=<record>`, opens the record
dialog (`RecordModal`): one input for each field kind, the errors of the
daemon under each input, and an offer to load the current record after a
`409`. **Schema** opens the schema editor (`SchemaEditor`). `?q=` fills the
query box. `db.type` and `db.record` events reload the page. The pure logic
is in `features/db/model.ts`. On a phone, the dialogs fill the screen and
the table scrolls in its box. The styles are in `styles/db.css`.
The **Repos** item of a project (after Board) opens `/p/<project>/repos`
(`features/repos/ReposPage.tsx`): the repos, and **Add repo**. A repo opens
its browser (`features/repos/RepoPage.tsx`, see "The repo browser").

**New agent.** The **New agent** button of the Agents page opens
`features/agents/NewAgentDialog.tsx`. The dialog starts on a chooser: a card
for each template of the project, then the global ones (with a "global"
chip), and a dashed **Custom** card. Above eight templates, a search box
shows. The arrow keys, Home, and End move the focus between the cards, and
Enter picks one. A template card opens a short form: the name and a summary
of the template. **Change settings…** shows the agent fields, filled from
the template. The create sends `template` and only the fields that differ
from the template (`formToCreate` in `features/templates/model.ts`,
docs/api.md §21.3). **Custom** opens the full form, with no template.
**Back** goes to the chooser and keeps the name. With no template, the
dialog opens on the full form, with a link to Settings → Templates. The
chooser model is the pure `features/agents/chooser.ts`.

**Templates.** The Templates tab (`features/templates/TemplatesPage.tsx`)
lists the templates, with Edit (`TemplateDialog.tsx`, the form) and Delete.
Each template is a WCL file in the config repo (docs/api.md §21.5): a save
writes the file. **View WCL** opens a read-only panel under the table with
the path and the text of the file (`GET /api/templates/{key}/wcl`), in a
`<pre>` text node, monospace. There is no WCL editor: edit the file by
hand, or use the form. The mock writes the same text (`renderWcl` in
`api/mock/templates.ts`).
A template whose subscription does not exist (`subscription_missing`,
docs/api.md §21.1) has a `<name> missing` warn chip in the Harness cell,
with the reason as its title. An agent from it does not start, so the
daemon refuses the create. The mock flags it too (`flagged` in
`api/mock/templates.ts`), but its create does not refuse it.

**System prompts.** An agent and a template have one **System prompt**
selector (`features/agents/SystemPromptSelect.tsx`) over the two fields
`persona` and `system_prompt` (docs/api.md §6.1, §28): **By role** (no
persona), **Harness default** (`none`), each entry of the system prompt
library (`persona` = the name), and **Custom…** (the `system_prompt` text
in a textarea; it wins over the persona). A library entry clears the custom
text. Custom starts from the text of the entry that was selected.
**Preview** shows the text of the selected entry (or of the entry by role)
in a `<pre>` text node, monospace. **Save as new system prompt** under the
custom text asks for a name and an optional description, sends
`POST /api/system-prompts`, then selects the new entry and clears the
text. A persona that the library does not list shows a warning, in the
form and on the agent page. The model is the pure
`features/agents/systemPrompt.ts`. The agent page shows one **System
prompt** row with an **Edit** button that opens the same selector. The
System prompts tab (`features/systemprompts/SystemPromptsPage.tsx`) lists
the library, with view (the text and the users), edit, delete, and **New
system prompt**. A delete of an entry in use gets `409` with
`details.users`: the tab lists the users and offers **Delete anyway**
(`?force=true`). `leader` and `worker` have no Delete.

**Live mode of the log tabs.** Mail, Events, and Audit have the same
**Live** control first in their toolbar (`components/Live.tsx`
`LiveControl`), on at the start, with a pulsing dot while on. Each tab is
newest first. Live, a new item that matches the filters comes in on top. Off,
the tab puts the new items in its list but holds them: the list stays still,
and **Show N new** counts the held items that match the filters and shows
them. Live on again, **Refresh**, or a new filter shows the held items too.
When the top of the list is out of view, `createScrollKeeper` moves the page
by the height of the new rows, so the viewport stays still, and a sticky
**N new** pill goes back up to them. The state is the pure
`components/live.ts`. The Audit tab gets its rows from the `audit` frames of
the event stream (below).

## Conventions

- Put each view in its own folder under `src/features/<feature>/`.
- Keep global state in `src/stores` only. `src/stores/index.ts` creates the
  app instances. Do not keep mutable state at module level anywhere else.
- Write each store as a factory (`createXStore(api, ...)`). Tests build their
  own instance with the mock API.
- Put logic that has no UI in pure functions (`moveCard`, `applyAgentEvent`,
  `renderWiki`). Test those functions directly.
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
- **202 answers.** `start`, `stop`, `restart`, `pause`, and `unpause` answer
  with the agent as the daemon accepted the call: the old status. Do not put
  that body into the store. The status change comes from the event stream.
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
dropped. The socket asks for `audit=1`: the daemon also sends an `audit`
frame (no `seq`) for each new audit row that the caller may read, and
`events.onAudit(...)` gives them to the Audit tab (docs/api.md §9.1). Before
the first connect, the store asks `GET /api/events/latest`.
When the daemon has it, the first connect uses `since = latest - 300`: the
backlog is the last 300 events only. When the daemon does not have it, the
first connect has no `since`, and the daemon sends the whole log. The agents
store uses the backlog for the last event of each agent.
While the socket is down, the layout shows a **Disconnected** strip with a
**Reconnect now** button.

Every view listens through `events.on(...)`: agent status badges, the
current card, and the self-report (`applyAgentEvent`), the board (`card` and
`board` events), the Inbox count in the sidebar (`stores/inbox.ts`, `inbox`
and `question` events), the mail log (`features/mail/feed.ts`), the quota banner and the hold badges
(`stores/quota.ts`), the subscription list (`quota.*` events) and the detect
banner (`subscription.offer`), the home usage strip, the Event
log, and the agent's Events tab. `lease` events keep the lease lists on the
Targets and agent pages live, and the "holds <target>" chips next to the
agent status badges (`stores/leases.ts`, `lib/leases.ts`).

`schedule` events keep the Schedules page live
(`features/schedules/model.ts`, `applyScheduleEvent`). The daemon sends IDs
only: `{change, schedule, name, project, run?, status?, trigger?}`.
`run_started` puts a spinner on the row. `run_finished` and the other
changes read the list again, and an open run history reads its newest page
again (`syncHead`). `POST .../run` answers with the run when it ends; that
answer also sets the last run.

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
`exec`, `card`, and `webhook`; a time schedule offers all seven. The preset
"CI failure → mail an agent" fills the example of docs/api.md §19.4. A
trigger run shows the event (`seq`, `kind`, `summary`) that started it.

The action "Send a webhook" (docs/api.md §19.5,
`features/schedules/WebhookFields.tsx`, `webhook.ts`) has the URL (or a
secret that holds it), the format (JSON, Slack, Discord, ntfy, or a
template), the method, the time limit, the content type, the headers (a
value, or a secret from the `CredentialSelect` with `kind="webhook"`: a
secret with one main value), and an optional signing secret. A live
preview shows the request for a sample event of the first kind
(`previewRequest`): the headers with each secret as `[secret:<name>]`, and
the body as the daemon renders it, with a warning when a JSON content type
gets a body that is not JSON. The daemon has the last word: its `422` shows
under the field that `details.field` names.

The row of a webhook trigger opens to its Deliveries panel
(`Deliveries.tsx`, `deliveries.ts`) above the runs: `GET
.../deliveries`, newest first, with a status chip (`queued`, `retry N at
<time>`, `ok`, `failed`, `giving up`, `dropped`), the HTTP status, the time
taken, the attempt, the answer, and the request body. **Redeliver** queues
a copy; **Send test** sends a sample event at once and shows the request
and the answer. A `webhook.delivery` event of the trigger reads the newest
page again. The mock answers from the URL: `fail` in it is a 503 (a
retry), `404` a 404, `redirect` a 302; the seed has the trigger
`page-ntfy` of terrarium with an ok, a retry, a 404, and a dropped row.

The Triggers tab also lists the event sources (docs/api.md §20,
`features/schedules/EventSources.tsx`). A create and a rotate answer with
the secret. The secret shows one time in a panel, with the hook URL and the
setup steps. The list never keeps the secret (`withoutSecret`), and the
panel drops it when it closes. `event_source` events, and the events that a
source adds to the log, read the list again.

## The agent lifecycle

The operator does not start or stop an agent. Work wakes a stopped agent:
a turn (`POST .../send`), mail, a card, a nudge, or an answer. The daemon
starts it with a resume and then delivers the work. An agent that stays
`idle` or `waiting` for its idle timeout stops (`status_reason: "idle"`).
The model is pure and in `lib/lifecycle.ts`.

- **Badge.** `StatusBadge` shows the status, `paused` (grey, with a pause
  glyph), or `stopped (idle)`. A `N queued` chip follows it when work waits
  (`queued`). The sidebar row gives a paused agent the same grey pause
  glyph in place of the status dot.
- **Buttons.** The agent page has Pause, or Resume for a paused agent
  (`POST .../pause`, `POST .../unpause`), and Restart. Restart is off while
  the agent is paused. Pause stops a live agent and blocks every wake;
  turns and mail stay queued. Resume wakes the agent at once only when
  work waits.
- **Send.** A send to an agent that does not run answers
  `{"accepted": true, "queued": true}`. The page says that the agent wakes,
  that it waits for a resume (paused), or that it waits for a restart (an
  agent in `error` does not wake).
- **Idle stop.** The agent form and the template form have an `idle_stop`
  field. Empty is the global default (`GET /api/health` `idle_stop`, shown
  as the placeholder). `off` or `0` never stops the agent. Else a duration
  of `d`, `h`, `m`, and `s` parts, such as `1h30m`. The form checks the
  format (`idleStopProblem`). A patch sends it only when it changed; an
  emptied field goes as `null`, back to the default.
- **Store.** A `status` event sets `status_reason` from `reason`.
  `agent.paused` and `agent.unpaused` set `paused`. No event carries
  `queued`, so a lifecycle event or a stop reads the list again.
- **Auto compact** (PLAN §18). The agent form and the template form have
  an "Auto compact" group: "Compact on idle" (default, on, or off; the
  default names the global `compact_on_idle` of `GET /api/health`) and
  "Compact max age" (the form of the idle stop; `compactMaxAgeProblem`).
  Empty goes back to the default (`null` in a patch). For a harness that
  is not in `compact_harnesses`, the group says "auto compact not
  supported for <harness>". The agent page shows the settings
  (`autoCompactText`) and "last compacted ..." (`lastCompactedText`), with
  "(failed)" for a failed one.
- **Compact and New session.** The agent page has two buttons.
  **Compact** (`POST .../compact`) compacts the session. **New session**
  (`POST .../new-session`) ends the session and starts a new, clean
  harness session. Each button opens a small dialog (`ResetDialog.tsx`)
  with an optional "Then send…" text area: the daemon sends that prompt
  as the next turn after the reset (`[System:reset requested by <who>]`).
  A new session forgets the conversation, so the prompt must restate the
  goal. The New session dialog also has "Include recap" (default on): a
  recap turn goes first. The dialog does not send a blank prompt, and it
  refuses a prompt of more than 32 KiB (`resetThenProblem`, `resetBody`).
  Compact is off, with the reason in its title, for an agent that is
  stopped or in `error`, is paused or held, or cannot compact
  (`compactButton` in `lib/lifecycle.ts`). It is on for a busy agent: the
  compaction waits until the current turn ends. New session is always on
  (`newSessionButton`); its title says when the new session starts. A
  stopped agent with a "then" prompt starts at once in a new session.
  Without one, its next wake starts a new session. A paused agent keeps the
  request until the resume. After the request, the notice says `started`,
  `queued`, or "at the next wake", and "It replaced the queued request."
  when the request replaced one (`resetNotice`). While a reset waits
  (`reset_pending`), the page header shows a "compaction queued", "new
  session queued", or "new session at next wake" chip. Its title names who
  asked and whether a "then" prompt follows (`resetChip`). One reset at
  most waits for an agent: a new request replaces it. While a compaction
  runs, the agent is `working` with `status_reason` `compacting`. A new
  session starts with `status_reason` `new session: requested by <who>`,
  and the Sessions tab shows it as the current session. The old session
  ends with the outcome `new_session`.
- **Store (resets).** `agent.reset` (`{kind, by, then_set, deferred,
  replaced, state}`) sets `reset_pending` for a `queued` or `next_wake`
  request, and clears it for a `started` one. A `status` event with the
  reason `compacting` or `new session: ...` clears it. The event also reads
  the list again, for the exact fields.
- **Mock.** `docs-writer` is stopped by the idle timeout with 2 queued, and
  `seo-worker` is paused with 1 queued. A send to a stopped agent wakes it
  (`agent.woken`), and the mock runs the turn. The mock does not stop idle
  agents by time, and does not compact by itself. `lead` has
  `compact_max_age` `24h` and a compaction 5 hours ago. Compact makes an
  idle agent `working` (`compacting`) for 1.5 s, then writes
  `agent.compacted`. Every harness can compact, as in the daemon. The mock
  caller is the operator. A reset of a busy agent waits in `reset_pending`
  and runs when the agent comes to rest. A new session ends the session
  (`new_session`), opens a new one, and types the recap and the "then"
  turns. Each request writes `agent.reset` and the audit row
  `agent.compact` or `agent.new_session`. `runner-dev` works all the time,
  so a reset on it stays queued.

### Ephemeral agents

An agent with spawn rights spawns ephemeral agents: one task each, then
`terrarium agent done`, and the daemon closes them (docs/api.md §6.10,
PLAN §18). The pure model is `features/agents/spawn.ts`; the views are in
`features/agents/Spawned.tsx`.

- **Hidden from the lists.** `api.agents.list()` asks without
  `?include=ephemeral`, so the agents store, the sidebar tree, the project
  health dots, the counts of the project cards, and the mail address book
  have no ephemeral agents. The store does not upsert one, and the events
  of an agent that a refresh did not bring ask for no more refreshes.
- **Card agents** (TER-75). The project agents page lists the card agents
  below the agents (`api.agents.list(project, { card: true })`, the
  ephemeral ones of `?include=card`), each with its card. A card tile
  names its bound agent (`agent_name`) when it is not the assignee
  (`features/board/cardAgent.ts`). In the mock, `lead-worker-3` works
  `TER-13`.
- **Spawned tab.** The agent page has a **Spawned** tab, with the count
  of running ones in its label (`spawned_running`). The table
  (`GET /api/agents/{id}/spawned`) shows the running rows first: a status
  chip (the live status, or the outcome), the name (a link while it
  runs), the template, the first line of the task, the first lines of
  the result, the duration, a **session** link (`/sessions/<id>`, also
  after the finish while the archive keeps the file), and **Cancel** for
  a running one. A row opens a drawer with the whole task and result as
  plain text. `agent.spawned`, `agent.finished`, and the status events of
  the children read the list again.
- **Timeline.** The Conversation tab shows only the transcript, so the
  lines `spawned <name> (<template>)` and `<name> finished: <outcome>`
  are the `agent.spawned` and `agent.finished` events in the **Events**
  tab of the owner, each with the session link of the ephemeral agent.
- **Ephemeral page.** The page of an ephemeral agent says "Ephemeral agent
  of <owner> · template · task · lifetime left", with a link to the
  owner's Spawned tab. It has **Cancel** (`POST .../cancel`) in place of
  Pause and Delete.
- **Spawn rights.** The **Edit** form has a **Spawn rights** group
  (`SpawnRightsFields`): on or off, the templates (`project:*`,
  `global:*`, and each template), max concurrent, max per day, max
  lifetime, and "its ephemeral agents may spawn too". The form checks the
  block as the daemon does (`spawnProblem`). A policy patch always carries
  the spawn block, so a change of the targets does not drop it. The agent
  page shows the rights (`spawnRightsText`).
- **Mock.** `terrarium/lead` may spawn `global:worker` and `project:*`.
  Two of its ephemeral agents run (`lead-worker-3` working,
  `lead-reviewer-2` idle), and three finished (`done` with a result,
  `timeout`, `cancelled`), with tombstones and sessions that open. A spawn
  types the task as the first turn; a cancel mails the owner; the removal
  of an owner removes its ephemeral agents and their rows.

## Self-hosted models, enable and disable, stop all

Settings, Subscriptions (`features/subscriptions/`). The pure logic is in
`wizard.ts` (with `selfhosted.test.ts`); the mock is in
`api/mock/selfhosted.ts`.

- **Self-hosted model** (a path of the Add subscription wizard): pick the
  API style (OpenAI or Anthropic), then the base URL (the OpenAI root
  with `/v1`; the Anthropic root without it), an optional key, the models,
  the default model, the context window, the max output tokens, the
  concurrent streams, and an optional status page. **Fetch models** asks
  the daemon to read the model list of the server once
  (`POST /api/subscriptions/fetch-models`); when it fails, the page shows
  the error and you type the model ids. The harness boxes show only the
  harnesses of the style (OpenAI: pi and Codex; Anthropic: Claude Code and
  pi; never Copilot). The wizard makes the subscription
  (`POST /api/subscriptions` with `selfhosted`), then stores the key only
  when you typed one.
- **The row.** A self-hosted subscription shows
  `self-hosted · <style> · N streams · 128k ctx`, its base URL and models,
  and `1/2 streams in use` (with `N waiting` when work waits for a stream).
  A disabled subscription shows a `disabled` chip. **Status page** opens
  the provider's public status page (or the `status_url` of a self-hosted
  server) in a new tab; the dashboard only links to it.
- **Disable / Enable.** A disabled subscription starts and wakes no agent,
  and the agent form does not offer it. Agents that run go on until they
  stop. **Stop all now** and **Disable and stop all** ask in the page
  before they act (`POST .../stop-all`).
- **Agents.** The agent page shows a `disabled` chip next to a disabled
  subscription. The badge says `stopped (subscription disabled)`, or
  `waiting for a free stream` when the work of the agent waits for a
  stream of its self-hosted subscription.
- **Usage.** A self-hosted subscription has no quota (`no_quota`): its
  tile shows the usage counts only.

## Harness versions and the auto-update

The "Harness versions" table of Settings, Subscriptions
(`features/subscriptions/HarnessVersions.tsx`) reads
`GET /api/harnesses?latest=true` (docs/api.md §4). The pure logic is in
`features/subscriptions/harnesses.ts`, with tests.

- **Installed column.** The installed version. Under it, `base <version>`
  when the installed version is not the base version of `harnesses.toml`
  (`offBase`, the `base` field). The base version is not a pin: the
  runtime pin is in the Updates column.
- **Updates column.** The policy chip (`policyChip`): `auto`, or
  `pinned <version>` (`auto_update.pinned_version`). Under it, the last check (`lastCheckLine`) and the
  changelog link.
- **Status column.** The chip comes from npm (`harnessStatus`): `latest`,
  `update available`, or `update available (major)`. When npm does not
  answer, the chip is `installed`. There is no tested range and no range
  warning.
- **Buttons.** "Roll back and pin" (with an inline confirm) when the
  previous version is older than the active one (`rollbackTarget`; `POST
  .../pin` with no version). "Pin current" for an `auto` row (`POST
  .../pin` with the active version). "Unpin" for a pinned row. "Check now"
  in the header (`POST /api/harnesses/check`). The box under the table
  shows one line for each result (`pinLine`, `checkLine`).
- **Under the table.** The schedule and the next check (`scheduleNote`).
- **Mock.** claude is pinned to 2.1.310. The auto-update moved pi to 1.5.1,
  so pi has "Roll back and pin" to 1.4.0. Check now installs codex 0.165.0
  and copilot 1.0.110. It sends no Inbox mail, as the daemon does with
  `TERRARIUM_HARNESS_UPDATE_MAIL=off` (the default). The mock GC keeps the
  previous and the pinned version and the newest two under the active one.

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
  card has the choice buttons, a free-text answer, and **Retract**. A
  bundle of related questions (`question.kind: "bundle"`, item ID
  `bundle:<id>`) is a question item too, and counts once. Its card
  (`features/inbox/BundleCard.tsx`) shows the summary, the shared context,
  and each item with its number, text, context, choice buttons, and a
  free-text answer. A counter says "N of M answered", and **Submit all
  answers** is on only when each item has an option or a text. The draft is
  in the card and in sessionStorage for each bundle and revision
  (`features/inbox/bundle.ts`), so a reload keeps it. The answer sends the
  revision: when the agent changed the bundle, the daemon answers `409`, the
  card reads the bundle again, keeps the answers of the items that did not
  change, and names the items that did. After the answer, the card shows
  each answer.
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
  read the page and the badge again.
- The question text, the options, the bundle items, the mail subject and
  body, and the retract reason are the sender's words: the page shows them as
  text nodes only. The context of a question, a bundle item, or a notice is Markdown: it goes through
  `renderMarkdown` and the DOMPurify sink (`SafeHtml`) only.
- The agent page shows the questions and notices of a leader
  (`features/inbox/QuestionsPanel.tsx`), each with its kind.
- A question, a bundle, or a notice with a `card` has a chip "card <id>".
  It opens the board with the drawer of that card
  (`/p/<project>/board?card=<id>`, `cardHref`). The board shows the same
  question on the card (see "The board").
- The pure helpers are in `features/inbox/model.ts` (tabs, order, counts,
  badge) and `features/inbox/questions.ts` (answer and acknowledge bodies).
  The mock is in `api/mock/questions.ts` and `api/mock/inbox.ts`: two open
  questions (one urgent, with a hostile context), one answered, one retracted;
  one open notice (with a hostile context) and one acknowledged notice; one
  open bundle of three questions (one item with a hostile context); three
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
- **Composer.** It is on for every agent, also a stopped or paused one.
  Enter sends (`POST .../send`) and Shift+Enter adds a line. When the agent
  is not idle or waiting, it says "queued — …": the daemon queues the turn.
  A stopped agent wakes to run it; a paused agent keeps it until the
  operator resumes it. The composer is off only for a live agent when the
  session is not active, and it gives the reason. The user block comes
  back through the stream when the harness writes it.
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

## Session links in the logs

A row of each Logs tab can link to the agent session that was active when
the row was written (docs/api.md §9, "Session links"). The daemon sends
`session_id` and `session_label` only when the reader may read the session.
`components/SessionLink.tsx` shows a small "session" link with a terminal
icon to `/sessions/<id>`. The tooltip is the full label, for example
`dev · 2026-10-26 09:12 UTC`. The link is at these places:

- Events tab: at the end of the event row (`components/EventList.tsx`, so
  the agent Events tab has it too).
- Mail tab: in the From cell of the row, and in the head of each message of
  the open thread (`features/mail/parts.tsx` `MessageView`). It is the
  sender's session.
- Audit tab: in the Actor cell, for an agent actor.

Live rows have the link too: the daemon puts the fields on the stream
frames. A click on the link opens the session and does not open or close
the row. `components/sessionLink.ts` has the model. The mock gives the
fields as the daemon does (`api/mock/index.ts` `withSession`).

## The mail log

The mail log is a record, not a to-do list. It keeps no unread state for the
operator: no unread count, no mark read, no attention badge. Mail that needs
the operator is in the Inbox.

The mail log is the Mail tab of the Logs screen. `/logs/mail` is a log of every message (agent to agent, agent to operator,
operator to agent, `system`, project inboxes), newest first.
`/p/<project>/logs/mail` is the same log for the messages that touch one
project. The old `/mail`, `/p/<project>/mail`, and `/p/<project>/inbox`
routes redirect to them. A row of the table shows
the time, from, to, the subject with the first line and the state chips (the
recipient's state, read by, released, thread paused), and the thread. A click
opens the thread under the row, with reply and compose. On a project page, a
thread to the project inbox replies from `project:<name>`, and **Make card**
makes a card from its newest message.

The filters are from, to, project (`/logs/mail` only), thread, time range (last
hour, day, week, all time, or from and to times), and free text. They are in
the URL (`?from=&to=&project=&thread=&range=&since=&until=&q=`). A click on
a row sets `open=<message>`: the thread opens under that row, also when the
thread has newer rows (mail to the operator usually has a later reply from the
operator). `open=<thread>` opens a thread under its newest shown row, so the
Inbox links to `/logs/mail?thread=<id>&open=<id>` (`openRowOf`).

**Live mode.** A `mail` event pulls the new messages and the event's thread,
live or not. The feed gives the IDs of the new messages to the live mode of
the log tabs (above): live, they show at once; off, they wait for **Show N
new**.

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

Every `/p/:project/...` page sits in the project shell
(`features/projects/ProjectScreen.tsx`). Its route tabs are Board, Dashboard,
Agents, Repos, Knowledge, Pipelines, Calendar, Logs, and Settings, in that
order (`app/nav.ts` `PROJECT_TABS`). `/p/:project` opens Board. The first path
segment selects the tab, including on deep links. Board, Knowledge, Logs, and
Settings keep their own tab row below the project tabs.

| Route | View | API calls |
|---|---|---|
| `/` | Home: usage strip (each tile has the `display_name` of the subscription, its name in small text, and the windows of the last good probe, shortest first, with clear labels: "5-hour", "Weekly", "Weekly, Fable"; an Anthropic Ultra plan has no 5-hour window, so its weekly window leads; `lib/format.ts` `orderWindows`, `windowLabel`), host cards (see "Home: hosts"); the tile detail has the usage split by project, agent, model, and card | `GET /api/subscriptions`, `GET /api/usage?bucket=hour&group=subscription`; tile detail: `GET /api/usage?subscription=&group=project\|agent\|model\|card`, `GET .../quota`; hosts: `GET /api/hosts`, `GET /api/hosts/{name}/metrics?minutes=15` |
| `/projects` | Projects: list, create. The Projects root of the sidebar tree links it | `GET`, `POST /api/projects` |
| `/p/:project/settings/general` | Project settings, General tab: project edit and delete, Targets panel, a link to the Repos page | `GET`, `PATCH`, `DELETE /api/projects/{name}?confirm=true`, `GET .../targets` |
| `/p/:project/repos` | Repos of the project: list, **Add repo** (project_admin), a checkout row for each repo | `GET`, `POST /api/projects/{name}/repos`, `GET .../targets`, `POST /api/repos/{id}/checkout` |
| `/p/:project/repos/:repo` | Repo browser: tree selector (`main` and each agent checkout), tabs Files, Changes, Commits. The query keeps `tree`, `tab`, `path`, `file`, and `commit` | `GET /api/repos/{id}/trees[?refresh=true]`, `GET .../trees/{tree}/files?path=`, `.../file?path=`, `.../diff`, `.../commits?cursor=`, `.../commits/{sha}` |
| `/p/:project/settings/secrets` | Project settings, Secrets tab: the secrets of the project, then the global ones that it can use | `GET /api/secrets?scope=`, `POST /api/secrets`, `PUT`, `DELETE /api/secrets/{scope}/{name}`, `POST .../generate\|reveal`, `GET .../grants` |
| `/p/:project/settings/templates` | Project settings, Templates tab: the templates of the project, then the global ones | `GET`, `POST /api/templates`, `PATCH`, `DELETE /api/templates/{key}`, `template` events |
| `/p/:project/settings`, `/p/:project/secrets`, `/p/:project/templates` | Gone: redirect to `/p/:project/settings/general`, `/p/:project/settings/secrets`, and `/p/:project/settings/templates` (`app/nav.ts` `MOVED_ROUTES`). The redirect keeps the query. | none |
| `/p/:project/agents` | Agents grid of the project, status filter, new agent (a template chooser, then a form, for the project of the route) | `GET`, `POST /api/agents`, `GET /api/templates?project=`, `GET /api/templates/{key}/resolve`, events socket |
| `/agents` | Gone: redirects to `/projects` (`app/nav.ts` `MOVED_ROUTES`). | none |
| `/agents/:id` | Agent detail: edit, delete, roles, quota, leases, conversation (the default tab), terminal, events, sessions | `GET`, `PATCH`, `DELETE /api/agents/{id}`, `pause/unpause/restart/send`, `GET`, `PUT .../roles`, `GET /api/usage/me?agent=`, `GET .../leases?all=`, `WS .../pty`, `GET .../events?since=`, `GET .../sessions`, `GET .../sessions/{session}/transcript?after=&limit=`, `GET .../transcript/stream?after=` (SSE) |
| `/settings/subscriptions` | Settings, Subscriptions tab: subscriptions, detect banner, add wizard; above them the "Harness versions" table (versions, updates, the auto-update policy, and the pins) | `GET`, `POST`, `PATCH`, `DELETE /api/subscriptions[/{name}]`, `GET .../detect`, `POST .../{name}/probe\|verify\|logout\|api-key\|enable\|disable\|stop-all`, `POST /api/subscriptions/fetch-models`, `POST /api/subscriptions/login`, `GET`, `DELETE .../login/{id}`, `POST .../login/{id}/input`, `WS .../login/{id}/pty`; `GET /api/harnesses?latest=true`, `POST /api/harnesses/{name}/update\|pin\|unpin`, `POST /api/harnesses/update`, `POST /api/harnesses/check` |
| `/inbox` | Inbox: questions, notices, mail to the operator, paused threads; kind tabs, open or done, project filter | `GET /api/inbox?status=&kind=&project=`, `GET /api/inbox/counts`, `POST /api/questions/{id}/answer\|ack\|retract`, `POST /api/inbox/mail/{id}/ack`, `POST /api/mail/{id}/release`, `POST /api/mail/threads/{id}/resume` |
| `/questions` | Gone: redirects to `/inbox`. | none |
| `/logs/mail` | Logs, Mail tab: every message, newest first, filters, live mode, the thread inline, reply, compose | `GET /api/mail?thread=&cursor=`, `POST /api/mail`, `POST /api/mail/{id}/reply` |
| `/p/:project/logs/mail` | Project logs, Mail tab: the mail log of one project; reply from the project inbox, make card | `GET /api/mail`, `POST .../inbox/{id}/reply`, `POST .../cards` (`from_mail`) |
| `/p/:project/logs/events` | Project logs, Events tab: the event log of one project, with the kind and agent filters (the agents of the project only) | `GET /api/events?since=&kind=&project=&agent=`, events socket |
| `/p/:project/inbox`, `/p/:project/mail`, `/p/:project/logs` | Gone: redirect to `/p/:project/logs/mail` (`app/nav.ts` `MOVED_ROUTES`). The redirect keeps the query. | none |
| `/p/:project` | Redirects to `/p/:project/board`, the first tab of the project shell. | none |
| `/p/:project/dashboard` | Dashboard: the blocks of the monitors (stat, status, chart, text, table, note), the job-queue strip (below), Edit dashboard (add, change, drag or move, remove, save), and the Monitors tab (`?tab=monitors`: state, last and next run, on/off, Run now, the edit dialog with Test run); see "The project dashboard". | `GET`, `PUT .../dashboard`, `GET .../monitors`, `POST .../monitors`, `POST .../monitors/test`, `PATCH`, `DELETE /api/monitors/{id}`, `POST .../run`, `GET .../results?since=&bucket=`, `GET .../target-grants`, `GET .../jobs/stats?since=` (TER-178) |
| `/p/:project/pipelines` | Pipelines: each CI/CD pipeline with the state of its newest run, the last poll, on/off, Refresh, Edit, Delete; a row opens the recent runs, a run opens its steps; the dialog (GitHub Actions or a script) has a Test poll; see "Pipelines". | `GET`, `POST .../pipelines`, `POST .../pipelines/test`, `PATCH`, `DELETE /api/pipelines/{id}`, `POST .../refresh`, `GET .../runs`, `GET .../runs/{run}` |
| `/p/:project/calendar` | Calendar: the project's events and the global ones in a month grid, a week, or an agenda; an occurrence opens the event (Edit, Remove, Skip); the dialog makes or changes an event; see "Calendar". | `GET .../calendar`, `POST .../calendar/events`, `GET`, `PATCH`, `DELETE /api/calendar/events/{id}`, `POST .../skip`, `GET`, `POST`, `DELETE .../calendar/watches` |
| `/calendar` | The combined calendar of every project and the global calendar (the operator). | `GET /api/calendar`, `GET /api/calendar/global`, `POST /api/calendar/global/events` |
| `/p/:project/board` | Board, Cards tab: drag and drop, New card dialog (the "+" of the backlog column), Edit columns dialog, ETA countdowns, question badges, the version filter (`?version=<name>`) and the version lanes (`?group=version`), card drawer with its questions (see "The board") | `GET .../board`, `PATCH .../board`, `POST .../cards`, `PATCH /api/cards/{id}`, `POST .../comments`, `GET .../events`, `DELETE /api/cards/{id}`, `GET /api/questions?project=\|card=`, `PATCH /api/questions/{id}` (`card`), `POST /api/questions/{id}/answer\|ack\|retract` |
| `/p/:project/versions/:id?`, `/p/:project/board/versions/:id?` | Gone: redirect in one hop to `/p/:project/board` (`app/nav.ts` `MOVED_ROUTES`). Version objects are removed (TER-58), so the old version ID does not go along. The redirect adds the old query (`withQuery`). Old mail can still link `/p/<project>/versions/<id>`. | none |
| `/p/:project/board/tests`, `/p/:project/board/tests/:id` | Board, Tests tab: the test runs (filters, counts bar, links) and one run (counts, compare with the previous run, suites and cases, Failed only, search, output, Download original, link and unlink, delete); see "Test runs" | `GET .../test-runs?card=&label=&name=&status=&cursor=`, `GET /api/test-runs/{id}?status=&q=&cursor=`, `GET .../compare?with=previous`, `GET .../raw`, `PATCH .../links`, `DELETE /api/test-runs/{id}` |
| `/p/:project/board/artifacts/:id?` | Board, Artifacts tab: artifact viewer, upload, delete | `GET .../artifacts`, `.../artifacts/usage`, `POST .../artifacts` (multipart), `POST /api/artifacts/{id}/url` |
| `/p/:project/knowledge/wiki/*path` | Knowledge, Wiki tab: page tree, render, edit, history, create | `GET .../wiki`, `GET`, `PUT /api/wiki/{project/path}`, `revisions`, `diff`, `restore`, `backlinks`, `review` |
| `/p/:project/knowledge/memory` | Knowledge, Memory tab: memory list, detail, graph | `GET .../memories`, `GET /api/memories/{project/MEM-n}/graph` |
| `/p/:project/knowledge/db` | Knowledge, DB tab: the record types, New type | `GET`, `POST .../db/types` |
| `/p/:project/knowledge/db/:type` | The records of a type: query box, table, record dialog, schema editor | `GET .../db/query`, `.../db/records`, `PATCH`, `DELETE .../db/types/{type}` |
| `/p/:project/artifacts/:id?`, `/p/:project/wiki/*path`, `/p/:project/memory`, `/p/:project/knowledge` | Gone: redirect to the Board and Knowledge tabs (`app/nav.ts` `MOVED_ROUTES`). The wiki redirect keeps the page path, and each redirect keeps the query. `/p/:project/knowledge` opens the Wiki tab. | none |
| `/p/:project/schedules` | Gone: redirects to `/p/:project/board/schedules` (`app/nav.ts` `MOVED_ROUTES`). The redirect keeps the query (`?open=`, `?tab=triggers`). | none |
| `/p/:project/spend` | Gone: redirects to `/` (`app/nav.ts` `MOVED_ROUTES`). The daemon's budget alert mail still links it. | none |
| `/p/:project/board/schedules` | Board, Schedules tab: schedules of the project and the global ones, scope filter, run history, run now, enable, create, edit, delete | `GET /api/schedules?project=`, `POST /api/schedules`, `PATCH`, `DELETE /api/schedules/{id}`, `POST .../run\|enable\|disable`, `GET .../runs?limit=&cursor=` (docs/api.md §19) |
| `/schedules` | All schedules, grouped by scope | `GET /api/schedules` and the calls above |
| `/p/:project/board/schedules?tab=triggers`, `/schedules?tab=triggers` | Triggers (spec `on`) and their runs with the event; webhook deliveries, redeliver, and test send; event sources: list, create, enable, rotate, delete, the one-time secret | `GET /api/schedules?kind=trigger`, `GET .../deliveries`, `POST .../deliveries/{id}/redeliver`, `POST .../test`, `GET`, `POST /api/event-sources`, `PATCH`, `DELETE /api/event-sources/{id}`, `POST .../rotate` (docs/api.md §19.4, §19.5, §20) |
| `/settings/targets` | Settings, Targets tab: targets, lifecycle, snapshots (list, take, restore, delete), exec console, leases (grant, extend, revoke, history), the resource limits chip (`limits_info.summary`, for example `2 CPU · 4G`; red with the host agent's `problem`), and the limit fields of the New target dialog for `host` and `docker` targets | `GET /api/targets`, `POST /api/targets/{name}/up\|down\|snapshot\|restore\|status`, `GET .../snapshots`, `DELETE .../snapshots/{snap}`, `POST .../exec`, `WS .../sessions/{id}/ws`, `DELETE .../sessions/{id}`, `GET`, `POST .../leases`, `PATCH`, `DELETE .../leases/{agent}` (docs/api.md §17.7) |
| `/settings/hosts` | Settings, Hosts tab: host agents: status, the version with a `stale` chip for a host agent of another build than the daemon (`stale`, docs/api.md §17.1), capabilities (also the `limits` or `no limits` chip of `capabilities.limits`, with the controllers or the reason as the tooltip), add with a one-time token, revoke | `GET`, `POST /api/hosts`, `DELETE /api/hosts/{name}` (new API, not in docs/api.md yet) |
| `/logs/events` | Logs, Events tab: event log with history; all projects or the selected one, kind and agent filters | `GET /api/events?since=&kind=&project=&agent=`, events socket |
| `/logs/audit` | Logs, Audit tab: audit log, newest first, live mode; actor, action, and project filters | `GET /api/audit?order=desc&actor=&action=&project=`, `audit` frames of the events socket |
| `/settings/account` | Settings, Account tab: session mode, end time, what the browser keeps; password; sign out, sign out everywhere | `GET`, `DELETE /api/session`, `DELETE /api/sessions`, `PUT /api/auth/password` |
| `/settings/software` | Settings, Software tab: the daemon's build (version, git ref, `dirty`, build time) and the stale host agents, with a link to Hosts. The self-update: the newest release with its notes, Check now, Update and Roll back (each asks in the page), the policy and the channel, the updater, the drain, and the update log with the steps of each run. It reads the state again every 3 s while a run runs | `GET /api/system/version` (docs/api.md §4), `GET /api/hosts`, `GET/POST /api/system/update`, `POST /api/system/update/check`, `POST /api/system/update/rollback` (docs/api.md §34) |
| `/settings/harness` | Settings, Harness config tab: the config repo, its changes and diffs, commit, sync, pull, push, secret grants to approve | `GET /api/config/...` (docs/api.md §25) |
| `/settings/secrets` | Settings, Secrets tab: every secret that the caller manages, with a scope filter | `GET /api/secrets`, `POST /api/secrets`, `PUT`, `DELETE /api/secrets/{scope}/{name}`, `POST .../generate\|reveal`, `GET .../grants` |
| `/settings/templates` | Settings, Templates tab: the global templates and those of each project, with a scope filter | `GET`, `POST /api/templates`, `PATCH`, `DELETE /api/templates/{key}`, `template` events |
| `/settings/system-prompts` | Settings, System prompts tab: the system prompt library in the config repo; view, edit, delete, new | `GET`, `POST /api/system-prompts`, `GET`, `PUT`, `DELETE /api/system-prompts/{name}` |
| `/settings/skills/:name?` | Settings, Skills tab: the skill library in the config repo; with a name, the editor of that skill | `GET`, `POST /api/skills`, `GET`, `PUT`, `DELETE /api/skills/{name}`, `GET .../usage` |
| `/secrets`, `/templates`, `/skills/:name?`, `/targets` | Gone: redirect to `/settings/secrets`, `/settings/templates`, `/settings/skills/:name?`, and `/settings/targets` (`app/nav.ts` `MOVED_ROUTES`). The skill redirect keeps the skill name, and each redirect keeps the query. | none |
| `/settings`, `/subscriptions`, `/hosts`, `/config` | Gone: redirect to `/settings/account`, `/settings/subscriptions`, `/settings/hosts`, and `/settings/harness` (`app/nav.ts` `MOVED_ROUTES`). The redirect keeps the query. A quota mail of the daemon still links `/subscriptions`. | none |
| `/logs`, `/mail`, `/events`, `/audit` | Gone: redirect to `/logs/mail`, `/logs/mail`, `/logs/events`, and `/logs/audit` (`app/nav.ts` `MOVED_ROUTES`). The redirect keeps the query. | none |
| `/dev/xss` | Mock only: the hostile payloads of `lib/xss-payloads.ts`, rendered | none |

Artifact upload sends a multipart form: `file`, `name`, `kind`, and
`attach_kind` with `attach_ref` (`features/artifacts/attach.ts`): the
"Attach to" text `page:<path>`, `card:<id>`, or `label:<label>`; since
TER-58 `version:<name>` attaches the label `version:<name>`. A label link
shows the label, and a version label opens the board with that version
filter. A folder goes up as one `bundle`: the
browser builds a tar (`lib/tar.ts`) from the folder pick.

The Targets and Hosts pages end with a footer that names the calls that are
new or mock-only. A daemon without them answers "Not available yet".

Resource limits (docs/api.md §17.2): `lib/limits.ts` has the checks and the
short text of `terrarium_core::limits`. The New target dialog checks a value
before it sends it (`features/targets/create.ts`, `limitsOfForm`); the
daemon stays the judge (`422`). The target chip uses the daemon's
`limits_info.summary`, else the summary of `limits`. Its tooltip says who
shares the budget: each agent on a `host` target, all agents on a `docker`
target. The mock applies the daemon's rules and merges a `PATCH` of
`limits`.

## The project dashboard

`features/dashboard/` (docs/api.md §30). The Dashboard item is the first
item of each project in the tree (`app/nav.ts` `PROJECT_ITEMS`). In the project
shell, Board is the first tab and `/p/:project` opens it. The project name in
the tree still only opens and closes the node.

- **Job queue.** Above the grid, the strip of the job queue (TER-178,
  `JobQueuePanel.tsx`) shows the first-attempt pass rate of the project's
  merge jobs (`GET .../jobs/stats?since=`, docs/api.md §35.4, field
  `merge`; validate jobs from before 2026-11-07 stay out): the rate as a
  whole percent rounded half to even like `terrarium job stats`, "N of M
  first attempts passed", and the quiet counts that sit outside the rate
  (reruns that passed, conflicts, errors, open jobs; all kinds). The
  window select reads 24 hours, 7 days (the default), or 30 days again.
  One stats call per mount and per window change, and one more,
  debounced, after a merge job of the project ends. The strip hides
  until an answer shows a job in the window, and on a failed call (a
  daemon without the route, a 403, the network) without a page error.
  The helpers are `model.ts` `passRateText`, `roundHalfEven`,
  `jobStatNotes`, and `JOB_WINDOWS`; the mock is `api/mock/jobs.ts`.

- **Grid.** CSS grid of four columns on a desktop, two up to 1100 px, one
  on a phone. A block is `s` (one column), `m` (two), or `l` (the full
  width). Each monitor block has a head (the title, and the state as a
  badge with a glyph and a word: never colour alone), a body by its kind,
  and a foot (the time of the last result, the error, and "stale" when
  the last result is older than `3 × period_secs + timeout_secs`).
- **Kinds.** `stat`: the value at 2 rem with its unit, the thresholds as
  text, and a 24-hour sparkline. `status`: the message and the value.
  `chart`: a line chart of the range (1 h, 6 h, 24 h, 7 d) from
  `GET .../results?bucket=` (the mean as a 2 px line, the min-max of
  each bucket as a 10 % wash, labelled warn and crit lines, hairline
  grid, y ticks and three time labels), a crosshair with a readout on
  hover and on the arrow keys, an `aria-label` summary, and a "Data
  table". `text`: monospace, a text node. `table`: the `rows` of the last
  `json` result; number columns align right. `note`: Markdown through the
  safe `Markdown` component.
- **Colours.** The one series colour is `--series-1` (`#2a78d6` light,
  `#3987e5` dark: slot 1 of the dataviz palette, checked with its
  validator). The states use the app's status tokens (`--accent`,
  `--warn`, `--danger`, `--muted`).
- **Edit mode.** "Edit dashboard" copies the layout to a draft. Each
  block gets a tool row: up and down, Edit (the block dialog), Remove.
  A block can also be dragged onto another. `layoutProblems`
  (`model.ts`) shows the errors that the daemon would refuse, and Save
  is off while there are some.
- **Monitors tab.** A table (a list of cards on a phone): the state, the
  last value, the last and next run, an on/off switch, Run now (with the
  raw output), Edit, and Delete. The dialog has the script editor
  (monospace), the target (global and project targets), the schedule
  (interval or cron, with the next runs), the timeout, the retention,
  the kind, and the unit and thresholds. **Test run** calls
  `POST .../monitors/test` and shows the parsed result and the raw
  output before a save. Each row shows the owner (`runs as <actor>`,
  `owned by the project`, `owned by the system`) and the reason when the
  daemon disabled the monitor; the dialog has an **Owner** select
  (docs/api.md §30.7), and sends `owner` only when it changes. Above the
  table, the panel lists the global targets granted to the project
  (`GET /api/projects/{name}/target-grants`).
- **Live.** A `monitor.result` or `dashboard.updated` event of the
  project loads the dashboard again (debounced); the series of a block
  load again when the last result of its monitor changes. A
  `dashboard.updated` does not drop an open draft.
- **Mock.** `api/mock/monitors.ts`: project `terrarium` has ten monitors
  of every kind (one on the stopped target `ci-box`, so it is unknown),
  a week of synthetic history, and a layout with a block of each kind.
  The activity timer runs the due monitors, so values move and statuses
  change. A plain `echo <text>; exit <n>` script runs as written, so the
  Test run of the editor shows its own output.

## Pipelines

`features/pipelines/` (docs/api.md §31): the second item of a project in
the tree.

- **List.** One row for each pipeline: the name and the source, the state
  badge (the status of the newest run, `Poll error` with the error, `Not
  polled yet`, or `Off`; a glyph and a word, not only a colour), the
  newest run (its environment or branch, when, and how long), the last
  poll, the on/off switch, Refresh, Edit, and Delete. A count of the
  pipelines by state is above the table.
- **Runs.** A row opens its recent runs. A run opens a dialog with its
  facts, a link to the CI system (`http` and `https` only), and its steps
  (the jobs of a GitHub run, read on demand).
- **Dialog.** GitHub (repo, workflow, branch, token secret, API URL) or a
  script (target, script, timeout, `VAR=secret` lines), the interval and
  the runs to keep. Test poll shows the runs or the error, and the raw
  output of a script, before a save.
- **Live.** A `pipeline.*` event of the project loads the list again.
- **Mock.** `api/mock/pipelines.ts`: project `terrarium` has a GitHub
  pipeline, an Octopus Deploy script pipeline, and a pipeline that fails;
  the activity timer polls the due ones, so runs start and end.

## Calendar

`features/calendar/` (docs/api.md §33): the third item of a project in the
tree (`/p/:project/calendar`), and the Calendar item above the tree for
the combined view (`/calendar`). `model.ts` has the pure parts (the month
grid, the week, the agenda, the dialog form and its body); `recur.ts` has
the repeat rules after the daemon's `calendar/recur.rs`, for the mock and
the zone helpers.

- **Views.** Month (whole weeks from Monday), Week (seven day columns),
  and Agenda (31 days by day). A phone (760 px or less) opens the agenda.
  Times show in the browser's zone. Each kind has its colour; a global
  event has a dashed edge and the word `global`; a running occurrence has
  a ring.
- **Event.** An occurrence opens its event: the occurrence, the zone, the
  repeat, the skips, the assignees, the links, and the markdown
  description. Edit, Remove, and Skip this occurrence show only with
  `can_edit`.
- **Dialog.** The title, the kind, the calendar (the combined view only),
  all day or the times in the event's zone, the repeat (daily, weekly
  with weekdays, monthly with a month day; an end date or a count), the
  assignees, the links, and the description. `New event` shows only with
  `can_write`, so a viewer without the write right reads only.
- **Watch.** The project page watches the calendar for the operator Inbox;
  the combined page watches the global calendar.
- **Live.** A `calendar.*` event loads the view again.
- **Mock.** `api/mock/calendar.ts`: project `terrarium` has a weekly patch
  window, a release freeze, a deploy, and a monthly task; the global
  calendar has a host window. The mock has no notice tick.

## Home: hosts

`features/home/HostCards.tsx` shows one card for each host agent, online
hosts first (docs/api.md §17.1). The model is `features/home/hosts.ts`.

- The page polls every 10 seconds: `GET /api/hosts`, then
  `GET /api/hosts/{name}/metrics?minutes=15` for each host with
  `capabilities.metrics`. The daemon sends no event for a sample. While a
  poll runs, the cards keep the last data.
- Each card has CPU and memory (the share, 0 to 100) and network in and
  out, disk read and write (bytes per second). Each metric shows its
  current value as text and a line over the last 15 minutes. A rate line
  scales to its peak (at least 1 KB/s); the text under it gives the peak.
  A gap of more than 35 seconds breaks the line.
- Point at a line, or focus it and use the arrow keys, to read the value
  and the time of one sample. Escape goes back to the latest.
- Disk space shows the 3 fullest filesystems; "N more" shows all. The bar
  is amber from 80% and red from 90%, and the text says "filling" or
  "almost full", so the level is not in the colour only.
- States: `online`; `stale` (online, but no sample in 30 seconds: the
  values are dim, with the time of the last sample); `offline` (the last
  samples, dim, when the daemon has them). A host agent without the
  `metrics` capability shows a note: install the new host agent on the
  host (`just host::install`, or the one-line installer of Settings >
  Hosts).
- The host name links to Settings > Hosts (`/settings/hosts`).
- The mock (`api/mock/hostMetrics.ts`) makes a sample every 10 seconds
  from the time for `lab1` and `build1`; `nas` is an older host agent and
  `pi-cluster` is offline.

## The board

`features/board/BoardPage.tsx` shows the columns, and the card drawer
opens from `?card=<id>`. A `card` or `board` event of the project reloads
the board after 300 ms.

**New card.** The "+" button in the head of the backlog column opens the
**New card** dialog. A board with no backlog column has the button on its
first column (`features/board/newCard.ts` `newCardColumn`). The dialog has
the title, the Markdown description, the column, the assignee, the labels,
the cards that the card depends on, and an optional ETA. Enter in the title
or Ctrl+Enter in the description creates the card
(`POST /api/projects/{name}/cards`). **Create and add another** keeps the
dialog open with empty fields, and keeps the column, the assignee, and the
labels. The dialog shows the field errors and the API errors. On a phone,
the dialog fills the screen. Escape closes it, and the focus goes back to
the "+" button.

**Columns.** **Edit columns** in the toolbar opens a dialog
(`features/board/ColumnsEditor.tsx`, rules in `columns.ts`). It lists the
columns with a drag handle and up and down buttons, the name (a rename
sends `rename_from`, and the cards stay), the title, the WIP limit, the
role (none, Working, Blocked; a role moves from the column that had it),
and **Remove**. `backlog` stays first and `done` last; they take no WIP
limit and no role. **Remove** is off, with a tooltip, for a fixed column
and for a column with cards. **Add column** adds a row before `done`.
The dialog checks the rules before the save (one Working column, at most
one Blocked column, names, titles, limits) and shows the errors inline;
**Save columns** sends the whole list (`PATCH /api/projects/{name}/board`)
and shows an API error in the dialog. A limit under the count of a column
is allowed, with a warning; the column header then shows the count red
until cards leave. The column headers show the titles. A move to the
column with the role `blocked` asks for a comment. On a phone, the dialog
fills the screen.

**Versions.** A version is a card label (TER-58): `version:vnext` for
the open version, `version:X.Y.Z` for a release. There are no version
objects. A card can have several version labels, or none
(`features/board/versionLabels.ts`). The labels show as chips on the card,
with the other labels, and the drawer edits them in the Labels field.

**Version filter.** The Filters row has a **version** select: Any version,
each version on the board's cards (vnext first, then the releases newest
first, a pre-release after its release, then other names A to Z), and No
version (the cards with no version label). The choice is in the URL
(`?version=<name>`, `?version=none`), so a reload and a link keep it. A
version in the URL that no card has stays in the select. The filter counts
in the Filters badge of a phone.

**Version lanes.** **Group by version** (`?group=version`) shows the cards
in version rows ("lanes") (`features/board/lanes.ts`, `LanesBoard.tsx`):
one row for each version label on the board's cards, in the order of the
filter, and a last row "No version". Without it, the board has one plain
lane with no row head. An old `?rows=` in the URL has no effect.
localStorage keeps the collapsed rows of each project
(`terrarium.board.lanes.<project>`); the page ignores an old `rows` value
in it. Each row has all the columns, under one row of column heads. A row
head has a chevron (collapse), the version name, and the card count. A card
with several version labels shows in each of their rows, with "also in
<name>". The WIP limits and the counts of the column heads are for the
whole column; each cell shows its own count, muted. A drag inside a row
moves the card as before. A drag into another row changes the card's
labels: the label of the old row goes, the label of the new row comes
(none for "No version") (`PATCH /api/cards/{id}` with `labels`), then the
card moves to the column of the drop. On a phone, each column holds the
rows as sections that collapse, and the Labels field of the card drawer
changes the version.

**ETAs.** A card with an `eta_at` (docs/api.md §8) shows a countdown chip
below its title: "34m 05s left" under an hour, "1h 12m left" above, and
"overdue 12m" in the danger colour. A thin bar under the chip shows the
time used since `eta_set_at`; it is in the warn colour after 90%. The
title and the aria-label give the absolute ETA and who set it. A done or
archived card shows no countdown. The board has one clock (`now`) for all
the chips: it ticks each second while a countdown is under an hour, each
30 s otherwise, and stops when no card counts down (`lib/eta.ts`
`tickInterval`). The card drawer shows the ETA, a duration input (`45m`,
`2h30m`, `1d`) to set or change it, and **Clear ETA**. The input checks
the value with the daemon's rule (`resolveEta`: in the future, at most 30
days away) before it sends `PATCH /api/cards/{id}` with `eta_at`. A done
card shows "done in X (estimated Y)" from `eta_set_at`, `eta_at`, and
`done_at`.

**Questions.** A card with open linked questions, bundles, or notices
(docs/api.md §6.8 "Cards") shows a badge "? N" next to its ID. N counts
each open item once (a bundle too). The colour is that of the highest
priority, as in the Inbox: `chip info` (normal), `chip bad` (urgent), and
`chip bad strong` (critical). The title and the aria-label list the
items. The board reads the open questions of the project once
(`GET /api/questions?project=`), and a `question` event of the project
reads them again after 300 ms. The card drawer has a **Questions**
section (`features/board/CardQuestions.tsx`,
`GET /api/questions?card=<id>&status=all`). It uses the Inbox cards
(`QuestionCard`, `BundleCard`), so the operator answers, acknowledges, or
retracts there; each change refreshes the Inbox counts and the badges.
The closed ones are under **Answered (N)**. **Link a question** picks an
open question of the project with no card, and **Unlink** on each item
sends `PATCH /api/questions/{id}` with `card: null`. A `question` event
whose `card_id` or `previous_card_id` is the card reads the section
again. The pure helpers are in `features/board/questions.ts`
(`cardQuestionBadges`, `linkable`, `isCardQuestionEvent`).

**Test chips.** A card with a linked test run (docs/api.md, test runs)
shows a chip with the latest run (`Card.tests`): "✓ 120 passed",
"✗ 3 failed" (failed plus errored), or "– no tests", as a colour and as
text. A click opens the run, not the card. A `test_run` event of the
project reloads the board. The card drawer has a **Tests** section
(`features/tests/parts.tsx` `LinkedRuns`, `GET .../test-runs?card=<id>`). It lists the linked runs,
newest first, and a combined status of the newest run of each name: failed
when one failed, else passed, else empty. A failed run on a card in review
changes nothing on the board: no card moves.

## Test runs

The **Tests** tab of the board (`features/tests`) shows the test runs that
agents push with `terrarium test push` (docs/api.md, test runs). The daemon
parses JUnit XML, TRX, CTRF, libtest JSON, Vitest and Jest JSON, and TAP.

- **The list** (`TestRunsPage.tsx`, `/p/<project>/board/tests`): newest
  first, with the status (icon and word), the name, the format and tool,
  who pushed it and when, the branch and the commit, the duration, the
  counts as a stacked bar and as text, and the linked cards and labels. A
  version label (`version:1.4.0`) opens the board with that version filter.
  The filters (name, status, card, label) are in the URL
  (`?name=&status=&card=&label=`). **Load more** reads the next page. A
  `test_run` event of the project reloads the list.
- **The run** (`TestRunPage.tsx`, `/p/<project>/board/tests/<id>`): the
  big counts (passed, failed, errored, skipped) with an icon each, the
  duration, the format, the tool, the file, the commit, the branch, and the
  linked cards and labels (unlink with ×, link a card by ID). **Compare
  with the previous run** reads `GET /api/test-runs/{id}/compare?with=previous`
  and shows Newly failing and Fixed open at the top, then Still failing,
  New tests, and Removed tests. The suites are groups that open and close,
  with their counts. A case shows its status as an icon and a word, its
  name, and its duration; a click shows the message, the trace, stdout,
  and stderr in `<pre>` text nodes. The text has no ANSI codes
  (`model.ts` `stripAnsi`) and never renders as HTML. **Failed only** is
  on at first when the run has a failed or an errored case. The search box
  and Failed only go to the daemon (`?q=&status=failed`), which pages the
  cases (**Load more**). **Download original** reads
  `GET /api/test-runs/{id}/raw` with the session and saves it from an
  object URL. **Delete** asks once.
- The pure logic is in `features/tests/model.ts`: the bar segments, the
  counts text, the chip text, the run status, the combined status, the
  case filter, the suite groups, the compare sections, `diffCases` (the
  mock's compare), and the durations. The mock (`api/mock/testRuns.ts`)
  seeds project `terrarium` with a run in each format; two `unit` runs
  show the compare, and some messages have ANSI colour codes. On a phone
  the rows stack and the filters take the full width.

## Security notes

See `docs/security-review.md` F3, F4, and F5.

### Untrusted HTML (F3)

- Wiki pages and markdown artifacts come from agents.
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
- A wiki `db` block becomes `<div class="db-embed" data-db-query>`. The
  query is text in an attribute: the sanitizer keeps the attribute on a
  `div` only, at most 2000 characters. `Markdown` reads it with `dataset`
  and mounts `components/DbEmbed.tsx`, which runs the query in the project
  of the page and shows each value as a text node (a ref is a link to the
  record in the DB tab). `components/DbEmbed.test.ts` and
  `lib/sanitize.test.ts` check hostile queries and values.
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

## The repo browser

`/p/:project/repos/:repo` (`features/repos/RepoPage.tsx`, docs/api.md
§5.3). The tree select has `main` (the daemon's mirror of the default
branch) and each agent checkout as `<agent> · <branch>`, with the ahead and
behind counts and a "changed" mark. **Refresh** fetches `main` again.

- **Files**: breadcrumbs, folders first, sizes. A symlink shows its target
  and is not a link. A text file has line numbers. A Markdown file shows its
  source; **Rendered** puts it through the safe `Markdown` component. A PNG,
  JPEG, GIF, or WebP file shows as an image from a blob URL with a fixed
  type (never SVG). Another binary file shows its size only. Over 1 MiB, the
  view shows the first 1 MiB and says so.
- **Changes**: for an agent tree, the committed diff (base..HEAD), then the
  uncommitted part: staged, not staged, and the untracked paths. `main` is
  the base, so it has no diff.
- **Commits**: newest first, **Load more**; a commit shows its message and
  its diff.

The diff viewer (`components/DiffView.tsx`, `lib/diff.ts`) has a file list
with +/- counts, one collapsible block for each file with a sticky header,
and the old and new line numbers. The colours are theme tokens. On a phone
only the code blocks scroll sideways. Agents write every name, file, and
diff: they go into text nodes only. The mock (`api/mock/repoTrees.ts`) has
`main` and two agent trees of the repo `terrarium`.


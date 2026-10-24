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

Other scripts:

| Script | What it does |
|---|---|
| `npm run build` | Builds to `dist/`. The mock is not in this build. |
| `npm run test` | Runs the unit tests with Vitest. |
| `npm run check` | Runs `tsc --noEmit` on the app and the config, then ESLint with the Solid plugin. |

## Authentication

The client sends `Authorization: Bearer <token>` on each request. It reads the
token from `localStorage` key `terrarium.token`. When the daemon answers 401,
the app shows a token screen. The token is `TERRARIUM_TOKEN` from `.env`.

Browsers cannot set headers on a WebSocket. Sockets send the token as the
`?token=` query parameter.

## Layout

```
src/
  api/            typed client, transport, socket protocols, mock
    types.ts      domain types (PLAN §5)
    http.ts       fetch transport: token, JSON, errors (ApiError, AuthError)
    client.ts     the Api interface and the HTTP implementation (PLAN §17)
    sockets.ts    PTY and events protocols, reconnecting event stream
    mock/         in-memory Api, seed data, fake PTY
    index.ts      picks mock or HTTP from VITE_MOCK
  stores/         all app-wide state (auth, projects, agents, events, board, ...)
  components/     shared UI: Terminal, UsageTile, StatusBadge, ArtifactViewer, ...
  features/       one folder per view
  lib/            pure helpers: format, markdown and wikilinks, force layout
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
`board` events), the mail and inbox counts in the sidebar
(`stores/unread.ts`), the quota banner and the hold badges
(`stores/quota.ts`), the subscription list (`quota.*` events) and the detect
banner (`subscription.offer`), the home queue and usage strip, the Event
log, and the agent's Events tab.

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

## Pages and the API

| Route | View | API calls |
|---|---|---|
| `/` | Home: usage strip, waiting-on-you queue | `GET /api/subscriptions`, `GET /api/usage?bucket=hour&group=subscription`; tile detail: `GET /api/usage?subscription=&group=project\|agent\|model`, `GET .../quota`; per project: `versions?status=in_review`, `wiki?status=ready_for_review`, `board`, `inbox?unread=true` |
| `/projects` | Projects: list, create | `GET`, `POST /api/projects` |
| `/p/:project/settings` | Project edit and delete, Repos panel | `GET`, `PATCH`, `DELETE /api/projects/{name}?confirm=true`, `GET`, `POST .../repos`, `POST /api/repos/{id}/checkout` |
| `/agents` | Agents grid, new agent | `GET`, `POST /api/agents`, events socket |
| `/agents/:id` | Agent detail: edit, delete, roles, quota, terminal, events, sessions | `GET`, `PATCH`, `DELETE /api/agents/{id}`, `start/stop/restart/send`, `GET`, `PUT .../roles`, `GET /api/usage/me?agent=`, `WS .../pty`, `GET .../events?since=`, `GET .../sessions` |
| `/subscriptions` | Subscriptions, detect banner, add wizard | `GET`, `POST`, `DELETE /api/subscriptions[/{name}]`, `GET .../detect`, `POST .../{name}/probe\|verify\|logout\|api-key`, `POST /api/subscriptions/login`, `GET`, `DELETE .../login/{id}`, `POST .../login/{id}/input`, `WS .../login/{id}/pty` |
| `/mail` | Threads, compose and reply as operator | `GET /api/mail`, `POST /api/mail`, `POST /api/mail/{id}/read`, `POST /api/mail/{id}/reply` |
| `/p/:project/inbox` | Project inbox, make card | `GET .../inbox`, `POST .../inbox/{id}/read`, `POST .../inbox/{id}/reply`, `POST .../cards` (`from_mail`) |
| `/p/:project/board` | Board, drag and drop, card drawer | `GET .../board`, `PATCH /api/cards/{id}`, `POST .../comments`, `GET .../events`, `DELETE /api/cards/{id}` |
| `/p/:project/memory` | Memory list, detail, graph | `GET .../memories`, `GET /api/memories/{project/MEM-n}/graph` |
| `/p/:project/wiki/*path` | Page tree, render, edit, history, create | `GET .../wiki`, `GET`, `PUT /api/wiki/{project/path}`, `revisions`, `diff`, `restore`, `backlinks`, `review` |
| `/p/:project/artifacts/:id?` | Artifact viewer, upload, delete | `GET .../artifacts`, `.../artifacts/usage`, `POST .../artifacts` (multipart), `POST /api/artifacts/{id}/url` |
| `/p/:project/versions/:id?` | Version timeline, review, new version | `GET`, `POST .../versions`, `POST /api/versions/{id}/submit|approve|reject|release`, `compare` |
| `/p/:project/spend` | Usage by day, agent, model, and subscription | `GET /api/usage?project=&bucket=day&group=agent\|model\|subscription` |
| `/targets` | Targets, lifecycle, exec console | `GET /api/targets`, `POST /api/targets/{name}/up\|down\|snapshot\|restore`, `POST .../exec`, `WS .../sessions/{id}/ws`, `DELETE .../sessions/{id}` (new API, not in docs/api.md yet) |
| `/hosts` | Host agents: status, capabilities, add with a one-time token, revoke | `GET`, `POST /api/hosts`, `DELETE /api/hosts/{name}` (new API, not in docs/api.md yet) |
| `/events` | Event log with history | `GET /api/events?since=&kind=&project=&agent=`, events socket |
| `/audit` | Audit log | `GET /api/audit?actor=&action=&project=` |

Artifact upload sends a multipart form: `file`, `name`, `kind`, and
`attach_kind` with `attach_ref`. A folder goes up as one `bundle`: the
browser builds a tar (`lib/tar.ts`) from the folder pick.

The Targets and Hosts pages end with a footer that names the calls that are
new or mock-only. A daemon without them answers "Not available yet".

## Security notes

- Wiki pages come from agents. `lib/markdown.ts` escapes raw HTML and allows
  only safe URL schemes. Tests cover this.
- The API-key form posts the key once (`POST .../api-key`) and clears the
  field. No endpoint returns a key. A login code goes to `.../input` and the
  daemon types it into the PTY; the page never shows it again.
- A host token shows once, in the add-host dialog only. Closing the dialog
  drops it.
- HTML artifacts and bundles render in `<iframe sandbox="allow-scripts">`
  without `allow-same-origin`. In real mode they load from the artifact origin.

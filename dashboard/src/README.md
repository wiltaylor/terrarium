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
Set `TERRARIUM_DAEMON_URL` to use a different daemon.

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

## Socket protocols

PTY sockets (`/api/agents/{id}/pty` and `/api/subscriptions/login/{id}/pty`):

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
| `/` | Home: usage strip, waiting-on-you queue | `GET /api/subscriptions`, `GET /api/usage`, per project: `versions`, `wiki`, `board`, `inbox` |
| `/agents` | Agents grid | `GET /api/agents`, events socket |
| `/agents/:id` | Agent detail: terminal, events, sessions | `GET /api/agents/{id}`, `start/stop/restart/send`, `WS /api/agents/{id}/pty`, `GET .../events`, `GET .../sessions` |
| `/subscriptions` | Subscriptions and the add wizard | `GET /api/subscriptions`, `detect`, `login`, `WS login/{id}/pty`, `login/{id}/input`, `api-key`, `verify`, `probe`, `logout` |
| `/mail` | Threads, compose as operator | `GET /api/mail`, `POST /api/mail`, `POST /api/mail/{id}/reply` |
| `/p/:project/inbox` | Project inbox, make card | `GET /api/projects/{name}/inbox`, `POST /api/projects/{name}/cards` (`from_mail`) |
| `/p/:project/board` | Board, drag and drop, card drawer | `GET /api/projects/{name}/board`, `PATCH /api/cards/{id}`, `POST /api/cards/{id}/comments`, `GET /api/cards/{id}/events` |
| `/p/:project/memory` | Memory list, detail, graph | `GET /api/projects/{name}/memories`, `GET /api/memories/{id}/graph?depth=1` |
| `/p/:project/wiki/*path` | Page tree, render, edit, history | `GET /api/projects/{name}/wiki`, `GET/PUT /api/wiki/{id}`, `revisions`, `diff`, `restore`, `backlinks`, `review` |
| `/p/:project/artifacts/:id?` | Artifact viewer | `GET /api/projects/{name}/artifacts`, the signed URL on the artifact origin |
| `/p/:project/versions/:id?` | Version timeline and review | `GET /api/projects/{name}/versions`, `POST /api/versions/{id}/approve|reject|submit|release`, `compare` |
| `/targets` | Targets and host agents | `GET /api/targets`, `POST /api/targets/{id}/up|down|snapshot`, `GET /api/hosts` |
| `/events` | Event log | events socket |
| `/audit` | Audit log | `GET /api/audit` |

`GET /api/hosts` and `GET /api/health` (`{version, uptime}`) are not in the
§17 list yet. The dashboard expects both.

## Security notes

- Wiki pages come from agents. `lib/markdown.ts` escapes raw HTML and allows
  only safe URL schemes. Tests cover this.
- HTML artifacts and bundles render in `<iframe sandbox="allow-scripts">`
  without `allow-same-origin`. In real mode they load from the artifact origin.

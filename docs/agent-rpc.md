# Agent RPC: the CLI inside the sandbox

Inside a sandbox, the `terrarium` CLI cannot reach the daemon's HTTP socket.
It reaches only its own runner socket (`TERRARIUM_SOCKET`). This document is
the contract between the CLI (client) and the daemon (server) over that
socket. The framing is the runner's newline-delimited JSON from
`docs/runner.md`.

## Identity

The runner socket belongs to one agent. The daemon resolves the caller to
`Actor::Agent { id, project, roles }` from the socket, never from a header.
The handler is `crates/terrariumd/src/agent_rpc.rs`. It puts the actor on
the request before the router sees it; no client can set it.

The `X-Terrarium-Agent` header on the daemon's Unix socket names an agent
only when the daemon runs with `--insecure-agent-header` (the integration
tests do). Else a request with the header gets `401 unauthorized`.

## Method `http`: tunnel the HTTP API

The CLI reuses its HTTP client code. It sends the same request it would send
over HTTP, wrapped in a runner request:

```json
{"type":"request","id":7,"method":"http","params":{
  "method":"POST",
  "path":"/api/mail",
  "query":{"limit":"20"},
  "body":{"to":"bob","subject":"hi","body":"..."}
}}
```

The daemon runs it through its own router with the agent actor and answers:

```json
{"type":"response","id":7,"result":{
  "status":201,
  "headers":{"x-next-cursor":"..."},
  "body":{...}
}}
```

Rules:

- `path` starts with `/api/` and has no `?`. Put the query in `query`; the
  values can be strings, numbers, or booleans.
- `body` is JSON or absent. An artifact upload is JSON with base64 data
  (below).
- Every HTTP status comes back in `result.status`. The CLI maps 4xx/5xx to
  its normal error output. A runner-level `error` means the tunnel itself
  failed (bad method, socket closed, daemon down).
- WebSocket and SSE endpoints are not reachable through the tunnel. Waiting
  commands use long-poll instead (below).
- Policy is enforced by the daemon exactly as over HTTP. The tunnel adds no
  rights.
- `result.headers` carries `x-next-cursor` when the list has more.
- A non-JSON answer body comes back as a JSON string. A body over 16 MiB is
  a runner-level `too_large` error.
- One line on the runner socket is at most 64 MiB.
- Tunnel errors: `invalid_params` (bad `params`), `unauthorized` (the agent
  was deleted), `unavailable` (the daemon shuts down), `internal`.

## Long-poll for `wait` commands

`terrarium mail wait`, `terrarium card wait`, and `terrarium version wait`
call list endpoints with `?wait=<seconds>` (max 300). The daemon holds the
request until a matching item appears or the time runs out, then answers as
normal. An empty array means timeout. The CLI loops until its own
`--timeout` passes.

In the daemon now: `GET /api/mail`, `GET /api/projects/{name}/inbox`, and
`GET /api/projects/{name}/cards`. A new `mail` event (or `card` event for
cards) wakes the request, and the daemon runs the query again. A `wait`
above 300 is cut to 300.

## Uploads: artifact bytes

In v1 the CLI uploads an artifact through the `http` method, with the JSON
form of `POST /api/projects/{name}/artifacts` (`docs/api.md` §12.2). The
bytes go base64-encoded in `data_base64`:

```json
{"type":"request","id":8,"method":"http","params":{
  "method":"POST",
  "path":"/api/projects/terrarium/artifacts",
  "body":{"name":"report.html","content_type":"text/html",
          "data_base64":"PGh0bWw+...","kind":"file",
          "attach":{"kind":"page","ref":"reports/perf"}}
}}
```

The answer is `201` with the artifact record. Rules:

- The data is at most 32 MiB (and at most the daemon's `max_file_bytes`).
  A larger upload gets `413 too_large`.
- The CLI tars a directory first and sends it with `kind: "bundle"`.
- `content_type` is a hint only. The daemon sniffs the type from the bytes.

Future: a chunked `upload` method for files over 32 MiB. The CLI would send
`upload` with `{project, name, content_type, attach, size}`, get an
`upload_id`, send `upload.chunk` frames of at most 256 KiB with
`{upload_id, seq, data}`, then `upload.finish` with the sha256. The daemon
does not have this method yet.

## Native methods

- `ping` -> `{"result":"pong"}`
- `whoami` -> `{"result":{"agent","name","project","harness","roles","status"}}`

Both are answered by the runner itself without the daemon.

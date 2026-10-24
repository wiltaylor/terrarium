# Agent RPC: the CLI inside the sandbox

Inside a sandbox, the `terrarium` CLI cannot reach the daemon's HTTP socket.
It reaches only its own runner socket (`TERRARIUM_SOCKET`). This document is
the contract between the CLI (client) and the daemon (server) over that
socket. The framing is the runner's newline-delimited JSON from
`docs/runner.md`.

## Identity

The runner socket belongs to one agent. The daemon resolves the caller to
`Actor::Agent { id, project, roles }` from the socket, never from a header.
The `X-Terrarium-Agent` header is for tests only and must not be honoured
once the runner handler is wired.

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

- `body` is JSON or absent. Binary upload (artifacts) uses `method`
  `upload` (below), not `http`.
- Every HTTP status comes back in `result.status`. The CLI maps 4xx/5xx to
  its normal error output. A runner-level `error` means the tunnel itself
  failed (bad method, socket closed, daemon down).
- WebSocket and SSE endpoints are not reachable through the tunnel. Waiting
  commands use long-poll instead (below).
- Policy is enforced by the daemon exactly as over HTTP. The tunnel adds no
  rights.

## Long-poll for `wait` commands

`terrarium mail wait`, `terrarium card wait`, and `terrarium version wait`
call list endpoints with `?wait=<seconds>` (max 300). The daemon holds the
request until a matching item appears or the time runs out, then answers as
normal. An empty array means timeout. The CLI loops until its own
`--timeout` passes.

## Method `upload`: artifact bytes

```json
{"type":"request","id":8,"method":"upload","params":{
  "project":"terrarium","name":"report.html","content_type":"text/html",
  "attach":{"kind":"page","ref":"reports/perf"},
  "size":12345
}}
```

The daemon answers `{"result":{"upload_id":"..."}}`. The CLI then sends
`{"type":"request","method":"upload.chunk","params":{"upload_id":"...","seq":0,"data":"<base64>"}}`
frames of at most 256 KiB, then `upload.finish` with the sha256. The daemon
answers `upload.finish` with the artifact record. Directories are tarred by
the CLI first and marked `kind: "bundle"`.

## Native methods

- `ping` -> `{"result":"pong"}`
- `whoami` -> `{"result":{"agent_id","name","project","harness","roles"}}`

Both are answered by the runner itself without the daemon.

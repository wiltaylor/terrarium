# The agent runner

The `terrarium-runner` crate holds one harness in a PTY. It keeps the parsed
screen, records the raw output and the events, serves a Unix socket for hooks
and the agent CLI, and runs the status machine of [PLAN §5](PLAN.md#5-domain-model).
One `Runner` holds one agent.

## Files

The runner writes these files. `<state>` is `RunnerConfig::state_dir`, for
example `/state`. `<run>` is `RunnerConfig::runtime_dir`, for example
`/run/terrarium`.

| Path | Content |
|---|---|
| `<state>/agents/<ulid>/pty.log` | The raw PTY bytes. The runner appends to it. |
| `<state>/agents/<ulid>/pty.log.1` | The archive. When `pty.log` grows past `log_max_bytes` (default 8 MiB), it moves here and a new `pty.log` starts. The runner keeps one archive. |
| `<state>/agents/<ulid>/events.jsonl` | One `EventRecord` as JSON on each line. The runner appends to it, so the history stays across restarts. |
| `<run>/agents/<ulid>.sock` | The runner socket, mode `0600`. The runner removes a stale socket before it binds, and removes the socket after the child exits. |

The runner adds `TERRARIUM_SOCKET` (the socket path) and `TERRARIUM_AGENT`
(the agent ULID) to the child env. When the `LaunchSpec` sets one of them, the
runner keeps the value from the spec. A sandbox that shows the socket at a
different path sets `TERRARIUM_SOCKET` in the spec. The runner also sets
`TERM=xterm-256color` when the spec does not set `TERM`.

## The socket protocol

The socket carries newline-delimited JSON. Each line is one message. The
`type` field names the kind. A client can send many messages on one
connection.

### Hook

`terrarium hook <event>` sends this message. The runner sends no answer.

```json
{"type":"hook","hook":"Stop","payload":{"session_id":"..."}}
```

`hook` is the hook name as the harness calls it. `payload` is the JSON that
the harness wrote to the stdin of the hook. The runner makes a `HookPayload`
from the two fields and the adapter's `HarnessId`. Then it calls
`Adapter::hook_event` and gets the events.

`terrarium hook` always exits 0, so that a hook never breaks the harness. It
prints nothing when it succeeds. When `TERRARIUM_SOCKET` is not set, or the
runner does not answer, it writes one line to stderr and exits 0. Stdin that
is not JSON goes in the payload as `{"raw": "<text>"}`. Empty stdin gives a
`null` payload. The command uses no async runtime, so it starts fast.

### Request and response

The agent CLI sends requests. The runner answers each request with one
response that carries the same `id`. The `id` can be any JSON value.

```json
{"type":"request","id":1,"method":"whoami","params":{}}
{"type":"response","id":1,"result":{"agent":"01K...","name":"alice","project":"demo","harness":"claude","status":"idle"}}
{"type":"response","id":2,"error":{"code":"method_not_found","message":"unknown method `x`"}}
```

A response has `result` or `error`, never both. The runner answers two
methods itself:

| Method | Result |
|---|---|
| `ping` | `"pong"` |
| `whoami` | `agent`, `name`, `project`, `harness`, `status` |

The runner sends every other method to the `RequestHandler` that the daemon
gives to `Runner::spawn`. The handler gets a `RequestContext` with the agent
identity. The runner knows the caller by the socket: each agent has its own
socket. `NoHandler` answers `method_not_found` for every method.

Error codes:

| Code | Cause |
|---|---|
| `parse_error` | The line is not a valid message, or it is longer than 4 MiB. The `id` is `null`. |
| `invalid_request` | The client sent a `response`. |
| `method_not_found` | No one knows the method. |

## The status machine

| From | Cause | To |
|---|---|---|
| (spawn) | The runner starts the child. | `starting` |
| `starting` | The prompt is ready for the first time. | `idle` |
| any live status | A `TurnStart` event (the prompt-submit hook). | `working` |
| any live status | A `TurnEnd` event (the stop hook). | `idle` |
| any live status | A `Notification` with `question: true`, or a `Question` event. | `blocked` |
| `working` | The adapter's `screen_hint` says `Question`. | `blocked` |
| `working` | No hook and no PTY output for `stall_timeout` (default 5 minutes). | `blocked` |
| `blocked` (stall) | New PTY output while the turn is open. | `working` |
| `blocked` | A `ToolUse` or `ToolFinished` event. | `working` |
| `blocked` | `Runner::answer` or `Runner::answer_keys`. | `working` when a turn is open, else `idle` |
| any | The child exits after `Runner::kill`. | `stopped` |
| any | The child exits and no one asked. | `error` |

Each transition is an `Event::StatusChanged { from, to, reason }`. The runner
appends every event to `events.jsonl` and publishes it on the channel of
`Runner::subscribe_events`. The adapter events from hooks go the same way. A
child exit also gives an `Event::Exit { code }`.

## Typing a turn

`Runner::type_turn(text)` queues the turn and returns a `Delivery` handle. The
runner types turns one at a time, in order:

1. The runner waits until the status is `idle` and the prompt is ready.
2. It writes `Adapter::type_turn(text)`. The default is bracketed paste
   (`ESC[200~ text ESC[201~`) and then Enter.
3. It waits `submit_timeout` (default 10 s) for the prompt-submit hook. The
   hook resolves the handle with `Ok(())`.
4. When the hook does not arrive, the runner types the turn again when the
   prompt is ready.
5. After `submit_attempts` tries (default 2), the handle resolves with
   `DeliveryError::NoSubmitHook`, and the runner raises
   `Event::DeliveryFailed`.

A turn that is queued while the agent is `working` waits for the stop hook.
When the child exits, every queued turn resolves with `DeliveryError::Exited`.

The runner decides "the prompt is ready" in this order:

1. It calls `Adapter::screen_hint` on the screen. `PromptReady` means ready.
   `Busy`, `Question`, and `NotReady` mean not ready.
2. For `Unknown`, it asks the `ReadyDetector`. The default detector needs no
   PTY output for 300 ms, and a last non-empty screen line that ends in `>`,
   `❯`, or `›`. `DefaultReadyDetector::new(quiet, pattern)` sets a different
   quiet time and a regex for the line.

## Attaching

- `Runner::attach()` gives a `ScreenSnapshot` and an output receiver. The
  snapshot and the receiver have no gap and no overlap. A client writes
  `snapshot.formatted` to an empty terminal, then each output chunk.
- `Runner::write(bytes)` sends keystrokes. `Runner::resize(rows, cols)` sets
  the PTY size and the screen size.
- `Runner::screen()` gives the text lines and the cursor.

## How an adapter plugs in

An adapter implements `terrarium_core::Adapter`. The runner uses these
methods:

| Method | Use in the runner |
|---|---|
| `id` | The `harness` of each `HookPayload`, and `whoami`. |
| `hook_event` | Turns each hook message into events. These events drive the status machine. |
| `screen_hint` | Tells when the prompt is ready, and when a question shows. |
| `type_turn` | The keystrokes for a turn. |
| `answer` | The keystrokes for `Runner::answer`. |

The adapter's `launch` gives the `LaunchSpec`. The supervisor calls `launch`
and gives the spec to `Runner::spawn`. When `screen_hint` cannot tell, set
`RunnerConfig::ready_detector` to a detector for the harness.

The hook config of the harness must call `terrarium hook <HookName>`, with
the hook JSON on stdin. The adapter maps the names that it gets back.

## Lifetime

The background tasks hold the runner until the child exits. Drop the last
`Runner` handle and the harness keeps running. Call `Runner::kill` to stop it,
and `Runner::wait` to get the `ExitStatus`. PLAN §6 puts each runner in its
own process, so that the agent lives through a daemon restart. This crate is
the library. The process that hosts it is not in this crate.

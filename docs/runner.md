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
`TERM=xterm-256color` when the spec does not set `TERM`. When
`LaunchSpec::clear_env` is true, the child starts from an empty env: it
gets only these values and `LaunchSpec::env`. The daemon always sets it.

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
| `whoami` | `agent`, `name`, `project`, `harness`, `roles`, `status` |

The runner sends every other method to the `RequestHandler` that the daemon
gives to `Runner::spawn`. The handler gets a `RequestContext` with the agent
identity. The runner knows the caller by the socket: each agent has its own
socket. `NoHandler` answers `method_not_found` for every method.

Error codes:

| Code | Cause |
|---|---|
| `parse_error` | The line is not a valid message, or it is longer than 64 MiB. The `id` is `null`. |
| `invalid_request` | The client sent a `response`. |
| `method_not_found` | No one knows the method. |

## The status machine

| From | Cause | To |
|---|---|---|
| (spawn) | The runner starts the child. | `starting` |
| `starting` | The prompt is ready for the first time. | `idle` |
| any live status | A `TurnStart` event (the prompt-submit hook). | `working` |
| `idle` | A typed turn is in flight and the adapter's `screen_hint` says `Busy`. | `working` |
| any live status | A `TurnEnd` event (the stop hook). | `idle` |
| any live status | A `Notification` with `question: true`, or a `Question` event. | `blocked` |
| `working` | The adapter's `screen_hint` says `Question`. | `blocked` |
| `working` | No hook and no PTY output for `stall_timeout` (default 5 minutes). | `blocked` |
| `blocked` (stall) | New PTY output while the turn is open. | `working` |
| `blocked` | A `ToolUse` or `ToolFinished` event. | `working` |
| `blocked` | `Runner::answer` or `Runner::answer_keys`. | `working` when a turn is open, else `idle` |
| any | The child exits after `Runner::kill`. | `stopped` |
| any | The child exits with code 0 and no signal, and no one asked. | `stopped` (reason `exited`) |
| any | The child exits with another code or a signal, and no one asked. | `error` |

The daemon's supervisor applies the restart policy on top of this
(PLAN §18). It does not pass the runner's final `stopped` or `error` on.
It decides the final status itself.

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
3. It waits `submit_timeout` (default 10 s) for a sign that the harness
   took the turn. There are two signs. The first sign is the prompt-submit
   hook (a `TurnStart` event). The second sign is a `Busy` screen hint from
   the adapter: the text left the input line and a turn runs. The first sign
   that comes resolves the handle with `Ok(())`. A busy screen also opens the
   turn and sets the status to `working` (reason `prompt submitted (screen
   busy)`). A prompt-submit hook that comes after that changes nothing.
4. When neither sign comes, the runner first writes
   `Adapter::clear_input_keys` (default Ctrl-U), because the text of the
   failed attempt can still be in the input box. Then it types the turn
   again when the prompt is ready.
5. After `submit_attempts` tries (default 2), the handle resolves with
   `DeliveryError::NoSubmitHook`, and the runner raises
   `Event::DeliveryFailed`.

Why two signs: Codex and Copilot send the prompt-submit hook late. On the
first prompt of a session, Codex creates the session first, and the hook
comes 0.86 s after the Enter key (`docs/compat.md`). A retry on the hook
alone can type the text twice. An adapter with no screen hint (`Unknown`)
gets only the hook sign.

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
| `screen_hint` | Tells when the prompt is ready, when a question shows, and when a typed turn went in (`Busy`). |
| `type_turn` | The keystrokes for a turn. |
| `answer` | The keystrokes for `Runner::answer`. |
| `clear_input_keys` | The keystrokes that empty the input line before a retry. The runner sends them only when no prompt-submit hook and no busy screen came. |

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

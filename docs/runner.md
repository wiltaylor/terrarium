# The agent runner

The `terrarium-runner` crate holds one harness in a PTY. It keeps the parsed
screen, records the raw output and the events, serves a Unix socket for hooks
and the agent CLI, and runs the status machine of [PLAN §5](PLAN.md#5-domain-model).
One `Runner` holds one agent. A runner runs in the daemon process
(`Runner`), or in its own process, the `terrarium-runner` binary. A runner
process keeps the harness alive when the daemon stops or restarts
(PLAN §6, §18). The daemon talks to a runner process with `RemoteRunner`.
Both types implement the `RunnerHandle` trait.

## Files

The runner writes these files. `<state>` is `RunnerConfig::state_dir`, for
example `/state`. `<run>` is `RunnerConfig::runtime_dir`, for example
`/run/terrarium`.

The agent directory `<state>/agents/<ulid>/` has three parts
(`terrarium_runner::{private_dir, harness_dir, workspace_dir}`):

| Directory | Content | In the sandbox |
|---|---|---|
| `private/` | The records of the runner and the daemon: the files below, and the daemon's `session-prompt.sha256`. Mode `0700`. | Not bound. The harness cannot read or change it (`docs/security-review.md`, F1). |
| `harness/` | `prompt.md` and the per-agent files of the adapter (`claude/`, `codex/`, `copilot/`, `pi/`). | Read-only at the same path. The parts that the harness writes itself have read-write binds of their own: the adapter's `FileOverlay` entries with `writable: true`. |
| `workspace/` | The agent's files, in the layout before the work root. The daemon now keeps them in `<work>/<project>/<agent>` (`TERRARIUM_WORK_DIR`), and moves this dir there once at the next start (`crates/terrariumd/src/workspace.rs`). | Read-write at `$HOME/work` and at `/work/<project>/<agent>`. |

A daemon start moves an agent directory of the older flat layout (all files
at the top) into `private/` and `harness/`, once
(`terrarium_runner::migrate_legacy_layout`). An entry that is already at its
new place stays; the old copy then stays too, with a warning.

The runner follows no symlink in the agent directory (`docs/security-review.md`,
F11). `create_private_dir` uses `symlink_metadata`. It makes `private/` with
mode `0700`, and it sets the mode through a handle that it opens with
`O_NOFOLLOW`. It refuses with `RunnerError::UnsafePath` ("refuse <path>: it is
a symlink") when the agent directory or `private/` is a symlink or not a
directory. It also refuses when a record name in `private/` (`events.jsonl`,
`pty.log`, `pty.log.1`, `runner.json`, `runner-argv.json`, `runner-exit.json`
and its `.tmp`, `runner.log`, `session-prompt.sha256`) is a symlink or not a
regular file. Then the runner does not start and writes nothing. The runner
opens every record, and the pid file, with `O_NOFOLLOW`, and it refuses a file
that is not a regular file. It makes a new file with mode `0600`. The temp file
of `runner-exit.json` is made new (`O_EXCL`) after the removal of an old one.
`migrate_legacy_layout` moves nothing, and fails with `InvalidData`, when an
entry to move, `private/`, or `harness/` is a symlink. The runner does not
repair a planted entry: the daemon puts such a directory in quarantine before
the migration. A refusal makes the launch fail, and the error text names the
path.

| Path | Content |
|---|---|
| `<state>/agents/<ulid>/private/pty.log` | The raw PTY bytes. The runner appends to it. With `log_max_bytes = 0` the runner writes no `pty.log` at all (the login flow does this: its output has a device code). |
| `<state>/agents/<ulid>/private/pty.log.1` | The archive. When `pty.log` grows past `log_max_bytes` (default 8 MiB), it moves here and a new `pty.log` starts. The runner keeps one archive. |
| `<state>/agents/<ulid>/private/events.jsonl` | One `EventRecord` as JSON on each line. The runner appends to it, so the history stays across restarts. Each record has a `seq`: its line number, from 1 (see "Event seq"). The event is flattened into the record, so a tool call ID goes as `tool_id`, not `id`. The reader keeps a record with a bad `id`: it makes a new ULID and logs one warning. |
| `<run>/agents/<ulid>.sock` | The runner socket (hooks and agent RPC), mode `0600`. The runner removes a stale socket before it binds, and removes the socket after the child exits. |

A runner process also uses these files:

| Path | Content |
|---|---|
| `<run>/runners/<ulid>.sock` | The control socket for the daemon, mode `0600`. |
| `<run>/runners/<ulid>.pid` | The pid of the runner process. The live runner holds an exclusive `flock` on the file. The file stays after the exit, empty. |
| `<state>/agents/<ulid>/private/runner.json` | The spec file (mode `0600`). `RemoteRunner::spawn_process` writes it. Its `launch.env` can hold the injected API key. |
| `<state>/agents/<ulid>/private/runner-argv.json` | The sandbox argv, when the daemon gives one. |
| `<state>/agents/<ulid>/private/runner.log` | The stdout and stderr of the runner process. The process appends to it. |
| `<state>/agents/<ulid>/private/runner-exit.json` | The `ExitRecord` of the harness, as JSON: the `ExitStatus` fields (`code`, `signal`) and `stopped_by` (see "The exit record"). The runner writes it after each harness exit: a normal exit, a crash, a `kill`, and a SIGTERM. It writes a temp file and renames it, so a reader never sees half a file. It removes an old one at the start. |

### The exit record

`runner-exit.json` looks like this:

```json
{"code":0,"stopped_by":"container"}
{"code":129,"signal":"Hangup","stopped_by":"operator"}
{"code":7}
```

`stopped_by` says who stopped the harness on purpose. It is absent when the
harness ended by itself (an exit or a crash).

| `stopped_by` | Set by | `Supervisor::recover` after a daemon start |
|---|---|---|
| `container` | SIGTERM or SIGINT to the runner process: `docker/entrypoint.sh` stops the runners when the container stops. A signal wins over a value that the daemon set. | Launches the agent again with resume, whatever the exit code and the restart policy. |
| `operator` | The daemon, through `set_stopped_by` before a stop (`POST /api/agents/{id}/stop` by the operator, a removed agent). | Reports `stopped`; no launch. |
| `agent:<project>/<name>` | The daemon, through `set_stopped_by` before a stop that an agent asked for (`POST /api/agents/{id}/stop` from a project lead, for example). | The same as `operator`. The reason is `stopped by agent:<project>/<name>`. |
| `policy` | The daemon, through `set_stopped_by` before a restart that it kills for. | Applies the restart policy to the exit status. |
| absent | | Applies the restart policy to the exit status. |

`terrarium_runner::read_exit_record(state_dir, agent)` reads the whole
record; `read_exit_status` reads the status only. A runner that SIGKILL
ended writes no record; the next daemon then launches the agent again.

### Event seq

The `seq` of a record is its line number in `events.jsonl`, from 1. The
runner appends to the file, so the seq goes on across runner restarts and
daemon restarts. When the runner opens the file and the last line has no
newline (a torn write), it adds the newline. The torn line keeps its seq,
and readers skip it. `terrarium_runner::events::read_since(path, seq)`
reads the records after a seq. `Runner::last_seq` gives the seq of the last
line. A record from before this field has no `seq` in the JSON; the line
number is still its seq.

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

Each `terrarium hook` call is one connection, and a harness runs its hooks
one after the other. The accept loop reads the first line of each
connection itself and runs a hook at once, before it accepts the next
connection. So the hooks reach the status machine in the order that the
harness sent them: a `UserPromptSubmit` always comes before the
`Notification` after it. A client that sends nothing for 1 s gets its own
task, so it cannot stop the hooks for long.

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
| any live status | A system prompt: a prompt-submit hook for which `Adapter::is_system_prompt` is true. | `working` (reason `system prompt`) |
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

A system prompt is not a sign. Copilot sends a `UserPromptSubmit` with a
`<system_notification>` prompt when a background shell command ends
(`docs/compat.md`, "a background shell can start a second turn after
Stop"). For such a hook, `Adapter::is_system_prompt` is true. The runner
then does not resolve the handle, and it does not record a `TurnStart`.
It records `Event::Notification { question: false }` with the prompt text,
so the daemon still sees it. The turn is open and the status becomes
`working`, because the harness works on the prompt. A pending delivery
still waits for its own prompt-submit hook or a busy screen.

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
  `snapshot.formatted` to an empty terminal, then each output chunk. A
  receiver that falls `RunnerConfig::output_capacity` chunks behind (default
  1024) gets `RecvError::Lagged`.
- `Runner::attach_stream()` and `RunnerHandle::attach` give a
  `ScreenSnapshot` and an `AttachStream`. `AttachStream::recv` gives
  `AttachEvent::Output(chunk)`, or `AttachEvent::Resynced(snapshot)`. When
  the stream falls behind, it attaches again by itself: it gives
  `Resynced` with a new snapshot, and the output goes on from that
  snapshot. Output is missing before `Resynced`, so a UI clears the
  terminal and writes `snapshot.formatted`. Each lag gets one new attach.
  When that attach fails, the stream ends. The stream ends (`None`) after
  the harness exits and the last output is out.
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
| `is_system_prompt` | True for a prompt-submit hook of a prompt that the harness made itself. Such a hook does not confirm a delivery and is not a `TurnStart` (see "Typing a turn"). The default is false. Copilot implements it. |
| `exit_keys` | The keystrokes that make an idle harness exit, for `Runner::stop`. |
| `exit_key_pause` | The pause before the final CR of `exit_keys`. With a pause, `Runner::stop` and the daemon's stop type the keys without the CR, wait, then type the CR (`terrarium_core::adapter::exit_key_writes`). The default is zero (one write). Codex sets 300 ms. |

The adapter's `launch` gives the `LaunchSpec`. The supervisor calls `launch`
and gives the spec to `Runner::spawn`. When `screen_hint` cannot tell, set
`RunnerConfig::ready_detector` to a detector for the harness.

The hook config of the harness must call `terrarium hook <HookName>`, with
the hook JSON on stdin. The adapter maps the names that it gets back.

## Lifetime

The background tasks hold the runner until the child exits. Drop the last
`Runner` handle and the harness keeps running. Call `Runner::kill` to stop it,
and `Runner::wait` to get the `ExitStatus`.

- `Runner::kill` sends SIGHUP to the harness. When the harness has not
  exited after `RunnerConfig::kill_grace` (default 3 s), the runner sends
  SIGKILL to the process group of the harness.
- `Runner::stop` is the graceful stop. When the harness is `idle` and the
  adapter has `exit_keys`, the runner types them (the final CR after
  `exit_key_pause`) and waits
  `RunnerConfig::stop_grace` (default 5 s). When the harness is busy, the
  adapter has no exit keys, or the harness does not exit in time, the
  runner calls `kill`. No queued turn goes out after `stop` or `kill`.
  Both set the final status to `stopped`.
- `ExitStatus::signal` is the signal text of the PTY crate, for example
  `Hangup` or `Killed`. `ExitStatus::signal_number` gives the number.
  `ExitStatus::exit_code` gives the code as a shell does: the code, or 128
  plus the signal number. An in-process runner ends with
the daemon process. To keep the harness through a daemon restart, use a
runner process.

## The runner process

```
terrarium-runner --agent <ulid> --state-dir <dir> --runtime-dir <dir> \
                 --spec <file> [--sandbox-argv-file <file>] [--linger-ms 2000]
```

The process does these steps:

1. It calls `setsid`, so that a signal to the process group of the daemon
   does not reach it.
2. It locks `<run>/runners/<ulid>.pid` and writes its pid. When a live
   runner holds the lock, it writes an error to stderr and exits with
   code 3. It never takes over the sockets of a live runner.
3. It binds the control socket `<run>/runners/<ulid>.sock`.
4. It starts the harness with `Runner::spawn`. The hook socket is
   `<run>/agents/<ulid>.sock`, as for an in-process runner. The runner
   answers `ping` and `whoami` itself. It sends every other agent request
   to the daemon over the control socket (see "Agent RPC through the
   daemon").
5. It waits for the harness to exit. SIGTERM and SIGINT stop the harness
   with `Runner::stop`: the exit keys for an idle harness, else SIGHUP, and
   SIGKILL after `kill_grace_ms`. The status becomes `stopped`, and the exit
   record gets `"stopped_by": "container"`. SIGHUP does nothing.
6. It writes `runner-exit.json`. When a daemon is connected, it waits up to
   `--linger-ms` for the daemon to disconnect. Then it removes the control
   socket, empties the pid file, and exits.

The exit code of the process reflects the harness: it is
`ExitStatus::exit_code`. A harness that exits with code 7 gives 7. A
harness that SIGHUP ends gives 129, and SIGKILL gives 137. When the runner
itself fails before the harness starts (for example a bad spec or a launch
error), the code is 1 and there is no `runner-exit.json`. Code 3 means that
a live runner holds the agent. Read `runner-exit.json` to tell the cases
apart.

The process runs while no daemon is connected. The harness keeps working,
hooks keep coming in, and the events go to `events.jsonl` with their seq.

The log level comes from `TERRARIUM_LOG` (default `info`).

### The spec file

The spec file is JSON. Only `harness` and `launch` are necessary. Each
other field takes the default of `RunnerConfig::new`.

```json
{
  "harness": "claude",
  "adapter": "harness",
  "adapter_options": {"config_dir": "/home/agent/.claude", "host_config_dir": "/auth/claude"},
  "launch": {"program": "bwrap", "args": ["..."], "env": {}, "cwd": "/state/agents/01K.../workspace", "clear_env": true},
  "name": "alice", "project": "demo", "roles": ["leader"],
  "rows": 40, "cols": 120,
  "stall_timeout_ms": 300000, "submit_timeout_ms": 10000, "submit_attempts": 2,
  "log_max_bytes": 8388608, "tick_ms": 50,
  "ready_detector": {"quiet_ms": 300, "pattern": "> $"},
  "rpc_queue_timeout_ms": 10000,
  "kill_grace_ms": 3000, "stop_grace_ms": 5000
}
```

| Field | Meaning |
|---|---|
| `harness` | The harness ID. It selects the adapter, and `whoami` reports it. |
| `adapter` | `harness` (default): the adapter of `harness`. `fake`: the test adapter for the fake TUI (Claude Code hook names, no screen hints). Use `fake` in tests only. |
| `adapter_options` | The paths that the hook mapping reads: the harness home as the harness sees it (`config_dir`), and as the runner sees it (`host_config_dir`). |
| `launch` | The `LaunchSpec`: the final command. For a sandbox, it is the full `bwrap` command. The runner does not call `Adapter::launch`. |
| `ready_detector` | The settings of `DefaultReadyDetector`. Absent: the default detector. |
| `rpc_queue_timeout_ms` | How long an agent request waits for a daemon connection. |
| `kill_grace_ms` | The time from SIGHUP to SIGKILL (`RunnerConfig::kill_grace`). |
| `stop_grace_ms` | The wait after the exit keys on SIGTERM (`RunnerConfig::stop_grace`). |
| `output_capacity` | The PTY output channel capacity. Use it in tests only: a small value forces an attach lag. Absent: 1024. |

`--sandbox-argv-file` names a JSON array of strings. The first string
replaces `launch.program`, and the rest replace `launch.args`.

`RunnerSpec` is the Rust type of the file. `RunnerSpec::from_config` makes
one from a `RunnerConfig`. A `ready_detector` in a `RunnerConfig` is code,
so set `RunnerSpec::ready_detector` yourself.

The adapter selection is in one module:
`crates/terrarium-runner/src/bin/terrarium-runner/adapters.rs`. The runner
process uses the adapter for `hook_event`, `is_system_prompt`,
`screen_hint`, `type_turn`, `clear_input_keys`, `exit_keys`,
`exit_key_pause`, and `answer` only.

### The `bin` feature

The binary and its adapter selection are behind the cargo feature `bin`.
The feature is on by default, so `cargo build -p terrarium-runner` builds
the binary. The feature adds the four adapter crates, `clap`, `anyhow`,
`tracing-subscriber`, and `semver`. The library alone needs none of them.
A crate that uses only the library (the daemon) can turn the feature off:

```toml
terrarium-runner = { workspace = true, default-features = false }
```

For a workspace dependency, the root `Cargo.toml` must also set
`default-features = false` on `terrarium-runner`, because a member can only
add features. `cargo build -p terrarium-runner --no-default-features`
builds the library alone. The `process` integration test runs the binary,
so it also needs the feature.

## The control socket

The control socket carries newline-delimited JSON, as the runner socket
does. Each line is one frame. The `type` field names the kind of frame.

### Frames

The daemon sends two kinds of frame:

```json
{"type":"call","id":1,"method":"hello","params":{"client":"terrariumd","protocol":1}}
{"type":"rpc_response","id":7,"result":{"status":200,"headers":{},"body":{}}}
```

The runner sends five kinds of frame:

```json
{"type":"reply","id":1,"result":{}}
{"type":"reply","id":2,"error":{"code":"invalid_params","message":"..."}}
{"type":"stream","id":3,"item":{}}
{"type":"end","id":3}
{"type":"delivery","delivery":4,"ok":false,"error":{"kind":"no_submit_hook","attempts":2}}
{"type":"rpc_request","id":7,"method":"http","params":{},"context":{"agent":"01K...","name":"alice","project":"demo","roles":[],"harness":"claude","status":"idle"}}
```

- `id` in a `call` is a number that the daemon picks. Each `reply`,
  `stream`, and `end` frame carries the `id` of its call.
- A normal call gets one `reply`. A `reply` has `result` or `error`, never
  both.
- A stream call gets `stream` frames, then one `end` frame. An `end` with
  an `error` tells why the stream stopped early.
- The runner handles `write`, `resize`, `type_turn`, `answer`,
  `answer_keys`, and `kill` in the order they arrive, so keystrokes stay in
  order. Stream calls and `wait` run in parallel with the other calls.
- A line that does not parse gets a `reply` with `id` 0 and the code
  `parse_error`.

### One daemon at a time

The first call on a connection must be `hello`. Another call before
`hello` gets the error `hello_required`. `hello` makes the connection the
daemon connection. When a second connection sends `hello`, the runner
closes the first connection. Agent requests that wait for an answer on the
first connection get the error `unavailable`.

### Methods

| Method | Params | Answer |
|---|---|---|
| `hello` | `client`, `protocol` | `reply`: the `Hello` object (below). |
| `status` | none | `reply`: `{"status":"idle"}`. |
| `watch_status` | none | Stream. The first item is `{"status":...}` with the current status. Then one item for each change. The stream ends after the harness exits, with the final status as the last item. |
| `events` | `since` (optional seq) | Stream. Each item is an `EventRecord` with its `seq`. With `since`, the runner first sends the records after `since` from `events.jsonl`, then the new records. With no `since`, only new records. The stream has no gap and no doubles: the runner skips each record with a seq at or below the last one that it sent. When the stream falls behind, the runner reads the missed records from the file. The stream ends after the harness exits and the last record is sent. |
| `screen` | none | `reply`: a snapshot: `rows`, `cols`, `lines`, `cursor` (`[row, col]`), and `formatted` (base64 escape sequences that draw the screen). |
| `attach` | none | Stream. The first item is `{"snapshot":{...}}`, as for `screen`. Each next item is `{"output":"<base64>"}`, a chunk of raw PTY output. The snapshot and the output have no gap and no overlap. When the client falls `output_capacity` chunks behind (default 1024), the stream ends with the error `lagged`; attach again. `RemoteRunner::attach` does that by itself (see "Attaching"). The stream ends after the harness exits. |
| `write` | `data` (base64) | `reply`: `null`. The bytes go to the PTY as keystrokes. This is the attach input. |
| `resize` | `rows`, `cols` | `reply`: `null`. |
| `type_turn` | `text` | `reply`: `{"delivery":<id>}`. Later, one `delivery` frame with the same id gives the outcome (below). |
| `answer` | `request_id`, `answer` (`{"kind":"allow"}`, `{"kind":"deny"}`, `{"kind":"choice","value":2}`) | `reply`: `null`. As `Runner::answer`. |
| `answer_keys` | `keys` | `reply`: `null`. As `Runner::answer_keys`. |
| `kill` | none | `reply`: `null`. As `Runner::kill`. The status becomes `stopped` when the harness exits, and then the runner process exits. |
| `set_stopped_by` | `stopped_by` (`"container"`, `"operator"`, `"policy"`) | `reply`: `null`. The value goes into `runner-exit.json` at the harness exit (see "The exit record"). The daemon sends it before a `kill` or the exit keys. `RunnerHandle::set_stopped_by`; an in-process `Runner` writes no exit file and ignores it. |
| `wait` | none | `reply` when the harness exits: `{"code":0}` or `{"code":1,"signal":"Killed"}`. |
| `cancel` | `call` (the id of a stream call) | `reply`: `null`. The stream stops with an `end` frame with the error `cancelled`. |

The `Hello` object:

| Field | Meaning |
|---|---|
| `protocol` | The protocol version, now `1`. |
| `version` | The version of the runner binary. |
| `agent`, `harness` | The agent ULID and the harness ID. |
| `runner_pid` | The pid of the runner process. |
| `harness_pid` | The pid of the harness (for a sandbox: of `bwrap`). |
| `status` | The current status. |
| `session` | The harness session ID from the last `session_started` event, if any. |
| `seq` | The seq of the last line of `events.jsonl`. |
| `exit` | Set when the harness has exited and the runner is about to exit. |
| `hook_socket`, `events_path` | The paths of the hook socket and of `events.jsonl`. |

The `delivery` frame:

| `ok` | `error.kind` | Meaning |
|---|---|---|
| `true` | | The harness took the turn: the prompt-submit hook, or a busy screen. |
| `false` | `no_submit_hook` | No sign after `error.attempts` attempts. |
| `false` | `exited` | The harness exited before the turn went in. |
| `false` | `write` | The write to the PTY failed. `error.message` tells why. |
| `false` | `dropped` | The runner dropped the turn. |

A delivery outcome goes only to the connection that typed the turn. When
that connection closes first, the outcome is lost. A failed delivery is
also an `Event::DeliveryFailed` in the events.

Error codes of the control socket:

| Code | Cause |
|---|---|
| `hello_required` | A call came before `hello`. |
| `invalid_params` | The params do not match the method, or the base64 is bad. |
| `method_not_found` | The method is not in the table. |
| `parse_error` | The line is not a valid frame. |
| `exited` | The harness has exited (from `write`, `answer`, `answer_keys`). |
| `failed` | Another runner error. The message tells why. |
| `lagged` | The attach client fell behind. |
| `cancelled` | A `cancel` call stopped the stream. |

### Agent RPC through the daemon

The agent CLI sends requests on the hook socket. The runner answers `ping`
and `whoami` itself. For every other method (`http` and the others in
`docs/agent-rpc.md`), the runner sends an `rpc_request` frame to the daemon
connection. The frame has the method, the params, and the
`RequestContext` of the agent. The daemon runs its `RequestHandler` and
sends an `rpc_response` with the same `id`. The runner gives the result or
the error to the agent CLI.

When no daemon is connected, the request waits for a daemon up to
`rpc_queue_timeout_ms` (default 10 s). When a daemon connects in that time,
the request goes to it. Else the agent gets this tunnel error:

```json
{"type":"response","id":2,"error":{"code":"unavailable","message":"the agent RPC tunnel is down: no daemon connected to the runner within 10000 ms; try again later"}}
```

When the daemon disconnects before it answers, the agent also gets
`unavailable`. The runner does not send the request again, because the
daemon can have done it already.

## RemoteRunner and RunnerHandle

`RunnerHandle` is the surface that the daemon uses. `Runner` and
`RemoteRunner` implement it. The trait is object safe, so the daemon can
hold an `Arc<dyn RunnerHandle>`.

| Method | `Runner` | `RemoteRunner` |
|---|---|---|
| `agent`, `socket_path`, `events_path` | From the config. | From `hello`. |
| `status`, `watch_status` | The live status. | The last status from the `watch_status` stream. |
| `subscribe_events` | New events. | New events. The first call also gets the replay of `ConnectOptions::events_since`. |
| `screen`, `attach` | Local, no wait. | One call, or one stream. |
| `attach` lag | A new local attach and `Resynced`. | A new `attach` stream and `Resynced`, when the runner ends the stream with `lagged`, or when the local reader falls `ConnectOptions::attach_capacity` items behind (default 1024). |
| `write`, `resize`, `answer`, `answer_keys`, `kill` | Local. | One call each. |
| `type_turn` | A `Delivery` with the runner id. | A `Delivery` with a local id. The `delivery` frame resolves it. |
| `wait` | The exit status. | The `wait` reply. When the connection closes first: `runner-exit.json`, else code 1 with the signal `RUNNER_LOST`. |

`Runner` keeps its own methods with the same names (`Runner::kill` and
`Runner::resize` stay sync). `Runner` also has `harness_pid`, `session`,
`last_seq`, and `exit_status`.

`RemoteRunner` functions:

- `RemoteRunner::connect(runtime_dir, agent)` connects with no agent RPC
  handler. `RemoteRunner::connect_with(runtime_dir, agent, ConnectOptions)`
  sets the handler (the daemon passes its `AgentRpc`), `events_since`, the
  client name, and the connect timeout. Both send `hello` and open the
  `events`, `watch_status`, and `wait` calls.
- `RemoteRunner::spawn_process(bin, SpawnOptions)` refuses when a live
  runner holds the agent. Else it writes `runner.json`, starts `bin`
  detached (stdin from `/dev/null`, stdout and stderr to `runner.log`),
  waits up to `start_timeout` (default 10 s) for the control socket, and
  connects. A thread reaps the process when it exits while the daemon
  runs. When the process ends before it is ready, the error names
  `runner.log`.
- `hello()` gives the `Hello` from the connect. `last_seq()` gives the
  highest seq that the connection got: keep it for the next
  `events_since`. `is_connected()` is false after the connection closes.
- After the `wait` reply and the end of the `events` stream, the client
  closes the connection, so the runner process can exit at once.

Drop the last `RemoteRunner` clone and the connection closes. The runner
process and the harness keep running.

Helper functions: `control_socket_path`, `pid_path`, `runners_dir`,
`live_runner_pid(runtime_dir, agent)` (the pid when a runner holds the
lock), `read_exit_status(state_dir, agent)`, `read_exit_record(state_dir,
agent)`, `private_dir`, `harness_dir`, `workspace_dir`, and
`migrate_legacy_layout`.

## Reconnect after a daemon restart

PLAN §18 asks the daemon to reconnect to each runner and reconcile. For
each agent that the database shows as live:

1. Call `live_runner_pid(runtime_dir, agent)`. When it is `None`, the
   runner is gone. Read `read_exit_record(state_dir, agent)` and the end of
   `events.jsonl`. `stopped_by` decides first (see "The exit record"); else
   apply the restart policy.
2. Else call `RemoteRunner::connect_with` with `events_since` set to the
   last seq that the daemon stored for the agent. The first
   `subscribe_events` receiver gets every event that the daemon missed, in
   order, with no gap.
3. Take the status, the session ID, and the pids from `hello()`.

The daemon reconnects in two places, both in
`crates/terrariumd/src/runner_supervisor.rs`. At start, `reconcile` in
`crates/terrariumd/src/lib.rs` calls `Supervisor::recover` for each agent
that the store shows as live but the supervisor does not run. `recover`
does the three steps above with the stored `runner_seq` as
`events_since`. It returns `Attached` (the harness keeps running), `Ended`
(the harness ended while the daemon was down, and the restart policy does
not start it again), or `Gone` (the daemon launches the agent again with
`--resume`). While the daemon runs, the `forward` task of each agent
passes the runner events on. When the control connection drops (`wait`
gives the signal `RUNNER_LOST`) and `live_runner_pid` still finds the
runner, `forward` connects again with `events_since` set to the last seq
that it passed on, and adopts the new connection. Only when that connect
fails does the supervisor treat the agent as exited. In `inprocess` mode,
`recover` always returns `Gone`.

## Tests

The integration tests in `crates/terrarium-runner/tests` drive the fake TUI
(`tests/fake_tui/fake_tui.py`). Each test waits for a condition, never for
a fixed time, and each wait has a limit of 30 s. Set
`TERRARIUM_TEST_SLOW=1` on a slow machine (CI) to multiply each limit and
each timing setting by 4, or `TERRARIUM_TEST_SLOW=<n>` to multiply by `n`.
The fake TUI has commands that wait for the test (`/hold`, the
`FAKE_TUI_SUBMIT_GATE` file, `FAKE_TUI_ANSWER_HOLD`), so that a test does not
race a timer in the fake TUI. With `FAKE_TUI_SLASH_NEEDS_PAUSE=1`, the fake
TUI submits a slash command only when its CR comes at least 100 ms after
the text, as Codex does with `/exit`.

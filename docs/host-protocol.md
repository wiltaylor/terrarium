# The host agent protocol

This document is the wire contract between the daemon (`terrariumd`) and the
host agent (`terrarium-host`). It closes [PLAN §22](PLAN.md#22-open-items)
item 4 (framing, reconnection, and the output buffer). The design is in
[PLAN §19](PLAN.md#19-targets-host-agents-and-executors).

The Rust types and the codec are in `crates/terrarium-host/src/protocol.rs`.
The daemon uses the same types: `terrarium-host` is a library and a binary.
When this document and the code do not agree, fix the one that is wrong and
the tests in `crates/terrarium-host/tests/stub_daemon.rs`.

## Summary

- The host agent dials **out** to `ws(s)://<daemon>:8082/hosts/connect`. It
  sends `Authorization: Bearer <per-host token>`.
- One WebSocket carries all the sessions of one host agent.
- A **text** frame holds one JSON control frame. The `type` field names it.
- A **binary** frame holds stream bytes. A 5-byte header comes first: a
  4-byte big-endian session ID and a 1-byte stream tag.
- The first frame is `hello` from the host agent. The daemon answers with
  `welcome`.
- The host agent keeps the output of each session until the daemon sends
  `ack`. After a reconnect, it replays the bytes that the daemon does not
  have.

## Connection

| Item | Value |
|---|---|
| URL | `ws://<daemon>:8082/hosts/connect`, or `wss://` with TLS |
| Port | 8082 (`TERRARIUM_HOST_PORT` in `compose.yaml`) |
| Auth | `Authorization: Bearer <token>` on the upgrade request |
| Subprotocol | none. The protocol version is in `hello`. |
| TLS | rustls with the ring provider. The system roots, or `ca_file` in the config. No OpenSSL. |
| Message size | A receiver accepts text frames up to 1 MiB and data payloads up to 1 MiB. |

The daemon refuses a bad token with HTTP `401` on the upgrade. The host agent
then retries with backoff (see [Reconnect](#reconnect)). It does not stop,
because the operator can fix the token on the daemon side.

The config accepts `http://` and `https://` too. The host agent changes them
to `ws://` and `wss://`. When the URL has no path, the host agent adds
`/hosts/connect`.

## Framing

### Control frames (text)

Each text frame is one JSON object. The `type` field is the kind. A frame
about a session has a `session` field. Field names are `snake_case`. A
receiver ignores fields it does not know. Verb names for Docker and vmlab
use a dot: `docker.run`, `vmlab.status`.

### Data frames (binary)

```
offset  size  field
0       4     session ID, unsigned, big-endian (0 is reserved)
4       1     stream tag
5       n     payload (0 to 1 MiB)
```

| Tag | Name | Direction | Use |
|---|---|---|---|
| 0 | `stdin` | daemon to host | Bytes for the process's stdin (or the PTY) |
| 1 | `stdout` | host to daemon | Process stdout. A PTY session has this stream only. |
| 2 | `stderr` | host to daemon | Process stderr |
| 3 | `file` | both | File content of a `cp` session |

The host agent sends payloads of 32 KiB or less. A frame with session `0`, an
unknown tag, or a payload over 1 MiB is an error. The receiver drops it and
logs it.

## Handshake

The host agent sends `hello` as its first frame:

```json
{
  "type": "hello",
  "protocol": 1,
  "host": "lab1",
  "version": "0.1.0",
  "os": "linux",
  "arch": "x86_64",
  "capabilities": {
    "docker": {"present": true, "version": "29.8.0"},
    "vmlab": {"present": true, "version": "0.30.0-alpha"},
    "shell": "/bin/bash"
  },
  "max_sessions": 32,
  "resume": {
    "token": "r-01JB...",
    "sessions": [{"session": 7, "sent": 18234, "finished": false}]
  }
}
```

- `host` is the name that target definitions use (`host = "lab1"`).
- `capabilities` is `terrarium_core::protocol::HostCapabilities`. Docker is
  `present` when `docker version --format json` gets an answer from the
  Docker server. vmlab is `present` when `vmlab --version` succeeds.
  `shell` is `$SHELL`, else `/bin/sh`.
- `resume` is absent on the first connection after the host agent starts.
  On a reconnect it holds the token from the last `welcome` and every
  session that the host agent still holds. `sent` is the number of output
  bytes the session made so far. `finished` is true when the session has
  ended and its terminal frames wait for the daemon.

The daemon checks the token, the protocol version, and the name. It then
sends `welcome`:

```json
{
  "type": "welcome",
  "host_id": "01JB...",
  "heartbeat_secs": 15,
  "resume_token": "r-01JC...",
  "resume": [{"session": 7, "received": 17000}]
}
```

- `host_id` is the daemon's ID for this host in its host registry.
- `resume_token` is new on each connection. The host agent keeps only the
  last one.
- `resume` lists the sessions that the daemon takes back. `received` is the
  number of output bytes of the session that the daemon has.

The host agent waits 15 seconds for `welcome`. If it gets no `welcome`, it
closes the connection and retries.

Either side can send `goodbye` and then close:

```json
{"type": "goodbye", "reason": "token_revoked", "message": "revoked from the dashboard"}
```

Known reasons: `token_revoked`, `replaced` (a new connection with the same
token took over), `protocol` (a version the daemon does not support),
`shutdown`.

## Heartbeat

Both sides send `{"type":"ping","nonce":N}` every `heartbeat_secs`. The other
side answers `{"type":"pong","nonce":N}`. Any frame counts as a sign of
life. When a side receives nothing for 3 × `heartbeat_secs`, it closes the
connection. The host agent then reconnects.

## Sessions

The daemon starts each session and picks its ID. An ID is a `u32` other
than 0. It must be unique among the sessions that the host agent holds. The
daemon must not reuse an ID until it has sent `release` for it. A counter
for each host is enough.

The life of a session:

1. The daemon sends a start frame: `exec`, `cp_push`, `cp_pull`, a
   `docker.*` verb, or a `vmlab.*` verb.
2. The host agent sends `started` `{session, pid}` when the process (or the
   transfer) starts. `started` is not replayed after a reconnect.
3. Data frames flow in both directions.
4. A verb with structured output sends `result` `{session, value}`.
5. The session ends with one **terminal** frame: `exit`, `error`, or
   `cp_done`.
6. The daemon sends `release` `{session}`. The host agent then forgets the
   session. A `release` for a live session abandons it: the host agent kills
   the process group.

A session that fails before it starts (bad request, path denied, busy,
spawn failure) gets one `error` frame and no `started`. The host agent holds
no state for it. The daemon does not send `release` for it. If the
connection drops before that `error` arrives, the session is missing from
the next `hello`. The daemon reports every session it started that is
missing from `hello` as lost.

### `exec`

```json
{
  "type": "exec",
  "session": 7,
  "cmd": "cargo",
  "args": ["test", "--workspace"],
  "cwd": "/srv/build/terrarium",
  "env": {"RUST_LOG": "info"},
  "pty": {"rows": 40, "cols": 120},
  "timeout_secs": 600
}
```

| Field | Meaning |
|---|---|
| `cmd` | The program. The host agent runs it directly, not through a shell. Send `sh -c` for a shell line. |
| `args` | Arguments. Default `[]`. |
| `cwd` | An absolute path. Default: the host agent's working directory. |
| `env` | Added to the host agent's environment. |
| `pty` | `{rows, cols}` runs the program in a PTY. `null` runs it with pipes. |
| `timeout_secs` | Kill the process group after this many seconds. `0` or absent: no limit. |

The host agent starts each session in its own process group. A PTY session
is a session leader. Signals and kills go to the whole group, so background
children end too. In a PTY the host agent sets `TERM=xterm-256color` when
`env` does not set `TERM`.

Frames for a running session:

| Frame | Meaning |
|---|---|
| data, tag `stdin` | Bytes for stdin. |
| `{"type":"eof","session":7}` | Close stdin. In a PTY the host agent writes `^D`. |
| `{"type":"resize","session":7,"rows":50,"cols":200}` | Resize the PTY. No effect with pipes. |
| `{"type":"signal","session":7,"signal":"INT"}` | Send a signal to the process group. Names: `INT`, `TERM`, `KILL`, `HUP`, `QUIT`, `USR1`, `USR2`, `WINCH`, `CONT`, `STOP`. The `SIG` prefix is optional. |

The terminal frame is `exit`:

```json
{"type": "exit", "session": 7, "code": 3, "reason": "exited"}
{"type": "exit", "session": 7, "signal": 9, "reason": "timeout"}
```

`code` is set when the process exited. `signal` is set when a signal ended
it. `reason` is `terrarium_core::protocol::ExitReason`:

| `reason` | Meaning |
|---|---|
| `exited` | The process exited by itself. |
| `signaled` | A signal ended the process, for example from a `signal` frame. |
| `timeout` | The session ran past `timeout_secs`. The host agent killed it. |
| `output_overflow` | The link was down and the output buffer filled. The host agent killed the process. See [Flow control](#flow-control-and-replay). |
| `detached_timeout` | The link stayed down for longer than the resume grace (default 600 s). The host agent killed the process. |

After the process exits, the host agent waits up to 2 seconds for the output
pipes to close. A background grandchild that keeps a pipe open does not hold
the session open.

`terrarium shell <target>` on a `host` target is an `exec` of
`capabilities.shell` with `args: ["-l"]` and a `pty`.

### `cp`

One session copies one regular file. To copy a directory, the daemon packs
it with `tar` and runs `tar -x` through an `exec` with stdin, or it sends
one `cp` session for each file.

**Push** (daemon to host):

```json
{"type": "cp_push", "session": 10, "path": "/srv/build/in.tar", "mode": 420, "mkdirs": true}
```

Then data frames with tag `file`, then:

```json
{"type": "cp_end", "session": 10, "bytes": 200000, "sha256": "9f86d08..."}
```

The host agent writes to `.<name>.terrarium-<session>.part` in the
destination directory. It counts and hashes the bytes. When both match
`cp_end`, it sets `mode` (default: the umask), syncs the file, renames it into
place, and sends:

```json
{"type": "cp_done", "session": 10, "bytes": 200000, "sha256": "9f86d08..."}
```

On a mismatch it removes the partial file and sends `error` with code
`checksum_mismatch`. The old file at `path` does not change. `mkdirs`
(default true) makes missing parent directories.

A push does not survive a reconnect. When the link drops during a push, the
host agent removes the partial file and ends the session with `error` code
`interrupted`. The daemon sends the file again in a new session.

**Pull** (host to daemon):

```json
{"type": "cp_pull", "session": 11, "path": "/srv/build/out/report.html"}
```

The host agent sends the file as data frames with tag `file`, then
`cp_done` with the byte count and the SHA-256. The daemon checks both. A pull
goes through the replay buffer, so it survives a reconnect like `exec`
output.

### `docker.*`

Each Docker verb runs one `docker` CLI process as a session. Its output
streams like `exec`. The table shows the command line. Environment values
never go on the command line: the host agent passes `-e KEY` and sets `KEY`
in the environment of the `docker` process.

| Frame | Fields | Command |
|---|---|---|
| `docker.run` | `image`, `name`, `detach` (default true), `rm`, `env`, `volumes` [`{source, target, read_only}`], `workdir`, `network`, `user`, `labels`, `command`, `pty`, `timeout_secs` | `docker run -d [--rm] [--name N] [-e K]... [-v S:T[:ro]]... [-w W] [--network N] [-u U] [--label K=V]... IMAGE [COMMAND...]`. With `detach: false`: `-i`, and `-t` with a `pty`. |
| `docker.exec` | `container`, `cmd`, `args`, `cwd`, `env`, `user`, `pty`, `timeout_secs` | `docker exec -i [-t] [-w CWD] [-e K]... [-u U] CONTAINER CMD ARGS...` |
| `docker.stop` | `container`, `grace_secs` | `docker stop [-t G] CONTAINER` |
| `docker.rm` | `container`, `force`, `volumes` | `docker rm [-f] [-v] CONTAINER` |
| `docker.inspect` | `container` | `docker inspect --type container CONTAINER` |
| `docker.ps` | `all`, `labels` | `docker ps --no-trunc --format json [-a] [--filter label=K=V]...` |
| `docker.volume_create` | `name`, `labels` | `docker volume create [--label K=V]... NAME` |
| `docker.volume_rm` | `name`, `force` | `docker volume rm [-f] NAME` |

Every Docker frame also takes `timeout_secs`. A bind-mount `source` (a path
that starts with `/`) must pass the [path policy](#capability-and-policy).
A positional value (image, container, name, network, volume) must not be
empty and must not start with `-`.

`result` values:

| Verb | `value` |
|---|---|
| `docker.run` (detached) | `{"id": "<container id>"}` |
| `docker.inspect` | `{"state": "running", "id", "name", "image", "status", "running", "exit_code", "started_at", "finished_at"}`. A missing container gives `{"state": "absent"}`. |
| `docker.ps` | `[{"id", "names", "image", "state", "status", "labels": {..}}]` |

`state` is `terrarium_core::protocol::TargetState`: `running`, `stopped`,
`absent`, or `unknown`. Docker's `running`, `paused`, and `restarting` map to
`running`. `created`, `exited`, and `dead` map to `stopped`.

When Docker is not present, every `docker.*` frame gets `error` code
`unsupported`.

### `vmlab.*`

Each vmlab verb runs one `vmlab` CLI process as a session. vmlab finds a lab
by a walk up from the current directory to a file named `vmlab.wcl`. So the
host agent runs the CLI with `cwd` set to the lab directory, and it names
machines by their bare names.

The `lab` field is the lab directory, or the path of its `vmlab.wcl`. A path
to a `.wcl` file with another name is refused (`bad_request`), because vmlab
cannot find it. Note: the PLAN §19 example `lab = "/home/wil/labs/win11.wcl"`
must become `lab = "/home/wil/labs/win11"` (a directory that holds
`vmlab.wcl`).

| Frame | Fields | Command (in the lab directory) |
|---|---|---|
| `vmlab.up` | `lab`, `machines` | `vmlab up [MACHINE...]` |
| `vmlab.down` | `lab`, `machines`, `force` | `vmlab down [--force] [MACHINE...]` |
| `vmlab.destroy` | `lab` | `vmlab destroy` |
| `vmlab.status` | `lab`, `verbose` | `vmlab status [-v]` |
| `vmlab.exec` | `lab`, `machine`, `cmd`, `args`, `user`, `timeout_secs` | `vmlab exec [--timeout T] [--user U] MACHINE -- CMD ARGS...` |
| `vmlab.shell` | `lab`, `machine`, `user`, `pty` | `vmlab shell [--user U] MACHINE`, always in a PTY |
| `vmlab.cp` | `lab`, `machine`, `direction` (`push` or `pull`), `host_path`, `guest_path` | push: `vmlab cp HOST MACHINE:GUEST`; pull: `vmlab cp MACHINE:GUEST HOST` |
| `vmlab.snapshot_take` | `lab`, `name`, `machine` | `vmlab snapshot create [--vm MACHINE] NAME` |
| `vmlab.snapshot_restore` | `lab`, `name`, `machine`, `discard_guest_changes` | `vmlab snapshot restore [--vm MACHINE] [--discard-guest-changes] NAME` |
| `vmlab.snapshot_list` | `lab`, `machine` | `vmlab snapshot list MACHINE` |

Every vmlab frame also takes `timeout_secs`. For `vmlab.exec` the host agent
passes it to `--timeout` and kills the process 30 seconds after it, so vmlab
reports its own timeout first. With no `machine`, the snapshot verbs act on
every machine in the lab.

`vmlab cp` copies between a path on the host agent's machine and a guest.
To copy from the agent's workspace to a guest, the daemon first sends a
`cp_push` to a staging path on the host, then a `vmlab.cp` push. It reverses
the steps for a pull.

`vmlab status` and `vmlab snapshot list` have no JSON output. The host agent
reads their tables by the header column positions. `result` values:

| Verb | `value` |
|---|---|
| `vmlab.status` | `{"lab": "ad-lab", "running": true, "machines": [{"name", "kind", "status", "state", "ip", "template", "detail"?}], "segments": [..], "dev": [..], "downloads": [..]}` |
| `vmlab.snapshot_list` | `[{"name", "kind", "taken"}]` |

`state` maps vmlab labels to `TargetState`: `running`, `booting`,
`starting`, and `unhealthy` give `running`. `stopped` and `exited (N)` give
`stopped`. Other labels give `unknown`. `ip` is `null` for `-`. `detail` is
the `key=value` line that `-v` adds. A lab that is not running gives
`"running": false` and no machines.

vmlab exit codes pass through in `exit.code`: 0 success, 1 failed, 2 usage
error, 3 reboot still required, 4 not found, 5 conflict, 6 unsupported.

When vmlab is not present, every `vmlab.*` frame gets `error` code
`unsupported`.

## Flow control and replay

The host agent counts the output bytes of each session: the payloads of all
data frames it sends for that session (`stdout`, `stderr`, and `file`
together), in the order it sends them. Offsets are positions in this one
sequence.

The daemon sends `ack` when it has stored output:

```json
{"type": "ack", "session": 7, "bytes": 65536}
```

`bytes` is the total count of output bytes of the session that the daemon
holds. It never goes down. The host agent drops the acknowledged bytes from
its buffer. The daemon sends `ack` at least once for each 1 MiB it receives,
and when the session ends.

Each session has a replay buffer of `buffer_limit_bytes` (default 8 MiB):

- **Link up, buffer full.** The host agent stops reading the process's
  output until an `ack` makes room. The process blocks on its pipe. This is
  backpressure. Nothing is lost.
- **Link down, buffer has room.** The process runs on, and its output goes
  into the buffer.
- **Link down, buffer full.** The host agent kills the process group and
  ends the session with `reason: "output_overflow"`. The buffer keeps the
  first unacknowledged bytes, up to the limit.

The terminal frames (`result` and then `exit`, `error`, or `cp_done`) come
after the last data byte. The host agent keeps them until `release`, and it
replays them after a reconnect.

## Reconnect

When the connection drops, the host agent:

1. Keeps every session. Live processes keep running. Running containers and
   VMs are not touched; they are state on the host, not in the connection.
2. Ends each `cp_push` with `error` code `interrupted`.
3. Waits, then dials again. The wait is exponential backoff with jitter: it
   starts at 1 second, doubles on each failure up to 60 seconds, and each
   wait is uniform in `[base/2, base]`. A `welcome` resets it.
4. Sends `hello` with `resume.token` (the last `resume_token`) and the
   sessions it holds, each with `sent` and `finished`.

The daemon then:

1. Checks that `resume.token` is the last token it gave to this host. If
   not (for example, the daemon restarted), it sends `welcome` with an empty
   `resume` list.
2. Lists in `welcome.resume` each offered session it still wants, with
   `received`: the output bytes it has. `received` may be more than its last
   `ack`.
3. Reports each session it started that is missing from `hello` as lost.

The host agent then:

1. For each session in `welcome.resume`: drops the bytes before `received`,
   sends every byte after it, and then the terminal frames if the session
   has ended. Then the session is live on the new link.
2. Ends every session that is not in `welcome.resume`: it kills the
   process group and forgets the session.

A session that stays without a link for longer than `resume_grace_secs`
(default 600) is killed with `reason: "detached_timeout"`. The host agent
keeps its output and terminal frames for the next `hello`.

When the daemon restarts, it loses its exec sessions. The host agent kills
them at the next `welcome`. Targets (containers, VMs) are not affected.

## Error codes

The terminal `error` frame:

```json
{"type": "error", "session": 12, "code": "path_denied", "message": "/etc is outside allowed_cwd_roots"}
```

| `code` | Meaning |
|---|---|
| `bad_request` | The frame did not parse, or a field is not valid (for example a value that starts with `-`). |
| `busy` | `max_sessions` sessions are running. |
| `duplicate_session` | The session ID is in use. The running session does not change. |
| `unknown_session` | The frame names a session that the host agent does not hold. |
| `unsupported` | The verb needs a tool (Docker, vmlab) that is not present. |
| `path_denied` | A path is not absolute, has `..`, or is outside `allowed_cwd_roots`. |
| `spawn_failed` | The process did not start (for example, the program does not exist). |
| `io` | A file operation failed. |
| `checksum_mismatch` | A pushed file's byte count or SHA-256 does not match `cp_end`. |
| `interrupted` | The link dropped during a push. The partial file is removed. |

`unknown_session` can also come for a frame (`stdin`, `eof`, `cp_end`) after
the session ended. The daemon can ignore it in that case.

## Capability and policy

The host agent runs only what the daemon sends. It has no list of targets
and no agent policy. The daemon applies the agent's policy (PLAN §9: the
`targets` list) before it sends a frame. The daemon also checks that the
target is defined on this host and of the right kind.

The host agent has these local limits:

- `allowed_cwd_roots` in the config. When set, every `cwd`, `cp` path, lab
  directory, `vmlab.cp` host path, and Docker bind-mount source must be
  under one of the roots. The host agent resolves symbolic links in the
  existing part of the path, so a link cannot lead out of a root.
- `max_sessions` (default 32).
- Positional values for Docker and vmlab cannot start with `-`, so they
  cannot become flags.
- Commands run as the user of the host agent (the `User=` of the systemd
  unit). Do not run the host agent as root unless the targets need it.

Token handling (PLAN §21, "Host agent token leaks"):

- One token for each host. The daemon stores only a hash of it and can
  revoke it from the dashboard. On a revoke, the daemon sends `goodbye`
  `token_revoked` and closes the connection.
- The host agent reads the token from `token_file` (mode 0600). It never
  logs the token. `terrarium-host token-file PATH` writes a token from stdin
  with mode 0600 in a directory with mode 0700.
- A second connection with the same token replaces the first. The daemon
  sends `goodbye` `replaced` on the old one.

## Host agent configuration

`/etc/terrarium-host/config.toml`, or the file given with `--config`:

```toml
daemon_url = "wss://terrarium.lan:8082"     # ws, wss, http, or https; path defaults to /hosts/connect
token_file = "/etc/terrarium-host/token"    # or: token = "..."
name = "lab1"                               # default: the hostname
max_sessions = 32
allowed_cwd_roots = ["/home/wil/labs", "/srv/build"]   # optional; absent means any absolute path
# ca_file = "/etc/terrarium-host/ca.pem"    # trust this CA for wss:// in place of the system roots
# buffer_limit_bytes = 8388608              # replay buffer for each session
# resume_grace_secs = 600                   # how long a session lives with no link
# docker_bin = "docker"
# vmlab_bin = "vmlab"
```

The flags `--daemon-url`, `--token`, and `--name` (and the env variables
`TERRARIUM_DAEMON_URL`, `TERRARIUM_HOST_TOKEN`, `TERRARIUM_HOST_NAME`)
override the file. `TERRARIUM_HOST_CONFIG` sets the config path.

Commands:

| Command | Does |
|---|---|
| `terrarium-host run` | Connect and serve sessions. The default command. Stops on SIGTERM or Ctrl-C. |
| `terrarium-host check [--json]` | Print the name, the daemon URL, whether a token is set, and the detected capabilities. Makes no connection. |
| `terrarium-host token-file PATH` | Read a token from stdin and write it to `PATH` with mode 0600. |

Logs go to stderr through `tracing`. `RUST_LOG` sets the level (default
`info`).

## Install

The systemd unit is `crates/terrarium-host/terrarium-host.service`. It runs
`/usr/local/bin/terrarium-host --config /etc/terrarium-host/config.toml run`
as the user `terrarium`, reads optional overrides from
`/etc/terrarium-host/env`, adds `~/.local/bin` to `PATH` (for a per-user
vmlab), and restarts on failure. Edit `User=` and `SupplementaryGroups=` to
match the host.

The `just host::install` and `just host::token` recipes are stubs today.
They must do this:

**`just host::token <host>`**

1. Ask the daemon to mint a token for `<host>` (for example
   `terrarium hosts token <host>` against the API). The daemon makes the
   host registry entry if it does not exist and stores only a hash.
2. Print the token once on stdout, so it can go into a pipe. Print nothing
   else on stdout.

**`just host::install <host>`**

1. Build the static binary (`host::build`).
2. Copy `target/host/terrarium-host` to `<host>:/usr/local/bin/` and
   `terrarium-host.service` to `<host>:/etc/systemd/system/` over ssh.
3. If `/etc/terrarium-host/config.toml` does not exist on the host, write
   one with `daemon_url` (from `TERRARIUM_PUBLIC_URL`, port 8082),
   `token_file = "/etc/terrarium-host/token"`, and `name = "<host>"`.
4. If `/etc/terrarium-host/token` does not exist, run
   `just host::token <host> | ssh <host> sudo terrarium-host token-file /etc/terrarium-host/token`,
   then `chown` the file to the service user.
5. Make the service user if it does not exist, then run
   `systemctl daemon-reload` and `systemctl enable --now terrarium-host`.
6. Run `terrarium-host check` on the host and print the result.

## What the daemon side must do

This list is for the daemon work that comes next (the host registry, the
dial-in listener on :8082, and target routing):

- Serve `GET /hosts/connect` on the dial-in listener. Check the bearer token
  against the host registry before the upgrade. Answer `401` on a bad token.
- Read `hello`. Refuse an unknown `protocol` with `goodbye` `protocol`.
  Store the capabilities for the dashboard and for policy checks
  (`HostCapabilities::summary` gives the flat `Capabilities`).
- Send `welcome` with a new `resume_token` and the `resume` list, as in
  [Reconnect](#reconnect).
- Pick session IDs with a counter for each host. Never reuse a live ID.
- Send `ack` at least once for each 1 MiB of output, and at the end.
- Send `release` after each terminal frame.
- Send `ping` every `heartbeat_secs`, and close a link that is silent for 3
  intervals.
- Apply the agent's policy before it sends any frame.

## Decisions on PLAN §22 item 4

| Question | Decision |
|---|---|
| WebSocket framing | JSON text frames for control. Binary frames with a 5-byte header (`u32` BE session, `u8` tag) for stream bytes. No base64 of output in JSON. |
| Session IDs | `u32`, picked by the daemon, unique among live sessions. The daemon maps them to its `ExecId` ULIDs. |
| Who opens sessions | The daemon only. The host agent never starts work by itself. |
| Flow control | Cumulative byte `ack` for each session. Backpressure at the buffer limit while the link is up. |
| Buffer while disconnected | Up to `buffer_limit_bytes` (default 8 MiB) for each session. Overflow kills the session with `output_overflow`. A session with no link for `resume_grace_secs` (default 600 s) is killed with `detached_timeout`. |
| Reconnection | Exponential backoff with jitter, 1 s to 60 s. A resume token in `hello`. The daemon says which sessions it takes back and from which byte. |
| Push over a reconnect | Not resumed. The session ends with `interrupted` and the daemon sends again. Pulls and exec output resume. |
| Liveness | JSON `ping`/`pong` both ways every `heartbeat_secs`. Dead after 3 silent intervals. |
| TLS | rustls with ring, system roots or `ca_file`. Static musl builds need no OpenSSL. |

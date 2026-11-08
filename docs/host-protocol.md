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

- The host agent dials **out** to `wss://<daemon>:8082/hosts/connect`. It
  sends `Authorization: Bearer <per-host token>`. Plaintext `ws://` is for
  loopback only, unless the config sets `allow_insecure` (see [TLS](#tls)).
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
| URL | `wss://<daemon>:8082/hosts/connect`. `ws://` only to loopback, or with `allow_insecure`. |
| Port | 8082 (`TERRARIUM_HOST_PORT` in `compose.yaml`) |
| Auth | `Authorization: Bearer <token>` on the upgrade request |
| Subprotocol | none. The protocol version is in `hello`. |
| TLS | rustls with the ring provider, on both sides. The system roots, `ca_file`, or a pinned SHA-256 (`server_fingerprint_sha256`). No OpenSSL. See [TLS](#tls). |
| Message size | A receiver accepts text frames up to 1 MiB and data payloads up to 1 MiB. |

The daemon refuses a bad token with HTTP `401` on the upgrade. The host agent
then retries with backoff (see [Reconnect](#reconnect)). It does not stop,
because the operator can fix the token on the daemon side.

The config accepts `http://` and `https://` too. The host agent changes them
to `ws://` and `wss://`. When the URL has no path, the host agent adds
`/hosts/connect`.

## TLS

The bearer token and all exec traffic (command lines, output, and file
bytes) go over this link. Over a network that you do not trust, the link
must use TLS. Else an attacker on the path can read the token and replay it
(security review F2).

### The daemon

| Variable | Default | Meaning |
|---|---|---|
| `TERRARIUM_HOST_TLS_CERT` | unset | A PEM file with the certificate chain, leaf first. |
| `TERRARIUM_HOST_TLS_KEY` | unset | A PEM file with the private key (PKCS#8, PKCS#1, or SEC1). |
| `TERRARIUM_HOST_LISTEN` | `auto` | The listen address. `auto` or unset: `0.0.0.0:8082` with TLS, `127.0.0.1:8082` without. Empty: no listener of its own. |

- Set both TLS variables, or neither. One alone stops the daemon at start.
  An empty value counts as unset.
- With TLS, the daemon serves the listener with rustls (`TlsListener` in
  `crates/terrariumd/src/hosts.rs`). Each handshake runs in its own task
  and must end in 15 seconds. The start log shows the SHA-256 of the leaf
  certificate.
- Without TLS, the listener is plaintext and binds `127.0.0.1:8082`. A
  plaintext bind to a non-loopback address logs a warning that starts with
  `INSECURE`. Use this only behind a TLS reverse proxy, or for a host agent
  on the same machine.
- The dashboard port (`TERRARIUM_LISTEN`, port 8080) does not serve
  `/hosts/connect`: it answers `404`. The Unix socket serves it.

`just host::cert <name> [more names...]` runs `terrarium-host cert` and
writes a self-signed certificate (ECDSA P-256, made with rcgen) to
`config/tls/host-cert.pem` and `config/tls/host-key.pem` (mode 0600). The
names are the DNS names or IP addresses that host agents dial. The recipe
prints the line to put in the host agent config:

```
server_fingerprint_sha256 = "A1:2F:...:1A"
```

The recipe does not replace existing files. Add `--force` to replace them.
`terrarium-host fingerprint <cert.pem>` prints the SHA-256 of an existing
certificate. Compose mounts `./config/tls` at `/etc/terrarium/tls`, so set
`TERRARIUM_HOST_TLS_CERT=/etc/terrarium/tls/host-cert.pem` and
`TERRARIUM_HOST_TLS_KEY=/etc/terrarium/tls/host-key.pem` in `.env`.

### The host agent

The host agent picks one TLS mode from `daemon_url` and the config:

| Mode | When | Checks |
|---|---|---|
| `plain_loopback` | `ws://` to `localhost`, `*.localhost`, `127.0.0.0/8`, or `::1` | None. The bytes stay on the machine. |
| `plain_insecure` | `ws://` to any other host, with `allow_insecure = true` | None. The host agent logs a warning at start. |
| `system_roots` | `wss://` with no `ca_file` and no pin | The chain and the name, against the system roots. |
| `ca_file` | `wss://` with `ca_file` | The chain and the name, against the CA certificates in the file. |
| `pinned` | `wss://` with `server_fingerprint_sha256` | The SHA-256 of the leaf certificate must be the pin. The name and the expiry are not checked: the pin names one certificate. With `ca_file` too, the chain must also verify. |

- `ws://` to a host that is not loopback, without `allow_insecure`, is
  refused. `run` stops with an error that gives the reason, and the client
  library returns `ClientError::Insecure` before it dials. The check does
  not resolve names: a DNS name that points to `127.0.0.1` is not loopback.
- `ca_file` or `server_fingerprint_sha256` with a `ws://` URL is a config
  error, not a silent downgrade.
- The pin is 64 hex digits. Colons, spaces, case, and a `sha256:` prefix do
  not matter. A pin that does not match fails the handshake, so the token
  is never sent. The error names the fingerprint that the server showed.

`terrarium-host check` prints the TLS mode (`tls:`), or the reason why it
refuses the URL. `terrarium-host check --probe` also dials the daemon and
does the TCP connect and the TLS handshake. It sends no upgrade, no token,
and no `hello`. It prints the TLS version, the cipher suite, and the
server's SHA-256, and exits 1 when the probe fails. With `--json`, the
fields are `tls`, `tls_error`, and `probe`.

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
  "version": "0.1.0+6e31754",
  "os": "linux",
  "arch": "x86_64",
  "capabilities": {
    "docker": {"present": true, "version": "29.8.0"},
    "vmlab": {"present": true, "version": "0.30.0-alpha"},
    "shell": "/bin/bash",
    "display": {"enabled": false, "tesseract": true,
                "tools": ["xdotool", "import"], "drivers": ["vmlab", "docker"]},
    "metrics": true,
    "limits": {"systemd": true, "controllers": ["cpu", "cpuset", "io", "memory", "pids"],
               "docker": true}
  },
  "max_sessions": 32,
  "max_running_targets": 4,
  "resume": {
    "token": "r-01JB...",
    "sessions": [{"session": 7, "sent": 18234, "finished": false}]
  }
}
```

- `host` is the name that target definitions use (`host = "lab1"`).
- `version` is the build version of the host agent: `<crate version>+<git
  ref>` ([CONVENTIONS.md](CONVENTIONS.md), "Build version"). The daemon
  marks a host agent of another build `stale` in `GET /api/hosts`. It
  offers it its own build in `welcome.update` (see
  [Self-update](#self-update)), or logs a warning when it has no binary
  for the arch. An older host agent sends only `0.1.0`: it is stale too.
- `update_failed` is present only when a self-update did not start, and
  the host agent went back to its previous binary. It holds the version
  that failed. An older host agent does not send it.
- `capabilities` is `terrarium_core::protocol::HostCapabilities`. Docker is
  `present` when `docker version --format json` gets an answer from the
  Docker server. vmlab is `present` when `vmlab --version` succeeds.
  `shell` is `$SHELL`, else `/bin/sh`. `display` is
  `terrarium_core::protocol::DisplayCapability` (see [`display`](#display)):
  `enabled` is the opt-in of the config (`[display] enabled`, default
  false), `allowed_displays` comes from the config, `tesseract` is true when
  `tesseract` is on `PATH`, `tools` lists the desktop tools on `PATH`, and
  `drivers` lists the display backends this host agent can drive: `vmlab`
  and `docker` when the tool is there, `desktop` when the opt-in is on,
  and always `vnc` and `rdp` (the clients are in the host agent). An older
  host agent sends no `display`: the daemon reads it as off. `metrics` is
  true when the host agent can send [`metrics`](#metrics) frames (Linux,
  with `/proc`). An older host agent sends no `metrics`: false. `limits`
  is `terrarium_core::limits::LimitsCapability`: what the host agent can do
  for [resource limits](#resource-limits). An older host agent sends no
  `limits`, and the daemon sends it no session that needs limits.
- `max_running_targets` is the `max_running_targets` of the config. The
  daemon counts the docker and vmlab targets (and lab instances) on this
  host that it shows as `running`, and refuses an `up` past the limit. An
  older host agent sends none: no limit.
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
  "resume": [{"session": 7, "received": 17000}],
  "metrics": true
}
```

- `host_id` is the daemon's ID for this host in its host registry.
- `resume_token` is new on each connection. The host agent keeps only the
  last one.
- `resume` lists the sessions that the daemon takes back. `received` is the
  number of output bytes of the session that the daemon has.
- `token` is present only when the host agent dialled with an enrol token
  (see [Tokens](#tokens)). It is the new long-lived host token. The host
  agent writes it to `token_file` (mode 0600, through a temporary file and a
  rename) and uses it for every later dial.
- `metrics: true` asks for [`metrics`](#metrics) frames. An older daemon
  does not send it. Then the host agent sends no `metrics` frames, because
  the older daemon does not know the frame.
- `update` is present only for a host agent of another build (`stale`),
  when the daemon has a `terrarium-host` binary for its `arch`:

  ```json
  "update": {
    "version": "0.3.0+6e31754",
    "path": "/install/terrarium-host?arch=x86_64",
    "sha256": "9f2c...",
    "release_sha256": "9f2c..."
  }
  ```

  `version` is the daemon's build. `path` is on the host listener.
  `sha256` is the hash of the binary in the dist dir. `release_sha256` is
  present only when the dist dir has the release `SHA256SUMS`. See
  [Self-update](#self-update). An older host agent ignores the field.

The host agent waits 15 seconds for `welcome`. If it gets no `welcome`, it
closes the connection and retries.

Either side can send `goodbye` and then close:

```json
{"type": "goodbye", "reason": "token_revoked", "message": "revoked from the dashboard"}
```

Known reasons: `token_revoked` (also: an enrol token that was used already
or ran out), `replaced` (a new connection with the same token took over),
`protocol` (a version the daemon does not support), `shutdown`.

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
   `docker.*` verb, a `vmlab.*` verb, `display`, or `probe`.
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
| `limits` | `{target, holder, limits}`: run the process in the budget of `holder` on `target`. See [Resource limits](#resource-limits). Absent: no limits. |

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
| `docker.run` | `image`, `name`, `detach` (default true), `rm`, `init`, `env`, `volumes` [`{source, target, read_only}`], `workspaces` [`{path}`], `workdir`, `network`, `user`, `labels`, `command`, `pty`, `timeout_secs`, `limits` | `docker run -d [--rm] [--init] [--cpus=C] [--memory=M --memory-swap=M+S] [--pids-limit=P] [--blkio-weight=W] [--name N] [-e K]... [-v S:T[:ro]]... [-v <work_dir>/<path>:/work/<path>]... [-w W] [--network N] [-u U] [--label K=V]... IMAGE [COMMAND...]`. With `detach: false`: `-i`, and `-t` with a `pty`. `workspaces`: see [Workspaces](#workspaces). `limits`: see [Resource limits](#resource-limits). |
| `docker.exec` | `container`, `cmd`, `args`, `cwd`, `env`, `user`, `pty`, `timeout_secs` | `docker exec -i [-t] [-w CWD] [-e K]... [-u U] CONTAINER CMD ARGS...` |
| `docker.start` | `container` | `docker start CONTAINER`. The container keeps its files. |
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
cannot find it. Note: the PLAN §19 example `lab = "/home/alice/labs/win11.wcl"`
must become `lab = "/home/alice/labs/win11"` (a directory that holds
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
| `vmlab.snapshot_delete` | `lab`, `machine`, `name` | `vmlab snapshot delete MACHINE NAME` |

Every vmlab frame also takes `timeout_secs`. For `vmlab.exec` the host agent
passes it to `--timeout` and kills the process 30 seconds after it, so vmlab
reports its own timeout first. With no `machine`, `vmlab.snapshot_take` and
`vmlab.snapshot_restore` act on every machine in the lab. `vmlab snapshot
list` and `vmlab snapshot delete` take one machine only, so
`vmlab.snapshot_list` and `vmlab.snapshot_delete` need `machine`. For a
lab-wide list or delete, the daemon sends one frame for each machine that
`vmlab.status` names. The host agent checks `machine` and `name` as
positional values: not empty, no leading `-`, no control characters
(`bad_request`).

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

### `vmlab.prepare` and `vmlab.release`

These two verbs run in the host agent as a task, not as one process. The
session sends `started` (no `pid`), the steps as text on stderr (each
command line with `$`, then its output), and then `result` and `exit` 0. A
failure is one `error` frame. The daemon sends them for a vmlab target with
`lab_repo` or `lab_from_workspace` (docs/api.md §17.8). A target with `lab`
(a lab dir on the host) does not use them.

A lab instance is the directory `<instances_dir>/<instance>/`:

| Path | Holds |
|---|---|
| `src/` | The git worktree of a `repo` source, at the commit of `ref`. A `workspace` source has none. |
| `lab/vmlab.wcl` | The lab file that vmlab runs: the source definition, after the allowlist check, with the attached workspace as an `@dev` decorator. vmlab keeps its state in `lab/.vmlab/`. |

`vmlab.prepare`:

```json
{"type": "vmlab.prepare", "session": 21, "instance": "terra-vbox",
 "source": {"kind": "repo", "local": "labs.git", "ref": "main", "path": "alpine-box"},
 "machine": "box", "workspaces": [{"path": "terra/w1"}], "timeout_secs": 600}
```

| Field | Meaning |
|---|---|
| `instance` | 1 to 128 of `[A-Za-z0-9._-]`, not starting with `.` or `-`. The daemon sends the target name, or `<target>.<agent ULID>` for a `lab_from_workspace` target. |
| `source` | `{"kind": "repo", "local"?, "url"?, "ref", "path"}` or `{"kind": "workspace", "workspace": "<project>/<agent>", "path"}`. |
| `source.local` | A repo under `repos_dir`, for example `labs.git`. Its real path must stay under the real `repos_dir`. |
| `source.url` | A repo URL. The host agent keeps a mirror in `<instances_dir>/.mirrors/<sha256 of the URL>.git`: `git clone --mirror` the first time, `git remote update --prune` after. The credential comes in `env`, as for a checkout on a target (docs/api.md §17.5). |
| `source.ref` | A branch, a tag, or a commit: `[A-Za-z0-9._/+@-]`, not starting with `-`, no `..`. |
| `source.path` | The directory in the repo or the workspace that holds `vmlab.wcl`. `""` or `.` is the root. No `..`. |
| `machine` | The machine that gets the workspace. |
| `workspaces` | At most one: a vmlab machine syncs one workspace. |

The steps:

1. A `repo` source: `git --git-dir <repo> rev-parse --verify <ref>^{commit}`,
   then `git --git-dir <repo> worktree add --detach <instance>/src <sha>`.
   A worktree that is there stays as it is: the next `vmlab.prepare` does not
   move it to a new commit. A `workspace` source: the workspace itself (see
   [Workspaces](#workspaces)); nothing is written into it.
2. Read `<source>/<path>/vmlab.wcl`. It must be a regular file (not a
   symbolic link) of at most 1 MiB. The real path of `<path>` must stay in
   the source.
3. Check the file against the [lab allowlist](#the-lab-allowlist). A refusal
   is `error` code `refused`, with every problem in the message.
4. Write `lab/vmlab.wcl`: each `@dev(workspace)` of the file as its real
   absolute path, and the attached workspace as
   `@dev(workspace = "<work_dir>/<project>/<agent>", workspace_guest = "/work/<project>/<agent>")`
   on `machine`. When `machine` declares its own `@dev(workspace)`, that
   mount stays and the attach adds nothing (`dev.own` is true). The file is
   written only when its text changed: vmlab refuses a changed lab file
   while a machine runs.
5. `vmlab validate` in `lab/`. A failure is `error` code `refused`.

`result`:

```json
{"lab": "/home/alice/.local/share/terrarium-host/instances/terra-vbox/lab",
 "instance": "terra-vbox", "lab_name": "alpine-box",
 "source": ".../instances/terra-vbox/src/alpine-box", "attached": ["terra/w1"],
 "dev": {"machine": "box", "workspace": "/home/alice/dev/terrarium/work/terra/w1",
         "guest": "/work/terra/w1", "own": false},
 "changed": true, "head": "3f2c1e0b9a..."}
```

The daemon then sends the other `vmlab.*` verbs with `lab` set to the
`lab` of the result.

`vmlab.release` `{session, instance}`: when `src/` is a worktree, `git
worktree remove --force` and `git worktree prune` in its repo; then remove
the instance dir. A missing instance gives `{"released": false}`. The daemon
sends it after `vmlab destroy`.

### The lab allowlist

A `lab_repo` or `lab_from_workspace` lab is a file that an agent can write.
vmlab runs parts of a lab file on the host, and a lab file can name host
resources. So the host agent checks the tokens of the file against an
allowlist (`crates/terrarium-host/src/labcheck.rs`), before `vmlab
validate`. Everything that is not in the list is refused, and the message
names it and its line.

| Allowed | |
|---|---|
| The file | `import <vmlab.wcl>` first, then one `lab "<name>"` block. No other import, no `template` block. |
| `lab` | `agent_update`; blocks `segment`, `vm`, `container`, `record`, `sinkhole`. |
| `segment` | `subnet`, `dhcp`, `nat`, `mtu`, `routes_to`; blocks `dns`, `route`, `record`, `block`, `redirect`, `sinkhole`. |
| `vm` | `template`, `arch`, `profile`, `cpus`, `memory`, `disk`, `depends_on`, `nested`, `agent_update`, `display`, `cpu_model`, `accel`, `firmware`, `tpm`, `secure_boot`; blocks `nic`, `disk` (`size` only), `login`. |
| `container` | `image`, `mode`, `entrypoint`, `command`, `workdir`, `user`, `profile`, `cpus`, `memory`, `depends_on`; blocks `nic`, `env`, `login`, `healthcheck`, `volume` (`name`, `target`, `read_only`: a named volume only). |
| `nic`, `env`, `login`, `healthcheck`, `record`, `sinkhole`, `dns`, `route`, `block`, `redirect` | Their documented attributes. |
| `@dev` | On a `vm` or a `container` in `lab`: `default`, `workspace_guest`, and `workspace` as a relative path with no `..` that stays in the lab's source directory (symbolic links resolved). |
| Values | A string (no `${`, no escapes other than `\n`, `\t`, `\\`, `\"`), a number with an optional unit, `true` or `false`, a symbol (`:idle`), or a list of those. |

| Refused | Why |
|---|---|
| `provision`, `on` | A wscript runs on the host. wscript reads and writes host files (`copy_to`, `copy_from`, screenshots) and host environment variables (`vmlab::env`). |
| `playbook` | It reads a playbook folder on the host. |
| `share`, `volume { host }`, `media`, `disk { from }`, `cdrom`, `floppy` | Host paths. `vmlab validate` accepts an absolute or `..` path in `@dev(workspace)`, so the host agent checks paths itself. |
| `qemu_args` | Raw QEMU flags reach any host file or device. |
| `gpu` | A host device. |
| `forward`, `port` | A port on the host. |
| `connect`, `global` | Another host, or a segment that other labs share. |
| `gui` | A viewer on the host desktop. |
| `template` | A template build reads host files and URLs. |
| Any other block, attribute, decorator, or expression | Not in the allowlist. |

The only host paths in an instance are the attached workspace (under
`work_dir`) and the lab's own `@dev(workspace)` (in its source directory).
A lab that needs more (an ISO, a share, a provision script) is a
pre-provisioned lab: a target with `lab`, which the operator writes.

### `display`

PLAN §19b, docs/api.md §17.10. One `display` frame is one operation on the
screen of one target. It runs in the host agent as a task (no `pid`).

```json
{"type": "display", "session": 31,
 "target": {"backend": "vmlab", "lab": "/home/alice/labs/win", "machine": "win11"},
 "request": {"op": "click", "x": 10, "y": 20, "button": "right"},
 "timeout_secs": 60}
```

`target` picks the driver (`backend`):

| `backend` | Fields | The screen |
|---|---|---|
| `vmlab` | `lab`, `machine` | `vmlab vm ...` in the lab directory, with the lab and path rules of the other `vmlab.*` verbs |
| `docker` | `container`, `protocol` (`x11`, `wayland`, `auto`), `display`? | the tools run with `docker exec [-i] -e DISPLAY=<d> <container> ...` (`WAYLAND_DISPLAY` for Wayland) |
| `desktop` | `protocol`, `display`? | the tools run on this machine, with `DISPLAY` and `XAUTHORITY`, or `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`, from `[display]`. Refused (`refused`) unless the config has `[display] enabled = true`, and the display is in `allowed_displays` when that list is set |
| `vnc` | `address`, `username`?, `password`? | the host agent's RFB client (`display/vnc.rs`). `username` is not used |
| `rdp` | `address`, `username`, `domain`?, `password` | the host agent's RDP client on IronRDP (`display/rdp.rs`). With no `username` or `password`: `bad_request` |

`request` is `terrarium_core::display::DisplayOp`, tagged by `op`: `info`,
`screenshot {region?, scale?}`, `move {x, y}`, `click {x?, y?, button,
double}`, `drag {x1, y1, x2, y2}`, `scroll {x?, y?, dx, dy}`, `keys
{chord}`, `type {text}`, `ocr {region?}`, `find {image_base64, threshold?,
region?}`, `record {action}`. The host agent checks every value again.

The answer:

- A screenshot sends its PNG as `stdout` data frames (32 KiB each), then
  `result` and `exit` 0. The PNG is at most 8 MiB.
- Every other operation sends `result` and `exit` 0. `result` is the answer
  of docs/api.md §17.10 without `target`.
- A failure is one `error` frame: `bad_request` (a bad value), `unsupported`
  (the backend cannot do it, or a tool is missing; the message names the
  package), `refused` (the desktop opt-in, or an RDP certificate that is
  not the pinned one), or `failed` (the tool ran and failed, or a VNC or
  RDP connection or login failed).

The tools of each backend:

| Operation | vmlab | X11 (`docker`, `desktop`) | Wayland, wlroots |
|---|---|---|---|
| `screenshot` | `vmlab vm screenshot <vm> <file>`, the file in `<lab>/.terrarium-stage/` | `sh -c` of a script: `import -window root png:-`, else `xwd -root \| convert xwd:- png:-`, else `scrot`; exit 127 names the packages | `grim -` |
| `move` | `vmlab vm mouse-move <vm> X Y` | `xdotool mousemove X Y` | `ydotool mousemove --absolute -x X -y Y` |
| `click` | `vmlab vm click [--button B] <vm> [X Y]`, two times for `double` | `xdotool [mousemove X Y] click [--repeat 2] N` | `ydotool click [--repeat 2] 0xC0` (`0xC1` right, `0xC2` middle) |
| `drag` | `vmlab vm drag <vm> X1 Y1 X2 Y2` | `xdotool mousemove X1 Y1 mousedown 1 sleep 0.1 mousemove X2 Y2 sleep 0.1 mouseup 1` | `ydotool` move, `click 0x40`, move, `click 0x80` |
| `scroll` | `unsupported` | `xdotool click --repeat N 4\|5\|6\|7` | `ydotool mousemove --wheel -x DX -y -DY` |
| `keys` | `vmlab vm sendkeys <vm> <chord>` | `xdotool key ctrl+l` | `wtype -M ctrl -k l -m ctrl` |
| `type` | one `sendkeys` for each character (below) | `xdotool type --delay 12 --file -`, the text on stdin | `wtype -`, the text on stdin |
| `ocr` | `vmlab vm ocr [--region X Y W H] <vm>` | `tesseract stdin stdout` on a screenshot, when `tesseract` is on the host agent's `PATH` | the same |
| `find` | `vmlab vm find-image [--threshold T] [--region ...] <vm> <file>`; exit 1 with "no match" is `found: false` | `unsupported` | `unsupported` |
| `record` | `vmlab record start\|stop <vm>` | `unsupported` | `unsupported` |
| `info` | a screenshot, for the size | the same | the same |

Limits:

- vmlab 0.34 has no text input on its CLI, and its chords take letters,
  digits, `f1` to `f12`, and named keys only. So `type` on vmlab sends one
  chord for each character (US layout: `shift-a` for `A`) and covers
  letters, digits, space, Tab, newline, and `!@#$%^&*()`. Other characters
  (`.`, `/`, `-`, ...) are `bad_request`. At most 500 characters at a time.
- `display = "auto"` runs `sh -c` of a probe first: `x11` when `xdotool` is
  there, else `wayland` when `grim` is there, else `unsupported`.
- `ydotool` needs `ydotoold` running and its socket (`YDOTOOL_SOCKET`).
  Absolute moves can be off with pointer acceleration on.
- GNOME and KDE on Wayland refuse the synthetic input of `wtype` and
  `ydotool`. Use a `vnc` or `rdp` target for them.
- Crop (`region`) and scale run in the host agent on the PNG, for every
  backend, so a driver gives the full screen only.

The driver code is `crates/terrarium-host/src/display/`: the
`DisplayDriver` trait and the engine in `mod.rs`, and one file for each
backend: `vmlab.rs`, `xtools.rs` (docker and desktop), `vnc.rs` and
`rdp.rs` (the clients), and `remote.rs` (the VNC and RDP drivers and their
connections).

#### VNC and RDP

The host agent speaks VNC and RDP itself; it runs no tool.

| Operation | VNC (RFB) | RDP (IronRDP) |
|---|---|---|
| Login | RFB 3.8 (3.7, 3.3); security type 2 (VNC authentication: DES of the 16-byte challenge with the first 8 bytes of the password) when there is a password, else type 1 (none). Other types (TLS, VeNCrypt) are `failed`, with the types in the message | X.224 negotiation, TLS (rustls on ring), then NLA (CredSSP, NTLM) when the server picks it, else the password in the Client Info PDU (TLS alone). No standard RDP security (RC4). The server's certificate pin (below) comes before any credential |
| `screenshot` | SetPixelFormat (32-bit true colour, BGRX) and SetEncodings (ZRLE, CopyRect, Raw, DesktopSize) once; then a FramebufferUpdateRequest for the full screen (not incremental), and the one update that answers it | A task reads the server's updates into a framebuffer all the time. The screenshot waits until the screen had no update for 250 ms (at most 1.5 s), then copies the framebuffer |
| `move`, `click`, `drag` | PointerEvent with the button mask: bit 0 left, 1 middle, 2 right | fast-path mouse events: move, then button down and up |
| `scroll` | a press and a release of bit 3 (up), 4 (down), 5 (left), or 6 (right) for each step | a wheel event of 120 units for each step (negative is down) |
| `keys` | KeyEvent: the modifiers down in order, the key down and up, the modifiers up in reverse. X keysyms (`Modifier::x_keysym_value`, `Key::x_keysym_value`) | the same order, with set-1 scancodes (US layout, the E0 flag for the extended keys). `pause` has no scancode: `unsupported` |
| `type` | a KeyEvent down and up for each character: Latin-1 is the code point, other Unicode is `0x01000000` + the code point, newline is Return, tab is Tab | a Unicode key event down and up for each character; newline and tab are scancodes |

**Connections.** The host agent keeps one connection for each target
(`remote::Remotes`). The key is the address and a SHA-256 of the user
name, the domain, and the password; it holds no secret. One operation at
a time runs on a connection: the driver holds its lock for the whole
operation. A connection with no operation for 60 s closes (a task checks
every 15 s). Before it uses a kept VNC connection, the driver reads the
messages that wait on it, so a server that closed it shows. An operation
that fails drops the connection; when the connection was a kept one, the
driver connects again and tries the operation once more. A login (TCP,
handshake, authentication) has at most 20 s, or the frame's
`timeout_secs` when that is less. After an RDP login, the driver waits for
the first screen updates (at most 4 s): xrdp drops the input that comes
before the session draws.

**The RDP certificate pin.** The TLS handshake accepts any certificate
and checks the handshake signatures. Then the host agent reads its pin
file, `[display] rdp_known_hosts` (default
`$XDG_DATA_HOME/terrarium-host/rdp-known-hosts`, else
`~/.local/share/terrarium-host/rdp-known-hosts`): one line
`<address> <SHA-256 of the certificate>` for each server, with the
address as the frame has it with its port. With no line for the address,
the host agent adds one (trust on first use) and logs it. With a line
that matches, it goes on. With a line that does not match, the frame ends
with `refused` and a message with both fingerprints, before any
credential goes out. To accept a new certificate, run `terrarium-host
rdp-forget <address>` (or delete the line), then connect again.
`terrarium-host rdp-forget` with no address lists the pins, and
`terrarium-host check` prints the file.

**Secrets.** The password is in the frame only. The host agent does not
log a frame's target, and no error text of the VNC or RDP client holds
the password.

### `probe`

`{"type": "probe", "session", "address", "timeout_secs"?}`: a TCP connect
from the host agent to `address` (`host:port`), for the `status` of a
`vnc` or `rdp` target. The `result` is `{"address", "reachable",
"elapsed_ms", "error"?}`, then `exit` 0. A bad address is `bad_request`.

## `metrics`

A host agent with the `metrics` capability sends one `metrics` frame every
10 seconds (`METRICS_INTERVAL_SECS`), after a `welcome` with
`"metrics": true`. The frame is about no session: it has no `session`, no
replay, and no `ack`. The code is `crates/terrarium-host/src/metrics.rs`.

```json
{"type": "metrics", "interval_secs": 10.0,
 "cpu": {"busy_pct": 12.4, "cores": 16, "load1": 1.32},
 "memory": {"total_bytes": 100923486208, "used_bytes": 24047804416, "used_pct": 23.8,
            "swap_total_bytes": 100923338752, "swap_used_bytes": 9918578688},
 "net": {"rx_bytes_per_sec": 18234.5, "tx_bytes_per_sec": 4021.0},
 "disk_io": {"read_bytes_per_sec": 0.0, "write_bytes_per_sec": 1294336.0},
 "filesystems": [{"mount": "/", "fstype": "btrfs", "device": "/dev/mapper/luks-97cd...",
                  "total_bytes": 1998694907904, "used_bytes": 1201450532864,
                  "avail_bytes": 795540078592, "used_pct": 60.2}]}
```

- The host agent takes the first counters when the link comes up. Each
  frame then gives the mean over the `interval_secs` since the last frame.
- `cpu.busy_pct` is the busy share of all cores together (0 to 100), from
  the `cpu` line of `/proc/stat`: idle is `idle + iowait`. `cores` counts
  the `cpuN` lines. `load1` is the first number of `/proc/loadavg`.
- `memory` is from `/proc/meminfo`: used is `MemTotal - MemAvailable`.
  Swap used is `SwapTotal - SwapFree`.
- `net` is the sum over the physical interfaces of `/proc/net/dev`. The
  host agent skips `lo` and the interfaces whose name starts with `veth`,
  `docker`, `br-`, `virbr`, `tap`, `tun`, `vnet`, `cni`, `flannel`,
  `cali`, `wg`, `tailscale`, `vxlan`, `podman`, or `lxc`. So the traffic of
  a container or a VPN counts one time, on the physical interface.
- `disk_io` is the sum over the whole disks in `/sys/block` (a partition
  is not there; without `/sys/block`, a name rule finds the partitions).
  The host agent skips the disks whose name starts with `loop`, `ram`,
  `zram`, `dm-`, `md`, `sr`, `fd`, or `nbd`: their IO also shows on the
  physical disks under them. A `/proc/diskstats` sector is 512 bytes.
- `filesystems` lists the real filesystems of `/proc/mounts`, with
  `statvfs`. A mount counts when its source is a block device (`/dev/...`,
  not `/dev/loop*`) or a ZFS dataset, its type is not `tmpfs`, `devtmpfs`,
  `ramfs`, `overlay`, `squashfs`, `iso9660`, `udf`, `proc`, `sysfs`,
  `cgroup`, `cgroup2`, `nsfs`, `autofs`, or `fuse.*`, and its mount point
  is not under `/proc`, `/sys`, `/dev`, `/run`, `/snap`, `/var/lib/docker`,
  `/var/lib/containers`, or `/var/lib/kubelet`. Network filesystems (NFS,
  SMB, rclone) have no `/dev` source, so they are out: a `statvfs` on a
  hung network mount can block. Each device shows one time, at its
  shortest mount point (a btrfs volume with many subvolumes is one
  entry). At most 32 entries. `used_pct` is `used / (used + avail)`, as
  `df` shows it: the blocks reserved for root do not count as free.
- A part that the host agent cannot read is absent. `filesystems` is then
  `[]`.
- The frames go on a queue of one. The writer sends them only when no
  other frame waits, so a sample never delays a session. When a sample
  finds the queue full, the host agent drops it. A slow `statvfs` holds
  back only the next sample.

## Resource limits

Reservation-capable hosts advertise `reservations: true` and `capacity`
with logical `cpu_ids` and `memory_bytes`. Missing capability means no
reservation enforcement, not unlimited fallback. Per-device IO budgets are
configured by the daemon operator rather than detected throughput.

`resource_apply` carries `session`, `target`, `holder`, optional `container`
and runtime `limits` (fixed limits, optional CPU list and per-device IO).
With a container it invokes `docker update`; per-device IO in a live update
is rejected. Docker creation accepts per-device limits, only for ephemeral
reservation containers in phase 1. Without a container, the host applies
systemd slice properties including AllowedCPUs and device bandwidth/IOPS.
An empty target and holder address the shared `terrarium.slice`. Exclusive
scopes use the sibling `terrariumreserved` hierarchy. Clearing affinity
restores inheritance. Device identifiers map to `/dev/block/<major>:<minor>`.

`resource_stop` carries `session`, `target` and `holder`. It stops only that
reservation slice through the user systemd manager. Both verbs use normal
session completion acknowledgements. The daemon keeps capacity charged
until cleanup and restoration succeed. Docker expiry uses registered
process-tree cleanup rather than stopping permanent shared containers.
No privileged helper or direct cgroup-file writes are used.

docs/api.md §17.2, "Resource limits". The code is
`crates/terrarium-host/src/limits.rs` (systemd) and `limit_flags` in
`crates/terrarium-host/src/docker.rs`. The shared types and checks are in
`crates/terrarium-core/src/limits.rs`.

`limits` on the wire are plain numbers (`terrarium_core::limits::Limits`):

```json
{"cpu_millis": 2000, "memory_bytes": 4294967296, "swap_bytes": 0, "pids": 512, "io_weight": 200}
```

Each field is optional. `cpu_millis` is CPU time in thousandths of one
core (a multiple of 10). `swap_bytes` absent with `memory_bytes` is no
swap.

**Versioning.** The protocol version stays `1`. The new fields are
optional, and a capability gates them, as for `metrics`. An older host
agent ignores a field that it does not know, so it would run the command
without limits. Thus the daemon sends `limits` only to a host agent whose
`hello` has `capabilities.limits` and can apply them. Else the daemon
refuses the session with `409 conflict`, `this host agent cannot apply
resource limits: <reason>`, and sends nothing.

**The capability.** At start, the host agent runs `systemd-run --user
--scope --quiet --collect -p MemoryMax=64M true`. When that works, it reads
the cgroup of the user manager (`systemctl --user show
--property=ControlGroup --value`) and the `cgroup.controllers` of that
cgroup. The result:

| Field | Meaning |
|---|---|
| `systemd` | The probe worked and the `cgroup.controllers` file was read (cgroup v2). |
| `controllers` | The controllers that the user manager can give to its units, sorted. A limit needs its controller: `cpu` for `cpu_millis`, `memory` for `memory_bytes` and `swap_bytes`, `pids` for `pids`, `io` for `io_weight`. |
| `reason` | Why `systemd` is false: for example `` `systemd-run` is not on PATH ``, or the first error line of `systemd-run`. |
| `docker` | Docker is present, so `docker.run` takes `limits`. |

`systemctl --user` and `systemd-run --user` find the user manager through
`XDG_RUNTIME_DIR`. A host agent in a system unit (`User=` in
`terrarium-host.service`) has no `XDG_RUNTIME_DIR`; the host agent then
sets `/run/user/<uid>` for these calls and for the limited commands, when
that directory exists. The user of the host agent needs a running user
manager: for a service user, `loginctl enable-linger <user>`. `terrarium-host
check` prints the capability, and the start log has the line `resource
limits: ...`.

**`exec` with `limits`.** The frame has `limits: {target, holder,
limits}`. `holder` is `<project>/<agent>`, or `operator`. The host agent:

1. Checks the capability again (`systemd`, and the controllers). A
   failure is `error` `unsupported` with the same sentence as the daemon.
2. Sets the limits on the slice of the holder:
   `systemctl --user set-property --runtime -- <slice> CPUQuota=<c>%
   MemoryMax=<m> MemoryHigh=<90 % of m, down to 4 KiB> MemorySwapMax=<s or 0>
   TasksMax=<p> IOWeight=<w>`. A limit that is not set gets its default
   (`CPUQuota=`, `MemoryMax=infinity`, `IOWeight=`, ...), so a limit that
   the operator removed does not stay. A failure is `error`
   `spawn_failed`: the command never runs without its limits. The frame
   loop waits for this call (about 20 ms), so the input frames of the
   session come after it.
3. Starts the command as `systemd-run --user --scope --quiet --collect
   --slice=<slice> --unit=<scope> -- <cmd> <args>...`. `systemd-run` moves
   itself into the new scope and then runs the command in its own process.
   So the PID, the process group, the environment, the working directory,
   and the PTY stay those of the session.

The names:

| Unit | Name |
|---|---|
| Slice | `terrarium-<target>-<project>-<agent>.slice`; the operator: `terrarium-<target>-_operator.slice`. Each part is escaped as `systemd-escape` does (a `-` becomes `\x2d`), so two targets or agents never share a slice. systemd nests it under `terrarium-<target>.slice` and `terrarium.slice`. A name over 255 bytes becomes `terrarium-x<32 hex of a SHA-256>.slice`. |
| Scope | `terrarium-exec-<host>-<session>-<8 hex nonce>.scope`, with the host agent's name escaped. |

All scopes of one holder on one target are in one slice, so they share one
budget. Each holder has its own slice, so one agent cannot starve
another. The limits stay on the slice until the user manager stops (a
`--runtime` drop-in under `$XDG_RUNTIME_DIR/systemd/user.control/`).

**The end of a session.** A kill (a timeout, a `release` of a live
session, an output overflow, a detached timeout) sends `SIGKILL` to the
process group, as for every session, and also runs `systemctl --user kill
--signal=SIGKILL -- <scope>`. So a process that left the process group
(for example with `setsid`) ends too. A session that exits by itself
leaves its background processes in its scope, as before; they count in
the budget of the holder.

A scope is in the user manager, not in the cgroup of the host agent. So
`KillMode=control-group` of `terrarium-host.service` does not end it when
the host agent stops. At start, the host agent lists the scopes
`terrarium-exec-<its name>-*` and kills them: their sessions ended with
the run before.

**Docker.** `docker.run` with `limits` adds `--cpus=<cores>`,
`--memory=<bytes>` with `--memory-swap=<memory + swap>` (no `swap`: no
swap), `--pids-limit=<n>`, and `--blkio-weight=<w>`. Docker takes a blkio
weight from 10 to 1000; the host agent maps the IO weight with the inverse
of the runc formula for cgroup v2 (`io = 1 + (b - 10) × 9999 / 990`). A
docker without IO weight support only warns and drops it. The limits are
on the container, so all holders of the target share them.

**Measured on systemd 261** (Arch, the user manager has `cpuset cpu io
memory pids`): a slice with `MemoryMax=100M` read back `MemoryMax=104857600`,
`MemoryHigh=94371840`, `memory.swap.max 0`, and `cpu.max 50000 100000` for
`CPUQuota=50%`. Two scopes in the slice asked for 70 MiB each. The second
one waited about 3 s, until the first one ended, and the slice peak was
96 MiB. When the first one held its memory, the OOM killer ended it after
about 28 s of slow progress; with no `MemoryHigh` it acted at once (0.3 s).

## Workspaces

PLAN §11, "mode 1". An agent's workspace is
`<TERRARIUM_WORK_DIR>/<project>/<agent>` in the daemon (`/work` in the
container). The daemon names it to the host agent as `<project>/<agent>`.
The host agent finds it under its own `work_dir`, which must be the host
path of the same directory. On the Docker host of the Terrarium container,
that is the source of the `./work` bind mount (`compose.yaml`). The host
agent checks that the workspace exists and that its real path is under the
real `work_dir`, so a symbolic link cannot lead out. A missing workspace is
`error` code `path_denied`, and the message says to check `work_dir`.

A target sees the workspace at `/work/<project>/<agent>`, the same path as
the sandbox, so a path in a compiler message is the same on both sides.

**A host agent with no workspaces** (TER-41). A remote host agent has no
copy of the daemon's work root, so its `work_dir` is not a directory there.
It then sends `"no_workspaces": true` in `capabilities` (an older host agent
sends none: false), and logs a warning at its start. The daemon sends no
`workspaces` to it: `up` of a docker target runs the container with no
`/work` mount, records no attached workspace, and the answer has a
`workspace_note` that says so. A lease or a start of another holder
attaches nothing and gives no `attach_error`. Commands on such a target see
no agent workspace: check a branch out on it (`terrarium repo checkout
--target`).

| Kind | How | Limits |
|---|---|---|
| `docker` | `docker.run` with `workspaces`: a bind mount for each. With no `user` in the frame, the container runs as the owner of the first workspace (`-u <uid>:<gid>`, `HOME=/tmp`), so the files that a build writes stay the agent's (the sandbox runs as the same user). A command that needs root in the image does not work then. | A running container cannot get a new bind mount. The daemon runs a new container (`docker rm`, then `docker run`) when the target is ephemeral and no exec session is live on it. Else the answer says "attach needs a restart of the target". A target with `container` (a container that Terrarium did not make) gets no workspace. |
| `vmlab` | `vmlab.prepare` with `workspaces`: an `@dev` decorator, so vmlab's workspace syncer keeps a copy in the guest in step with the host dir, both ways. | One workspace for each machine. A running lab cannot load a changed lab file, so the same restart rule applies. The guest path is a Linux path. A target with `lab` gets no workspace. See docs/compat.md, "vmlab workspace attach". |

There is no separate detach verb: Docker and vmlab cannot remove a mount
from a running container or lab. The daemon detaches a workspace by a new
container or lab instance without it (ephemeral, no live session), or by the
destroy when the last holder goes.

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

## Self-update

A host agent follows the daemon's build. When the daemon gets a new build
(for example a new release image), each host agent replaces itself at its
next connect. The code is `crates/terrarium-host/src/selfupdate.rs` (the
host agent) and `crates/terrariumd/src/api/install.rs` (the offer).

**The offer.** The daemon compares `hello.version` with its own build
(`terrarium_version::is_stale`). For a stale host agent, it puts
`update` in `welcome` (see [Handshake](#handshake)). The offer names the
binary of the dist dir (`TERRARIUM_HOST_DIST_DIR`; in the image
`/opt/terrarium/dist`, built from the same commit as the daemon). When
the daemon's own ref is unknown (`+unknown`), nothing is stale, so it
offers nothing. When the dist dir has the release `SHA256SUMS` and the
binary does not match it, the daemon offers nothing and logs a warning.

**The download.** The host listener serves
`GET /install/terrarium-host?arch=<arch>` and
`GET /install/terrarium-host.sha256?arch=<arch>`, as the dashboard port
does ([api.md §17.9](api.md#179-the-host-installer)). The host agent
fetches `path` from the scheme and the host of its `daemon_url` (`wss`
becomes `https`, `ws` becomes `http`), with the same TLS settings as the
link: the pin, `ca_file`, or the system roots. The offer cannot name
another host. A TLS proxy in front of the host listener must pass
`/install/terrarium-host` as well as `/hosts/connect`.

**The steps of the host agent.**

1. It checks that it may update. It does not update when:
   - `auto_update = false` is in its config;
   - it does not run under systemd (no `INVOCATION_ID`), because then
     nothing starts the new binary after the exit;
   - the offered version is in `<binary>.failed` (see below);
   - this process refused or failed that version already.

   It logs the reason as a warning.
2. It checks that it can write the directory of its binary. The binary of
   an install from before self-update is in `/usr/local/bin`, which is
   root's: run `install-host.sh --upgrade` one time (see [Install](#install)).
3. It downloads the binary (at most 256 MiB, in 5 minutes) and checks its
   SHA-256 against `sha256` and, when present, `release_sha256`. A
   mismatch stops the update and changes no file.
4. It writes the binary as `<binary>.new` (mode 0755) and runs
   `<binary>.new --version`. The new binary must exit 0 in 10 seconds and
   print the offered version (`-dirty` does not count). Else it removes
   `.new` and stops.
5. It waits until it holds no session (it looks every 10 seconds). An
   update does not kill the sessions of agents.
6. It keeps the running binary as `<binary>.prev` (a hard link), writes the
   new version to `<binary>.pending`, and renames `.new` over the binary.
   The rename is atomic: the binary is always the old one or the new one.
7. It exits with status 0. systemd (`Restart=always`) starts the new
   binary after `RestartSec=5`.
8. At its first `welcome`, the new binary removes `<binary>.pending`.
   `.prev` stays, for a roll back by hand.

When the update fails before step 6, the host agent logs an error, keeps
its binary, and keeps its link to the daemon.

**The roll back.** The unit has an `ExecStartPre=` guard (a `/bin/sh -c`
line, so it works when the new binary does not run at all). When
`<binary>.pending` exists, the guard adds a line to it at each start.
After three starts that did not reach a `welcome`, the guard does these
steps:

1. It moves `<binary>.prev` back over the binary.
2. It renames `.pending` to `<binary>.failed`.
3. It prints `restored the previous binary` to the journal.

The old binary then starts. It does not take the failed version again. It
sends that version in `hello.update_failed`. The daemon logs a warning
and records the event `host.update_failed` and the audit entry
`host.update_failed`, one time for each version while the daemon runs.
A failure that the guard cannot see (the new binary connects, then fails
later) does not roll back: put `.prev` back by hand.

To install a version by hand after a roll back, run
`install-host.sh --upgrade`: it removes `.pending` and `.failed`.

**The record.** When a host agent that was stale connects with the
daemon's build, the daemon writes the event `host.updated` (`from`,
`to`) and the audit entry `host.update`. A reinstall by hand to the
daemon's build records the same.

**The file ownership (security).** The service user owns
`/var/lib/terrarium-host/bin/` and the binary in it, so the host agent
can replace itself without root. This has a cost. Each command that the
daemon runs on the host runs as the service user, so a session can also
replace the binary. Before, it could not. But the binary runs only as that
same user, so a session gets no rights that it did not have. The risk is
root: when root runs that file, root runs code that the service user
wrote. The installers run it only as the service user (`runuser`,
`sudo -u`). `/usr/local/bin/terrarium-host` is a symlink to it, for the
command line. Do not run `terrarium-host` as root, and do not use the
service user `root`. The `ExecStartPre=` guard runs as the service user
too.

To turn self-update off on a host, add `auto_update = false` to its
config and restart it. An older host agent refuses that key (the config
refuses unknown keys), so add it after the upgrade.

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
| `refused` | `vmlab.prepare`: the lab definition uses a feature outside the [lab allowlist](#the-lab-allowlist), or `vmlab validate` refused it. `display`: a `desktop` frame while the config does not opt in, a display that is not in `allowed_displays`, or an RDP server whose certificate is not the pinned one. The daemon answers `403`. |
| `failed` | `display`: the tool ran and failed (for example, the VM does not run, or the X server does not answer). |

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
  `work_dir` and `instances_dir` are roots too.
- `max_running_targets` (default 4). `hello` sends it, and the daemon
  refuses an `up` past it with `429 rate_limited` (docs/api.md §17.3).
- `max_sessions` (default 32).
- [Resource limits](#resource-limits) on `host` and `docker` targets,
  when the daemon sends them.
- Positional values for Docker and vmlab cannot start with `-`, so they
  cannot become flags.
- Commands run as the user of the host agent (the `User=` of the systemd
  unit). Do not run the host agent as root unless the targets need it.

## Tokens

PLAN §21, "Host agent token leaks". The host token is a long-lived bearer
token by design: the host agent must reconnect with no operator. These
rules limit what a leaked token can do.

- One token for each host (`thk_` and 64 hex digits). The daemon stores
  only its SHA-256 and can revoke it from the dashboard. On a revoke, the
  daemon sends `goodbye` `token_revoked` and closes the connection.
- The host agent reads the token from `token_file` (mode 0600). It never
  logs the token. `terrarium-host token-file PATH` writes a token from stdin
  with mode 0600 in a directory with mode 0700.
- A second connection with the same token replaces the first. The daemon
  sends `goodbye` `replaced` on the old one.
- Each connect records `first_used_at` (the first connect with the current
  token), `last_used_at`, and `last_used_from` (the TCP peer as the daemon
  sees it; a reverse proxy shows as itself). `GET /api/hosts` shows them,
  and `host.connected` carries `from`. A `first_used_at` that you do not
  expect, or a strange `last_used_from`, is a sign of a leaked token.

**Rotation.** `POST /api/hosts/{name}/rotate` makes a new token and shows it
once. The old token stops working for new connections at once. A link that
is up now stays up, because the daemon checks the token only on the
upgrade. When that link ends, the host agent must dial with the new token.
So write the new token to `token_file` on the host, then restart the host
agent or wait for the next reconnect.

**Enrol.** An enrol token keeps the long-lived token out of the operator's
clipboard:

1. `POST /api/hosts` with `{"name": "lab1", "enrol": true}` (or
   `POST /api/hosts/{name}/rotate` with `{"enrol": true}`). The answer has
   `token` (`the_` and 64 hex digits), `token_kind: "enrol"`, and
   `token_expires_at`, 10 minutes from now. The long-lived token of the host
   is now a random value that nobody knows.
2. Put the enrol token in the host agent's `token_file`, and start the host
   agent.
3. The daemon accepts the enrol token on the upgrade, reads `hello`, and
   then swaps the enrol token for a new long-lived token in one database
   update. Only one dial can win the swap. The daemon writes the audit
   entry `host.enrol`.
4. `welcome.token` carries the new token. The host agent writes it over the
   enrol token in `token_file`, and uses it from then on.

An enrol token works one time, and only until `token_expires_at`. A dial
with a used or expired enrol token gets `401`. When the host agent cannot
write `token_file`, it logs an error and keeps the token in memory only:
the next start then fails, so rotate again.

## Host agent configuration

`/etc/terrarium-host/config.toml`, or the file given with `--config`:

```toml
daemon_url = "wss://terrarium.lan:8082"     # ws, wss, http, or https; path defaults to /hosts/connect
token_file = "/etc/terrarium-host/token"    # or: token = "..."
name = "lab1"                               # default: the hostname
max_sessions = 32
allowed_cwd_roots = ["/home/alice/labs", "/srv/build"]   # optional; absent means any absolute path
# ca_file = "/etc/terrarium-host/ca.pem"    # trust this CA for wss:// in place of the system roots
# server_fingerprint_sha256 = "A1:2F:..."   # pin the daemon's certificate (just host::cert prints it)
# allow_insecure = false                    # allow ws:// to a host that is not loopback
# buffer_limit_bytes = 8388608              # replay buffer for each session
# resume_grace_secs = 600                   # how long a session lives with no link
# docker_bin = "docker"
# vmlab_bin = "vmlab"
# git_bin = "git"
# work_dir = "/srv/terrarium/work"          # the host path of the daemon's TERRARIUM_WORK_DIR; default ./work
# instances_dir = "/var/lib/terrarium-host/instances"   # lab instances; default $XDG_DATA_HOME/terrarium-host/instances
# repos_dir = "/srv/terrarium/repos"        # the host path of TERRARIUM_REPOS_DIR, for a local lab_repo; default ./repos
# max_running_targets = 4                   # docker and vmlab targets that may run at one time
# auto_update = true                        # take the daemon's build (see "Self-update")

# [display]                                 # target displays (see "display"); all optional
# enabled = false                           # open THIS machine's desktop to `desktop` targets
# allowed_displays = [":0"]                 # the displays a desktop target may use; empty: any
# default_display = ":0"                    # for a desktop target that names none
# xauthority = "/home/alice/.Xauthority"      # XAUTHORITY for the X tools
# runtime_dir = "/run/user/1000"            # XDG_RUNTIME_DIR for the Wayland tools
# tesseract_bin = "tesseract"               # OCR for the backends that have none
# rdp_known_hosts = "/var/lib/terrarium-host/rdp-known-hosts"  # RDP certificate pins; default $XDG_DATA_HOME/terrarium-host/rdp-known-hosts
```

**`[display] enabled = true` is a risk.** An agent with an exclusive lease
on a `desktop` target of this host can see the screen, move the mouse, and
type, as the user of the host agent. It can then do anything that user can
do in the desktop session, also outside Terrarium. Turn it on only for a
machine that is there for agents (a test desktop, a kiosk VM), not for your
own workstation. The daemon cannot turn it on, and the host agent logs a
warning at start when it is on. A systemd service has no desktop session
environment: set `xauthority` (X11) or `runtime_dir` (Wayland), and run the
host agent as the user that owns the session, or give that user access (for
example `xhost +SI:localuser:terrarium`).

A relative `work_dir` or `repos_dir` is relative to the working directory of
the host agent. `terrarium-host check` prints the three paths and says
`(missing)` for a path that does not exist.

The flags `--daemon-url`, `--token`, and `--name` (and the env variables
`TERRARIUM_DAEMON_URL`, `TERRARIUM_HOST_TOKEN`, `TERRARIUM_HOST_NAME`)
override the file. `TERRARIUM_HOST_CONFIG` sets the config path.

Commands:

| Command | Does |
|---|---|
| `terrarium-host run` | Connect and serve sessions. The default command. Stops on SIGTERM or Ctrl-C. |
| `terrarium-host check [--json] [--probe]` | Print the name, the daemon URL, whether a token is set, the TLS mode, and the detected capabilities. Makes no connection. With `--probe`: also do the TCP connect and the TLS handshake (no token is sent). |
| `terrarium-host token-file PATH` | Read a token from stdin and write it to `PATH` with mode 0600. |
| `terrarium-host cert NAME... [--out DIR] [--force]` | Write a self-signed `host-cert.pem` and `host-key.pem` for the daemon's host listener (default `config/tls`), and print the SHA-256. |
| `terrarium-host fingerprint CERT` | Print the SHA-256 of the first certificate in a PEM file. |

Logs go to stderr through `tracing`. `RUST_LOG` sets the level (default
`info`).

## Install

The systemd unit is `crates/terrarium-host/terrarium-host.service`. It runs
`/var/lib/terrarium-host/bin/terrarium-host --config /etc/terrarium-host/config.toml run`
as the user `terrarium`, reads optional overrides from
`/etc/terrarium-host/env`, adds `~/.local/bin` to `PATH` (for a per-user
vmlab), and restarts always (`Restart=always`: a self-update exits 0). Its
`ExecStartPre=` line is the self-update guard (see [Self-update](#self-update)).
The service user owns `/var/lib/terrarium-host/bin/` and the binary, and
`/usr/local/bin/terrarium-host` is a symlink to the binary. `just host::install` sets `User=` and
`Group=` from `--user`. Edit `SupplementaryGroups=` to match the host.

There are two installers. Both make the same config and the same unit
(`scripts/test_install_host.py` compares them):

- `scripts/install-host.sh` runs on the host. The daemon serves it at
  `GET /install/host.sh`, with the binary at `GET /install/terrarium-host`
  ([api.md §17.9](api.md#179-the-host-installer)). It needs no checkout of
  the repo and no ssh from the operator.
- `scripts/host-install.py` (`just host::install`) runs on the operator's
  machine. It builds the binary locally and uses ssh.

**`curl -fsSL <daemon>/install/host.sh | sudo sh -s -- --token <enrol token>`**

`terrarium admin host new <name> --enrol` and "Add host" on the Hosts page
print this line with the token and `--name`. The script runs as root. Its
flags:

| Flag | Meaning |
|---|---|
| `--token TOKEN` | The enrol token. `TERRARIUM_ENROL_TOKEN` also works. A new host needs one. |
| `--name NAME` | The host agent name. Default: the short host name. |
| `--url URL` | The daemon URL for the downloads. Default: from the daemon. |
| `--host-url URL` | `daemon_url`. Default: from the daemon. A bare name becomes `wss://NAME:8082`. |
| `--fingerprint FP`, `--no-pin` | The pin, or no pin. Default: the pin from the daemon, for a `wss://` URL. |
| `--user USER` | The service user. Default: `SUDO_USER` (or `DOAS_USER`), else `terrarium` (a new system user). |
| `--enable-display` | A new config gets `[display]` with `enabled = true`, so `desktop` targets can use the screen of this machine. Default: off. The script prints the risk. It does not change an existing config: it tells you to add the table by hand. |
| `--dry-run` | Print the plan and change nothing. |
| `--upgrade` | Replace the binary and the unit, keep the config and the token, restart. It keeps the `User=` of the installed unit. It moves the binary of an install from before self-update out of `/usr/local/bin`, and it removes `.pending` and `.failed` of a self-update. |
| `--uninstall [--purge]` | Stop, disable, and remove the unit and the binary. `--purge` also removes `/etc/terrarium-host`. |

The script does these steps:

1. It checks each value (the name, the user, the URLs, the pin, the
   token) before it changes anything. A new host with no `--token` stops
   the script.
2. It finds Docker and vmlab on the `PATH` of the unit
   (`/usr/local/bin`, `/usr/bin`, `/bin`, and `~/.local/bin` of the
   service user), and systemd (`/run/systemd/system`).
3. It makes the service user when it does not exist (`useradd --system`,
   as `host-install.py` does). With Docker, it adds the user to the group
   `docker`; with vmlab, to `kvm`. systemd gives the unit the
   supplementary groups of its `User=`.
4. It downloads the binary and its SHA-256 for `uname -m`, checks the
   hash, and installs the binary as
   `/var/lib/terrarium-host/bin/terrarium-host` (mode 0755; the directory
   and the file are the service user's). It makes
   `/usr/local/bin/terrarium-host` a symlink to it. A wrong hash stops the
   script before it changes anything else.
5. It makes `/etc/terrarium-host` (mode 0700, owned by the service user)
   and, when it does not exist, `config.toml` (mode 0640, `root` and the
   group of the service user) with `daemon_url`, `token_file`, `name`, and
   the pin.
6. With `--token`, it writes the token to the token file as the service
   user (`terrarium-host token-file`, the token on stdin). The script
   never prints the token.
7. It writes the unit (`User=` and `Group=` set), runs
   `systemctl daemon-reload`, `enable`, and `restart`. Without systemd, it
   prints the command that runs the host agent.
8. It runs `terrarium-host check --probe` as the service user. A failed
   probe makes the script exit 1. Then it prints a summary.

The token is in the command line of `sh` while the script runs. It works
one time, for 10 minutes. `scripts/test_install_host.py` runs the script
with fake programs in a temp root.

**`just host::token <name> [enrol]`**

1. It runs `terrarium admin host new <name>` against the daemon (the Unix
   socket, or `TERRARIUM_URL` with `TERRARIUM_TOKEN`). With `enrol` as the
   second argument, it adds `--enrol`. Only the operator can do this. The
   daemon makes the host registry entry and stores only a hash of the
   token. A host that exists already gets `409`: run
   `terrarium admin host rotate <name>` to get a new token.
2. It prints the token once on stdout, so it can go into a pipe. It prints
   nothing else on stdout. For an enrol token, the expiry goes to stderr.

**`just host::install <user@host> [name] [flags]`**

The recipe runs `host::build`, then `scripts/host-install.py` with `uv`.
The script uses `ssh` and `scp` to `<user@host>` as ssh takes it (use
`~/.ssh/config` or `--ssh-opt` for a port or a key). `name` is the host
agent name. The default is the first label of the host name (`lab1.lan`
gives `lab1`). The ssh user must be `root`, or must have `sudo` with no
password prompt: the script uses `sudo -n`.

The script does these steps:

1. It finds `daemon_url`. The order is `--daemon-url`, then
   `TERRARIUM_HOST_DAEMON_URL`, then the host of `TERRARIUM_PUBLIC_URL`.
   A bare host name becomes `wss://<name>:8082`. A `localhost` URL stops
   the install, because a remote machine cannot dial it.
2. For a `wss://` URL, it finds the pin
   (`server_fingerprint_sha256`). The order is `--fingerprint`, then
   `config/tls/host-cert.fingerprint`, then
   `terrarium-host fingerprint config/tls/host-cert.pem` (`--cert` sets
   another certificate). With no certificate, or with `--no-pin`, the
   config has no pin, and the host agent checks the daemon against the
   system roots.
3. It reads what is on the host already: the config, the token file, the
   unit, and the service user.
4. It copies `target/host/terrarium-host`, the unit (with `User=` and
   `Group=` set), and a new config to the home directory of the ssh user
   with `scp`. It does not copy the config when one exists.
5. As root on the host, it makes the service user if it does not exist
   (`useradd --system`), installs the binary as
   `/var/lib/terrarium-host/bin/terrarium-host` (owned by the service
   user, for the [self-update](#self-update)) with a symlink in
   `/usr/local/bin/`, installs the unit in `/etc/systemd/system/`, and
   runs `systemctl daemon-reload`. It
   makes `/etc/terrarium-host` with mode 0700, owned by the service user,
   because the host agent writes the new token through a temporary file
   and a rename. A new config gets mode 0640 (`root`, the service group),
   with `daemon_url`, `token_file = "/etc/terrarium-host/token"`, `name`,
   and the pin. An existing config does not change.
6. It gets an enrol token from the daemon:
   `terrarium admin host new <name> --enrol`, or
   `terrarium admin host rotate <name> --enrol` when the daemon knows the
   host already. When the token file exists on the host, the script asks
   before it replaces it. When the daemon knows the host but the host has
   no token file, the script asks before it rotates. `--yes` answers yes.
   On a no, the existing token file stays.
7. It writes the enrol token to the token file with
   `sudo -u <user> terrarium-host token-file /etc/terrarium-host/token`.
   The token goes on stdin, not in an argument. The file has mode 0600.
8. It runs `systemctl enable terrarium-host` and
   `systemctl restart terrarium-host`. The host agent dials with the enrol
   token in its 10 minutes and swaps it for the long-lived token
   ([Tokens](#tokens)).
9. It runs `terrarium-host check --probe` on the host as the service user
   and prints the result. A failed probe makes the script exit 1. After a
   new token, it waits up to 20 seconds for `admin host show` to show the
   host online.

A second run is safe. It replaces the binary and the unit, keeps the
config, and asks before it replaces the token file.

`just host::install <user@host> [name] --enable-display` writes
`[display]` with `enabled = true` into a new config, as `install-host.sh`
does (default: off; an existing config does not change).

`just host::install <user@host> [name] --dry-run` skips `host::build`,
prints each `ssh`, `scp`, and `terrarium` command and the generated unit
and config, and runs nothing. A dry run assumes a new host. The enrol token
shows as `<enrol token>`. `just host::install-test` runs the unit tests of
the script (`scripts/test_host_install.py`).

## What the daemon side must do

The daemon side is in `crates/terrariumd/src/hosts.rs` (the dial-in
endpoint, the host registry, and the session multiplexer),
`src/executor.rs` (the routing and the policy checks), and
`src/api/hosts.rs`. The tests are in `crates/terrariumd/tests/hosts.rs`.
Each item of the list is done:

- [x] Serve `GET /hosts/connect` on the dial-in listener
  (`TERRARIUM_HOST_LISTEN`; default `0.0.0.0:8082` with TLS, `127.0.0.1:8082`
  without; and on the Unix socket, not on the dashboard port). Check the bearer token against the
  host registry before the upgrade. Answer `401` on a bad token.
  (`hosts::connect`, `hosts::HostListen`)
- [x] TLS on the dial-in listener with `TERRARIUM_HOST_TLS_CERT` and
  `TERRARIUM_HOST_TLS_KEY` (`hosts::serve_tls`).
- [x] Token rotation, the enrol swap, and the `first_used_at` and
  `last_used_from` record ([Tokens](#tokens)).
- [x] Read `hello`. Refuse an unknown `protocol` with `goodbye` `protocol`.
  Store the capabilities for the dashboard and for policy checks
  (`HostCapabilities::summary` gives the flat `Capabilities`). The
  registry keeps the capabilities of the last `hello`; `GET /api/hosts`
  shows them with the `summary`.
- [x] Send `welcome` with a new `resume_token` and the `resume` list, as in
  [Reconnect](#reconnect). (`HostRegistry::attach`)
- [x] Pick session IDs with a counter for each host. Never reuse a live ID.
  (`HostRegistry::open`)
- [x] Send `ack` at least once for each 1 MiB of output, and at the end.
  The daemon acks each 256 KiB that the consumer took.
- [x] Send `release` after each terminal frame.
- [x] Send `ping` every `heartbeat_secs`, and close a link that is silent for 3
  intervals.
- [x] Apply the agent's policy before it sends any frame
  (`policy::use_target` in `executor`, before a session opens).
- [x] Send `limits` only to a host agent whose `capabilities.limits` can
  apply them, and refuse the session (`409 conflict`) otherwise
  (`executor::exec_limits` for `exec`, `docker_limits` for `docker.run`;
  [Resource limits](#resource-limits)).
- [x] Register and revoke host agents with `POST` and `DELETE /api/hosts`.
  Only the operator can do this, because PLAN §10 keeps host agent tokens
  for the operator. `just host::token <name>` runs
  `terrarium admin host new <name>` and prints the token alone on stdout.
  `POST /api/hosts/{name}/rotate` rotates it.

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
| TLS | rustls with ring on both sides: the daemon's listener with `TERRARIUM_HOST_TLS_CERT`/`_KEY`, and the host agent with the system roots, `ca_file`, or a pinned SHA-256. Static musl builds need no OpenSSL. `ws://` only to loopback unless `allow_insecure`. |

## Verified live

Date: 2026-10-25. The Docker stack ran the daemon. One host agent ran
on the same machine, without systemd, as the user `wil`. Docker was
29.8.0 and vmlab was 0.33.3-alpha.

### Set-up

1. `just host::cert localhost 127.0.0.1` wrote the certificate and the key
   in 3 s. `.env` got `TERRARIUM_HOST_TLS_CERT` and `TERRARIUM_HOST_TLS_KEY`,
   and `TERRARIUM_HOST_LISTEN` stayed `auto`. After `just up`, the daemon
   log showed `host agent dial-in listening on TCP with TLS
   addr=0.0.0.0:8082` and the SHA-256 of the certificate. `openssl
   s_client` got TLSv1.3 with `TLS_AES_256_GCM_SHA384`. A `curl -k` without
   a token got `401`.
2. The config was `state/host-local/config.toml`: `daemon_url =
   "wss://localhost:8082"`, the pin from `terrarium-host fingerprint`,
   `token_file`, `name = "local"`, `max_sessions = 8`, and
   `allowed_cwd_roots`. The roots must hold each vmlab lab directory,
   because the lab, its `.terrarium-stage/`, and the `rm` of a staging
   file are checked against them.
3. `terrarium admin host new local --enrol | terrarium-host token-file
   <token_file>` wrote the enrol token (mode 0600). `terrarium-host run`
   connected in 8 ms and logged `enrolled: stored the long-lived host
   token`. The file then held a `thk_` token, mode 0600. `GET /api/hosts`
   showed `local` online, with `docker 29.8.0` and `vmlab 0.33.3-alpha`.
4. `terrarium-host check --probe`: `TLS ok (TLSv1_3,
   TLS13_AES_256_GCM_SHA384) in 1 ms`, and the server SHA-256 was the pin.

### Docker target

A global target `dbox`: `kind = "docker"`, `host = "local"`, `image =
"debian:bookworm-slim"`.

| Check | Result |
|---|---|
| `up` (the image was in the cache) | `running` in 0.44 s |
| `exec -- sh -c 'echo hello from $(hostname); id'` | the output, uid 0, exit 0, in 0.08 s |
| `exec -- sh -c 'exit 7'` | CLI exit 7 |
| `exec -i` with stdin, `exec -t` | stdin reached `cat`; `tty` gave `/dev/pts/0` |
| `exec` of 20 MB to stdout | 20000000 bytes in 0.31 s (the ack path) |
| `exec --timeout 2 -- sleep 10` | CLI exit 137 |
| `exec -- nosuchprog` | the Docker message, CLI exit 127 |
| `cp` of 300000 bytes both ways | the same SHA-256 both ways, 0.14 s and 0.12 s |
| `ephemeral = true`, `keep = false`, then `down` | `absent`, the container is removed |

### vmlab target

The lab is `labs/terrarium-test/vmlab.wcl`: one `container "box"` with
`image = "alpine:3.22"`, `profile = "container"`, and `mode = :idle`. It
has no NIC. It is the cheapest vmlab machine: no template, only a 3.6 MB
image (the squashfs). The global target `vlab` has `kind = "vmlab"`,
`lab` (the directory), `machine = "box"`, `ephemeral = true`, and
`snapshot_before_task = true`.

| Check | Result |
|---|---|
| `vmlab validate` | `ok: lab "terrarium-test" — 0 vm(s), 1 container(s), 0 segment(s)` |
| `status` before `up` | `stopped` (vmlab printed `lab "terrarium-test": not running`) |
| `up`, with the first pull of `alpine:3.22` | `running` in 4.1 s |
| `exec -- uname -a` | `Linux box 6.12.113-0-virt ... x86_64 Linux` |
| `exec -- sh -c 'id; echo err >&2; exit 5'` | stdout, stderr, CLI exit 5 |
| `cp` in and out (11 bytes) | the same SHA-256, 0.25 s and 0.10 s |
| `snapshot --snapshot clean` | 0.47 s; `vmlab snapshot list box` shows `clean online` |
| `restore --snapshot clean` | 0.59 s; a file made after the snapshot was gone |
| `down` (ephemeral: `vmlab destroy`) | `absent` in 0.25 s; `.vmlab/` is removed |

What vmlab printed (each exit 0):

```
$ vmlab up
pull: alpine:3.22
pull: alpine:3.22 done
lab "terrarium-test" is up                       (2.8 s with the image cached)
$ vmlab down
lab "terrarium-test" is down (clones retained)    (0.54 s)
$ vmlab destroy
lab "terrarium-test" destroyed                    (0.18 s)
$ vmlab status -v
lab "terrarium-test"

  NAME KIND      STATUS  IP TEMPLATE/IMAGE
  box  container running -  alpine:3.22
      state=running ready=yes cached=yes health=- exit=- digest=sha256:dc92...
```

The real outputs are fixtures in
`crates/terrarium-host/tests/fixtures/vmlab-0.33.3/`. The test
`vmlab::tests::parses_live_vmlab_0_33_output` reads them. The parser
needed no change.

Facts about vmlab 0.33.3 that the sections above do not give:

- `vmlab up` returns before a container is ready. Right after it,
  `status` shows `starting` and `ready=no`. `vmlab exec` works at once
  all the same.
- vmlab sizes each table column to its widest cell, so the columns are
  narrow (`NAME KIND      STATUS  IP TEMPLATE/IMAGE`). The parser reads
  the header positions, so this works.
- `vmlab destroy` on a running lab stops it and removes it in one step.
  On a lab that does not exist, it exits 0.
- `vmlab exec` into a container takes 5 to 16 ms. It can end before the
  client sends `eof`; see the fix below.
- `status` cannot tell a destroyed lab from a stopped one: both print
  `not running`. So a `status` after an ephemeral `down` reports `stopped`,
  while the `down` stored `absent`.

### Through an agent

Project `lab` (prefix `LAB`). The agent `tester` (Claude, haiku, prompt
`default`, `policy.targets = ["dbox", "vlab"]`) started. The start ran
`vmlab up` for `vlab` because it is ephemeral. In one turn, the agent ran
`terrarium targets ls`, `terrarium exec dbox -- uname -a`, and
`terrarium exec vlab -- uname -a` through the runner tunnel and the
long-poll session endpoints. It replied with the outputs verbatim. The
audit has two `target.exec` rows with the actor
`agent:01M5S5Q8FXH5AA1JSJ84JD65TZ`. The agent `outsider`
(`policy.targets = []`) got `target `dbox` is not in your policy.targets;
ask a project admin to add it [forbidden]`, CLI exit 3, and no audit row.
The stop of `tester` ran `vmlab destroy` for `vlab`.

### Bugs found and fixed

| Bug | Fix | Test |
|---|---|---|
| A permanent Docker target lost its files on `down` and `up`: `up` removed the stopped container and ran a new one. | The new verb `docker.start`. `up` starts a stopped container. Only an ephemeral target with `image` starts fresh. | `hosts::a_permanent_docker_target_restarts_its_container_on_up`, `docker::tests::other_verbs_golden` |
| `down` of a Docker target took 10.2 s: `sleep infinity` as PID 1 ignores SIGTERM, so `docker stop` waited for the grace. | `docker.run` takes `init`; the daemon sets it (`--init`). `down` then took 0.24 s. | `docker::tests::run_golden`, `hosts::lifecycle_verbs_map_to_docker_and_vmlab` |
| `terrarium exec vlab -- uname -a` failed one time in seven with `the session has ended; read its output for the end [conflict]`. The command ended before the CLI sent `eof`, and the daemon refused the late `eof`. | A late `eof` is a no-op (`"accepted": false`). Data after the end is still `409`. After the fix: 30 of 30 runs passed. | `hosts::exec_streams_output_and_the_exit_code` |
| The `up` on an agent start and the `destroy` on an agent stop wrote no audit row. | `ensure_up` and `maybe_down` write `target.up` and `target.destroy` with the actor `system` and a `reason`. | `hosts::lifecycle_verbs_map_to_docker_and_vmlab` |
| `snapshot_before_task` was stored, but the daemon did not use it. | On an agent start, a vmlab target with the flag gets the snapshot `terrarium-clean` after `up` and before any exec, when it has none. On a stop, a kept target is restored to it. A destroy clears it. A permanent target gets it one time, when it runs. Docker: nothing. Audit rows `target.snapshot` and `target.restore`, actor `system`. | `hosts::snapshot_before_task_takes_and_restores_terrarium_clean` |
| `terrarium agent start` printed `start: lab/tester is stopped`: the daemon answers `202` with the status before the start. | The CLI polls the agent for up to 10 s and prints the new status with the old one (`start: lab/tester is idle (was stopped)`). `--no-wait` prints `start requested; was stopped`. The same for `stop` and `restart`. | `commands::tests::agents` |
| `terrarium exec -i` failed with `409 conflict` when the command ended before its stdin was sent. | The CLI stops the input on a `409` for `data`, and takes the exit code from the output stream, as for a late `eof`. | `exec_stdin_after_the_command_ended_is_not_an_error` (`crates/terrarium/tests/cli.rs`) |

The fixes were verified live on a second daemon outside Docker, with a
second host agent. The stack image still has the old code until the next
`just up`.

### Mode 1: workspaces on targets

Date: 2026-10-25, after `just up` with the code of PLAN §11 "mode 1".
A second host agent `mode1` ran on the same machine
(`state/host-mode1/config.toml`: the pin of `state/host-local`, `work_dir =
"<repo>/work"`, `repos_dir = "<repo>/repos"`, `instances_dir` under
`state/host-mode1`), enrolled with `terrarium admin host new mode1
--enrol`. The daemon log at the start: `agent workspaces now live under the
work root; each moves at its next start agents=11 work_dir=/work`.

Project `mode1`, repo `labs` = `file:///repos/labs.git` (the example repo:
`alpine-box/vmlab.wcl`, and `vmlab.wcl` at the root). The project targets
(the CLI printed `created target mode1-dk (a project target gets the
project prefix)`):

| Target | Definition |
|---|---|
| `mode1-dk` | `docker`, `debian:bookworm-slim`, ephemeral |
| `mode1-vbox` | `vmlab`, `lab_repo = "labs"`, `lab_ref = "main"`, `lab_path = "alpine-box"`, `machine = "box"`, ephemeral |
| `mode1-vws` | `vmlab`, `lab_from_workspace`, `lab_path = "labs"`, `machine = "box"`, ephemeral |

The agent `mode1/w1` (Claude, haiku, `policy.targets = [mode1-dk,
mode1-vbox]`) got `terrarium repo checkout labs`, then started:

| Check | Result |
|---|---|
| The start | `mode1-dk` and `mode1-vbox` `running`. `docker inspect`: one bind, `/home/alice/dev/terrarium/work/mode1/w1` to `/work/mode1/w1`, user `1000:1000`. |
| A file made on the host in `work/mode1/w1`, then `terrarium exec mode1-dk -- sh -c 'ls -la /work/mode1/w1; cat ...'` | The file and its text, exit 0, 0.15 s. |
| The vmlab instance | `instances/mode1-vbox/src` is a detached worktree at `3230d5a` (`git worktree list` of `labs.git`), and `instances/mode1-vbox/lab/vmlab.wcl` has `@dev(workspace = ".../work/mode1/w1", workspace_guest = "/work/mode1/w1")` on `box`. `vmlab status -v` shows the `DEV` row. |
| `terrarium exec mode1-vbox -- sh -c 'hostname; ls -la /work/mode1/w1; cat ...'` | `box`, the file and its text, exit 0, 0.13 s. |
| A file written in the guest, and one in the container | On the host after about 400 ms (the syncer), and at once (the bind). |
| `admin target grant mode1-vws w1` | The agent's own instance `mode1-vws.<ULID>` from `work/mode1/w1/labs/vmlab.wcl` (lab `labs-dev`), `running`. `git status` of the checkout: clean. |
| `exec mode1-vws --agent mode1/w1`; the same without `--agent`; a grant with `--exclusive` | The output from `box`; `422 ... name the holder with agent`; `422 ... an exclusive lease does not apply`. |
| A lab in the workspace with `@dev(workspace = "/home/alice/.ssh")` and a `provision` | `vmlab.prepare failed: ... refused:` with both lines named, `details.code` `refused`. Nothing ran. |
| One haiku turn: write `loop.txt` with the file tool, then `terrarium exec mode1-dk -- sh -c 'cd /work/mode1/w1 && cat loop.txt && id -u && sha256sum loop.txt'` | The tool wrote `/work/mode1/w1/loop.txt` (the second path in the sandbox). The target printed `edited in the sandbox`, `1000`, and the SHA-256. The audit row `target.exec` has the actor `agent:<ULID>`. The lease grant before had typed one pointer turn, so 2 turns in all. |
| `admin agent stop w1` | All three targets `absent`, `runtime` `{}`. The container is gone, both instance dirs are gone, and `labs.git` has no worktree left. `vmlab lab list` shows no lab of the test. |

Found and fixed on the way: a container ran as root, so the files of a
build in the workspace were root's (now `-u <owner>`); a refused prepare
left an empty instance dir (now removed); `terrarium exec` had no
`--agent`. Still open: `terrarium admin project repo-add <URL>` reads the
URL as the daemon URL (the global `--url` flag), so the repo was attached
with `POST /api/projects/mode1/repos`.

### Open

- The `snapshot_before_task` and CLI fixes above have unit tests only.
  They were not tried on the live stack.
- In a PTY, `docker exec -t` sets `TERM=xterm` in the container. The
  `TERM=xterm-256color` of the host agent goes to the `docker` process
  only.
- `vmlab shell` and `terrarium shell` were not tried by hand.

# The Terrarium container

This directory holds the image (`Dockerfile`), the entrypoint
(`entrypoint.sh`), the seccomp profile (`seccomp-bwrap.json`, see
[seccomp-bwrap.README.md](seccomp-bwrap.README.md)), and the crash test
(`crash-test.py`).

## The process tree

```
tini (PID 1)
└── terrarium-entrypoint (bash, the supervisor loop)
    └── terrariumd
terrarium-runner (one for each agent, its own session; parent: terrariumd, or tini after a daemon crash)
└── bwrap ── harness TUI
```

`terrariumd` starts each `terrarium-runner` detached: the runner calls
`setsid`, and its stdout and stderr go to
`/state/agents/<ulid>/private/runner.log`. When `terrariumd` dies, the kernel gives
its runners to PID 1. tini is PID 1, and it reaps them when they exit.

## The restart loop

The entrypoint runs `terrariumd` as a child and waits for it. When
`terrariumd` ends with a non-zero status or a signal, and the container
did not get a stop signal, the entrypoint:

1. Logs the status and the uptime.
2. Waits for the backoff: 1 s first, doubled after each crash, to a
   maximum of `TERRARIUM_SUPERVISOR_MAX_BACKOFF` (30 s).
3. Starts `terrariumd` again in the same container.

When `terrariumd` ran for `TERRARIUM_SUPERVISOR_RESET_AFTER` (300 s) or more
before a crash, the backoff goes back to 1 s.

The runners keep running during the restart. The runtime dir
(`/run/terrarium`) is in the container filesystem, and the daemon does not
clear `/run/terrarium/runners/` at start. The new daemon reads the pid files
and the control sockets there, and `Supervisor::recover` connects to each
live runner again. The harness does not restart.

When `terrariumd` exits with status 0 and the container got no stop signal,
the entrypoint stops the runners (see below) and the container exits.

The Compose service explicitly runs `--daemon-service terrariumd`. Only this
mode initializes releases and supervises the daemon. Direct `docker run`
must supply that command too. Other commands (for example `bash`, backup
subcommands, or `terrariumd --version`) run once using bundled binaries and
never change the live release or probation marker. A state-volume lock rejects
a second service, including an accidental `compose run terrarium` without a
replacement command.

The updater normalizes the service prefix for the destination image when it
creates a long-running container. This installs the new supervisor from an old
image default command, and strips the prefix again for rollback to an old image.
One-off offline commands never receive this prefix.

## Binary-only upgrades

The image has one final release-payload layer. All preceding RootFS layer
digests, the runtime image configuration and `io.terrarium.runner-compat-epoch`
must match for a hot swap. Missing metadata, a changed runtime base, deployment
environment changes, or a protocol break selects the drained container path.
The first upgrade from an image without this supervisor therefore replaces
the container, putting pre-TER-260 runners onto hold/replay-capable binaries.

The updater verifies and pulls by digest before staging the immutable payload
under `/state/releases/<content-hash>`. `current` is an atomic symlink. Besides
the daemon, runner and CLI, the payload includes packaged defaults, harness
pins and host distribution files so a binary upgrade cannot silently retain
old release assets. Docker's physical container image does not change. The
updater reports the selected payload's `image.json` instead.

The supervisor pauses only its own daemon child, with graceful RPC drain, and
keeps the runners alive. The updater takes the full backup for explicit
rollback and a separate offline SQLite snapshot for automatic rollback. It
then selects the new payload and starts the daemon in probation. Health and
reads work, but agent RPCs retry, HTTP writes return `503` with `Retry-After`,
and background work is not activated until commit. On failed health it stops
the probation daemon, restores only the database, selects the old payload,
checks health and releases admission. Live runner logs and sockets stay intact.
An ambiguous commit response never causes a database rewind.

An explicit rollback after successful commit uses container replacement and
the full backup, because admitted writes may already have external effects.
Release binaries are not part of application backups. The selected release
survives a restart of the same container, while a new container boots its own
bundled release. The updater process itself stays on its bundled binary until
its container is replaced.

The total hold budget is 15 minutes, including drain, backup and health.
The updater reserves the last minute for recovery and retries commit three
times. Admission remains closed until activation finishes. An activation
failure exits the daemon nonzero so the supervisor can restart it. Once any
commit was attempted, automatic journal rollback is forbidden.
After commit the updater waits up to 30 seconds (within the remaining hold
budget) for core activation. A failure reports Failed without a journal rewind.
Use explicit rollback if the committed release cannot activate.

Post-commit activation has two remaining ordering windows: harness pin install
and default seeding in the executable follow core activation, and the git proxy
binds asynchronously. An agent admitted in that interval can see the previous
pins/defaults or a not-yet-ready proxy. Core reconcile and background activation
must finish before admission, but auxiliary provisioning is not atomic with it.
Do not treat probation health as proof that post-commit provisioning succeeded.

Commit prunes unused payloads, keeping `current`, `previous`, and releases
with a live executable under `/proc`. Thus retained runners keep their binaries.

### Interrupted transaction recovery

Stop automatic update attempts first. Keep the existing daemon container and
runners alive. Use its actual container name in place of `terrarium` below:

```sh
docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py status
```

This reads durable `/state/releases/transaction.json` plus current marker
state. Record `old`, `new`, `backup`, `phase` and `admission_attempted`.
Use the literal values from this trusted local record as quoted arguments.
Do not delete markers by hand or replace the container during probation.
The helper is private to administrators, not exposed to agents.

* If `admission_attempted` is true, **never restore the database with retained
  runners**, even if `phase` is `commit_attempted` or the response was lost.
  Retry `docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py commit`.
  It is idempotent. Check daemon logs and health. If activation failed, fix
  the reported cause and let the supervisor restart the selected release.
  A later rollback must use the updater's drained container-replacement path.
* For `staged` or `snapshotting`, with `current` equal to `old` and no probation,
  run `docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py cancel-pause`.
  The old daemon resumes. A partial snapshot is not a valid rollback source.
* For `backed_up`, `probation`, `restoring` or `restored` with
  `admission_attempted: false`, restore the old release with the sequence below.
  An interrupted SQLite restore is safe to repeat from the completed snapshot.
  Do not use this sequence if admission was ever attempted.

`paused` and `pause_requested` are independent status flags, not transaction
phases. A pause during rollback preserves `probation`, `restoring` or `restored`.
After interruption at any pause, use the durable phase and admission flag above,
not the pause flag, to choose recovery. `cancel-pause` refuses probation/restore
states rather than incorrectly marking an unfinished rollback aborted.

```sh
docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py pause
docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py restore-database 'BACKUP_PATH'
docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py start 'OLD_RELEASE_HASH'
docker exec terrarium terrarium daemon status
# Only after health succeeds:
docker exec terrarium python3 /usr/local/lib/terrarium-hot-swap.py commit 'BACKUP_PATH' 'OLD_RELEASE_HASH'
```

Read `status` again and confirm probation is absent and health succeeds.
After manual rollback, align the deployment's image setting with the restored
release before enabling automatic updates. `committed`, `rolled_back` and
`aborted` are terminal phases. If the durable record or completed snapshot is
missing, stop and use a drained full-backup recovery instead of guessing.

## Container shutdown sequence

`docker compose down` and `docker stop` send SIGTERM to tini. tini sends it
to the entrypoint only (no `-g`). Then the entrypoint:

1. Sends the signal (SIGTERM or SIGINT) to `terrariumd`.
2. Waits `TERRARIUM_DAEMON_STOP_TIMEOUT` (5 s) for it. After that, it sends
   SIGKILL. `terrariumd` leaves its runners running when it stops.
3. Sends SIGTERM to each live runner. It finds them through
   `/run/terrarium/runners/<ulid>.pid`. A runner types the exit keys of an
   idle harness, and stops a busy harness with SIGHUP.
4. Waits `TERRARIUM_SHUTDOWN_GRACE` (20 s) for the runners. After that, it
   sends SIGKILL to each runner that is still live.
5. Each runner that SIGTERM stopped writes `"stopped_by": "container"` into
   its `runner-exit.json`. The container stopped the agent, not the harness
   or the operator. So the next container start launches the agent again
   with resume (PLAN §18), whatever the exit code, and the restart policy
   does not see a normal exit. A runner that needed SIGKILL writes no exit
   record; the next start launches its agent again too.
6. Exits 0.

`compose.yaml` sets `stop_grace_period: 40s`. Keep it larger than
`TERRARIUM_DAEMON_STOP_TIMEOUT` plus `TERRARIUM_SHUTDOWN_GRACE`, else
Docker sends SIGKILL to the container before the runners stop.

## Environment variables

| Variable | Default | Use |
|---|---|---|
| `TERRARIUM_SUPERVISOR` | `1` | Set `0` to run `terrariumd` once, with no restart loop. |
| `TERRARIUM_SUPERVISOR_MAX_BACKOFF` | `30` | The maximum wait before a restart, in seconds. |
| `TERRARIUM_SUPERVISOR_RESET_AFTER` | `300` | The uptime, in seconds, after which the backoff goes back to 1 s. |
| `TERRARIUM_DAEMON_STOP_TIMEOUT` | `5` | The time, in seconds, that `terrariumd` gets to stop. |
| `TERRARIUM_SHUTDOWN_GRACE` | `20` | The time, in seconds, that the runners get to stop. |
| `TERRARIUM_RUNTIME_DIR` | `/run/terrarium` | The runtime dir. The daemon reads the same variable. |
| `TERRARIUM_STATE_DIR` | `/state` | The state dir. The daemon reads the same variable. |

## The crash test

`just docker::crash-test` starts the stack, starts a pi agent with no
provider, kills `terrariumd` with SIGKILL, and checks these items:

- `/api/health` answers again, with a new `started_at`.
- The runner of the agent has the same pid, and its parent is tini.
- The daemon log shows a reconnect to the runner.

Then it deletes the agent and the project, and prints PASS or FAIL.

## Verified

On 2026-10-24, commit `efd9663`: `just docker::image-build` (image 1.43 GB; claude 2.1.310, codex 0.160.0, copilot 1.0.103, pi 0.85.1), `just up`, the security headers on `/`, port 8082 loopback-only, `/a/` only on 8081, the cookie session flow (201, 200, 403, 204), and `just docker::crash-test` (daemon killed with SIGKILL, runner kept its pid, daemon reconnected, agent back to idle) all passed, twice. `just down` left no containers, and every file under `state/` and `config/` belongs to the host user.

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

The entrypoint supervises only `terrariumd` with no subcommand. Other
commands (for example `bash`, or `terrariumd sandbox-dry-run`) run once.

## The shutdown sequence

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

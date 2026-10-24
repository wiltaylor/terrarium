# /// script
# requires-python = ">=3.11"
# ///
"""Crash test for the daemon supervisor in docker/entrypoint.sh.

Run it with `just docker::crash-test`; the stack must be up. It:

1. Creates the project `crash-test` and the pi agent `probe` (no provider,
   restart `never`), and starts the agent.
2. Records the pid of terrariumd and of the agent's terrarium-runner.
3. Kills terrariumd with SIGKILL.
4. Waits until /api/health answers with a new `started_at`.
5. Checks that the runner kept its pid, that tini (PID 1) is now its
   parent, and that the new daemon connected to it again (no relaunch).
6. Stops and deletes the agent and the project.

When the agent cannot start (no runner shows up), the test checks only
the daemon restart. It prints PASS or FAIL and exits 0 or 1.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

PROJECT = "crash-test"
AGENT = "probe"
KEY = f"{PROJECT}/{AGENT}"
SERVICE = "terrarium"
RESTART_TIMEOUT_S = 60
RUNNER_TIMEOUT_S = 30


def log(msg: str) -> None:
    print(f"crash-test: {msg}", flush=True)


def env_file_value(name: str) -> str | None:
    env = Path(".env")
    if not env.exists():
        return None
    for line in env.read_text().splitlines():
        line = line.strip()
        if line.startswith(f"{name}="):
            return line.split("=", 1)[1].strip().strip('"').strip("'")
    return None


TOKEN = os.environ.get("TERRARIUM_TOKEN") or env_file_value("TERRARIUM_TOKEN") or ""
PORT = os.environ.get("TERRARIUM_PORT") or env_file_value("TERRARIUM_PORT") or "8080"


def compose_exec(*args: str, check: bool = False) -> subprocess.CompletedProcess[str]:
    cmd = ["docker", "compose", "exec", "-T", SERVICE, *args]
    return subprocess.run(cmd, capture_output=True, text=True, check=check)


def cli(*args: str) -> subprocess.CompletedProcess[str]:
    return compose_exec("terrarium", *args)


def health() -> dict | None:
    req = urllib.request.Request(
        f"http://localhost:{PORT}/api/health",
        headers={"Authorization": f"Bearer {TOKEN}"},
    )
    try:
        with urllib.request.urlopen(req, timeout=2) as r:
            return json.load(r)
    except (urllib.error.URLError, OSError, ValueError):
        return None


def processes() -> list[tuple[int, int, str]]:
    """(pid, ppid, args) for every process in the container."""
    out = compose_exec("ps", "-eo", "pid=,ppid=,args=").stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            rows.append((int(parts[0]), int(parts[1]), parts[2]))
    return rows


def daemon_pid() -> int | None:
    for pid, _, args in processes():
        if args.split()[0].endswith("terrariumd"):
            return pid
    return None


def runner_of(agent_id: str | None) -> tuple[int, int] | None:
    """(pid, ppid) of the agent's terrarium-runner."""
    for pid, ppid, args in processes():
        argv = args.split()
        if not argv[0].endswith("terrarium-runner"):
            continue
        if agent_id is None or agent_id in argv:
            return pid, ppid
    return None


def agent_id() -> str | None:
    r = cli("--json", "agent", "show", KEY)
    try:
        data = json.loads(r.stdout)
    except ValueError:
        return None
    for obj in (data, data.get("agent", {}) if isinstance(data, dict) else {}):
        if isinstance(obj, dict) and isinstance(obj.get("id"), str):
            return obj["id"]
    return None


def agent_status() -> str:
    r = cli("--json", "agent", "show", KEY)
    try:
        data = json.loads(r.stdout)
    except ValueError:
        return "unknown"
    for obj in (data, data.get("agent", {}) if isinstance(data, dict) else {}):
        if isinstance(obj, dict) and isinstance(obj.get("status"), str):
            return obj["status"]
    return "unknown"


def setup() -> str | None:
    r = cli("project", "new", PROJECT, "-d", "docker::crash-test")
    if r.returncode != 0 and "exist" not in (r.stderr + r.stdout):
        log(f"project new: {r.stderr.strip() or r.stdout.strip()}")
    r = cli("-p", PROJECT, "agent", "new", AGENT, "--harness", "pi", "--restart", "never")
    if r.returncode != 0 and "exist" not in (r.stderr + r.stdout):
        log(f"agent new: {r.stderr.strip() or r.stdout.strip()}")
    r = cli("agent", "start", KEY)
    if r.returncode != 0:
        log(f"agent start: {r.stderr.strip() or r.stdout.strip()}")
    return agent_id()


def cleanup() -> None:
    cli("agent", "stop", KEY)
    time.sleep(2)
    cli("agent", "rm", KEY, "--confirm")
    cli("project", "rm", PROJECT, "--confirm")


def main() -> int:
    failures: list[str] = []
    before = health()
    if before is None:
        log(f"FAIL: /api/health on port {PORT} does not answer; run `just up` first")
        return 1

    aid = setup()
    log(f"agent {KEY} id {aid}")
    runner = None
    deadline = time.monotonic() + RUNNER_TIMEOUT_S
    while time.monotonic() < deadline:
        runner = runner_of(aid)
        if runner and agent_status() in ("idle", "working", "blocked", "starting"):
            break
        time.sleep(1)
    status_before = agent_status()
    if runner:
        log(f"runner pid {runner[0]} (parent {runner[1]}), agent status {status_before}")
    else:
        log(f"no runner for {KEY} (status {status_before}): checking the daemon restart only")

    old_daemon = daemon_pid()
    if old_daemon is None:
        log("FAIL: no terrariumd process in the container")
        cleanup()
        return 1
    kill_at = datetime.now(timezone.utc)
    log(f"killing terrariumd pid {old_daemon} with SIGKILL")
    compose_exec("pkill", "-KILL", "-x", "terrariumd")

    after = None
    new_daemon = None
    deadline = time.monotonic() + RESTART_TIMEOUT_S
    while time.monotonic() < deadline:
        time.sleep(1)
        h = health()
        if h and h.get("started_at") != before.get("started_at"):
            after = h
            new_daemon = daemon_pid()
            break
    if after is None:
        failures.append(f"the daemon did not answer /api/health again in {RESTART_TIMEOUT_S}s")
    else:
        log(f"daemon is back: pid {new_daemon}, started_at {after.get('started_at')}")
        if new_daemon == old_daemon:
            failures.append(f"terrariumd kept pid {old_daemon}; it did not restart")

    if runner and after is not None:
        time.sleep(3)
        now = runner_of(aid)
        if now is None:
            failures.append(f"runner pid {runner[0]} is gone after the daemon restart")
        elif now[0] != runner[0]:
            failures.append(f"runner pid changed: {runner[0]} -> {now[0]} (relaunch, not reconnect)")
        else:
            log(f"runner kept pid {now[0]}; its parent is now pid {now[1]}")
            if now[1] != 1:
                failures.append(f"runner parent is pid {now[1]}, not tini (pid 1)")
        logs = subprocess.run(
            ["docker", "compose", "logs", "--no-color", "--since", kill_at.isoformat(), SERVICE],
            capture_output=True,
            text=True,
        ).stdout
        if "connected to the live runner again" in logs or "connected to runner pid" in logs:
            log("the new daemon connected to the live runner")
        else:
            failures.append("the daemon logs do not show a reconnect to the runner")
        status_after = agent_status()
        log(f"agent status after the restart: {status_after}")

    cleanup()
    if failures:
        for f in failures:
            log(f"FAIL: {f}")
        return 1
    scope = "daemon restart and runner reconnect" if runner else "daemon restart only (no runner)"
    log(f"PASS: {scope}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

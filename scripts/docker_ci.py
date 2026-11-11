"""Opt-in Docker gate. Only a daemon-provisioned Unix endpoint reaches branch code."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

LIVE_TESTS = (
    "hosts::docker_real_recreate_changes_image_id_and_preserves_workspace",
    "reservations_api::live_docker_release",
)


def passed_exactly(output: str, name: str) -> bool:
    """A successful cargo exit alone also allows ignored or unmatched tests."""
    events = []
    for line in output.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("type") == "test" and event.get("event") != "started":
            events.append((event.get("name"), event.get("event")))
    return events == [(name, "ok")]


def live_tests(env: dict[str, str]) -> int:
    results = env.get("RESULTS_DIR")
    report = None
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        report = Path(results, "docker.json").open("w", encoding="utf-8")
    try:
        for name in LIVE_TESTS:
            cmd = ["cargo", "test", "-p", "terrariumd", "--test", "it", "--",
                   "--ignored", "--exact", name, "-Z", "unstable-options",
                   "--format", "json", "--report-time"]
            run = subprocess.run(cmd, env=dict(env, RUSTC_BOOTSTRAP="1"),
                                 stdout=subprocess.PIPE, text=True, check=False)
            print(run.stdout, end="", flush=True)
            if report:
                report.write(run.stdout)
                report.flush()
            if run.returncode or not passed_exactly(run.stdout, name):
                print(f"Docker gate: {name} must run and pass, not skip", file=sys.stderr)
                return run.returncode or 1
    finally:
        if report:
            report.close()
    return 0


def suite_env(env: dict[str, str]) -> dict[str, str]:
    """The env of the full suite: without the gate's Docker env (TER-518).

    The suite starts its own test daemons and recipes, which must not see
    the outer merge job's gate switch or its Docker socket. Only
    `docker info` and the explicit live tests get DOCKER_HOST.
    """
    child = dict(env)
    for name in ("TERRARIUM_DOCKER_CI", ENDPOINT_ENV, "DOCKER_HOST", "DOCKER_CONFIG"):
        child.pop(name, None)
    return child


def gate(env: dict[str, str]) -> int:
    subprocess.run(["docker", "info"], env=env, check=True, stdout=subprocess.DEVNULL)
    code = subprocess.call(["just", "ci::validate"], env=suite_env(env))
    return code or live_tests(env)


# The daemon passes the job socket under this name, not as DOCKER_HOST: on a
# docker target the host's docker CLI would read DOCKER_HOST itself (TER-477).
ENDPOINT_ENV = "TERRARIUM_DOCKER_HOST"


def main() -> int:
    enabled = os.environ.get("TERRARIUM_DOCKER_CI", "0")
    if enabled == "0":
        # Gate off: no Docker endpoint reaches the suite, whatever is set.
        env = dict(os.environ)
        for name in ("DOCKER_HOST", ENDPOINT_ENV):
            env.pop(name, None)
        return subprocess.call(["just", "ci::validate"], env=env)
    if enabled != "1":
        print("TERRARIUM_DOCKER_CI must be 0 or 1", file=sys.stderr)
        return 1
    endpoint = os.environ.get(ENDPOINT_ENV, "")
    if os.environ.get("TERRARIUM_JOB_KIND") != "merge" or not endpoint.startswith("unix://"):
        print("Docker gate requires a merge job and its daemon-provisioned Unix endpoint", file=sys.stderr)
        return 1
    env = dict(os.environ)
    for name in (ENDPOINT_ENV, "DOCKER_CONTEXT", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH", "SSH_AUTH_SOCK"):
        env.pop(name, None)
    env["DOCKER_HOST"] = endpoint
    with tempfile.TemporaryDirectory(prefix="terrarium-docker-config-") as directory:
        env["DOCKER_CONFIG"] = directory
        return gate(env)


if __name__ == "__main__":
    sys.exit(main())

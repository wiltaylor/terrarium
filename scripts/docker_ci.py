"""Merge-only Docker gate. The private key goes to ssh-add on stdin, never a file."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

DOCKER_HOST = "ssh://root@10.3.0.2:2250"
KNOWN_HOST = "[10.3.0.2]:2250 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGgTrtbsNPDR6lWHJVpgAh6tStS4C7O9F0ksb405ScXy\n"
KEY_ENV = "DOCKER_CI_KEY"
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


def gate(env: dict[str, str]) -> int:
    subprocess.run(["docker", "info"], env=env, check=True, stdout=subprocess.DEVNULL)
    code = subprocess.call(["just", "ci::validate"], env=env)
    return code or live_tests(env)


def main() -> int:
    key = os.environ.pop(KEY_ENV, "")
    if os.environ.get("TERRARIUM_JOB_KIND") != "merge" or not key:
        print("Docker gate requires a merge job and its delivery-granted key", file=sys.stderr)
        return 1
    env = dict(os.environ)
    ssh = shutil.which("ssh")
    if not ssh:
        raise RuntimeError("Docker gate needs OpenSSH and the Docker CLI on the build target")
    with tempfile.TemporaryDirectory(prefix="terrarium-docker-ci-") as directory:
        temp = Path(directory)
        socket = temp / "agent.sock"
        hosts = temp / "known_hosts"
        hosts.write_text(KNOWN_HOST, encoding="utf-8")
        wrapper = temp / "ssh"
        wrapper.write_text(
            "#!/bin/sh\nexec " + shlex.quote(ssh) + " -F /dev/null"
            " -o BatchMode=yes -o StrictHostKeyChecking=yes -o IdentityFile=none"
            " -o GlobalKnownHostsFile=/dev/null -o UserKnownHostsFile=" + shlex.quote(str(hosts))
            + " -o IdentityAgent=" + shlex.quote(str(socket)) + ' "$@"\n', encoding="utf-8")
        wrapper.chmod(0o700)
        agent = subprocess.Popen(["ssh-agent", "-D", "-a", str(socket)],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 5
            while not socket.exists():
                if agent.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("ssh-agent did not start")
                time.sleep(0.02)
            env["SSH_AUTH_SOCK"] = str(socket)
            subprocess.run(["ssh-add", "-"], input=key + "\n", text=True, env=env,
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            key = ""
            env["PATH"] = str(temp) + os.pathsep + env.get("PATH", "")
            env["DOCKER_HOST"] = DOCKER_HOST
            return gate(env)
        finally:
            agent.terminate()
            try:
                agent.wait(timeout=5)
            except subprocess.TimeoutExpired:
                agent.kill()
                agent.wait()


if __name__ == "__main__":
    sys.exit(main())

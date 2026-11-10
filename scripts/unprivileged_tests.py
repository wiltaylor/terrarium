"""Run the terrarium-host tests again as an unprivileged user (TER-579).

Merge jobs run as root, the release CI as uid 1001: tests that depend on the
user passed in the merge suite and failed in the release CI (TER-494,
TER-575). This step reuses the test binaries that the root run of `ci::test`
built (`cargo test --no-run`, no rebuild with the same env) and runs each of
them as UID with `setpriv`, with a private HOME. Doctests are not run.
Without root it skips: the main run is then already unprivileged.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

PACKAGES = ("terrarium-host",)
UID = 1001
# Only these reach the tests: the merge job's env stays with root.
PASS_ENV = ("PATH", "LANG", "LC_ALL", "TZ", "RUST_BACKTRACE", "RUST_LOG")


def test_binaries(cargo_output: str) -> list[tuple[str, str]]:
    """(executable, package manifest dir) of each test artifact, in order."""
    found = []
    for line in cargo_output.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("reason") != "compiler-artifact" or not msg.get("profile", {}).get("test"):
            continue
        exe = msg.get("executable")
        if exe:
            found.append((exe, str(Path(msg["manifest_path"]).parent)))
    return found


def child_env(home: str) -> dict[str, str]:
    env = {k: os.environ[k] for k in PASS_ENV if k in os.environ}
    env.update(HOME=home, USER="ci", LOGNAME="ci")
    return env


def setpriv(uid: int) -> list[str]:
    return ["setpriv", f"--reuid={uid}", f"--regid={uid}", "--clear-groups", "--"]


def run() -> int:
    if os.geteuid() != 0:
        print("unprivileged tests: skipped, not root (the main run is already unprivileged)")
        return 0
    cmd = ["cargo", "test", "--no-run", "--message-format=json-render-diagnostics"]
    for p in PACKAGES:
        cmd += ["-p", p]
    build = subprocess.run(cmd, stdout=subprocess.PIPE, text=True, check=False)
    if build.returncode:
        return build.returncode
    bins = test_binaries(build.stdout)
    if not bins:
        print("unprivileged tests: cargo listed no test binaries", file=sys.stderr)
        return 1
    results = os.environ.get("RESULTS_DIR")
    report = None
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        report = Path(results, "rust-unprivileged.json").open("w", encoding="utf-8")
    home = tempfile.mkdtemp(prefix="terrarium-ci-home-")
    os.chown(home, UID, UID)
    failed = []
    try:
        env = child_env(home)
        args = []
        if report:
            env["RUSTC_BOOTSTRAP"] = "1"
            args = ["-Z", "unstable-options", "--format", "json", "--report-time"]
        for exe, cwd in bins:
            print(f"unprivileged tests: uid {UID}: {exe}", flush=True)
            with subprocess.Popen(setpriv(UID) + [exe, *args], cwd=cwd, env=env,
                                  stdout=subprocess.PIPE, text=True, encoding="utf-8",
                                  errors="replace") as child:
                assert child.stdout is not None
                for line in child.stdout:
                    print(line, end="", flush=True)
                    if report:
                        report.write(line)
                        report.flush()
                if child.wait():
                    failed.append(exe)
    finally:
        if report:
            report.close()
        shutil.rmtree(home, ignore_errors=True)
    if failed:
        print(f"unprivileged tests: {len(failed)} binaries failed as uid {UID}:", file=sys.stderr)
        for exe in failed:
            print(f"  {exe}", file=sys.stderr)
        return 1
    print(f"unprivileged tests: {len(bins)} binaries passed as uid {UID}")
    return 0


if __name__ == "__main__":
    sys.exit(run())

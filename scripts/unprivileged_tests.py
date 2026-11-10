"""Run selected test binaries again as an unprivileged user (TER-579, TER-611).

Merge jobs run as root, the release CI as uid 1001: tests that depend on the
user passed in the merge suite and failed in the release CI (TER-494,
TER-575). This step reuses the test binaries that the root run of `ci::test`
built (`cargo test --workspace --no-run`, the same command and env, so no
rebuild), keeps those that SELECT names, and runs each of them as UID with
`setpriv`, with its own private HOME and TMPDIR. Doctests are not run.
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

# Package -> the test targets to run ("lib" for the unit tests), None for all.
SELECT: dict[str, frozenset[str] | None] = {
    "terrarium-host": None,
    "terrariumd": frozenset({"lib", "it"}),
}
UID = 1001
# Only these reach the tests: the merge job's env stays with root. The rustup
# and cargo homes let tests that run rustc or cargo find the pinned toolchain.
PASS_ENV = ("PATH", "LANG", "LC_ALL", "TZ", "RUST_BACKTRACE", "RUST_LOG",
            "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_HOME")


def package_names(metadata: str) -> dict[str, str]:
    """Manifest path -> package name, from `cargo metadata --no-deps`."""
    return {p["manifest_path"]: p["name"] for p in json.loads(metadata)["packages"]}


def target_label(target: dict) -> str:
    return "lib" if "lib" in target.get("kind", ()) else target.get("name", "")


def test_binaries(cargo_output: str, names: dict[str, str],
                  select: dict[str, frozenset[str] | None]) -> list[tuple[str, str]]:
    """(executable, package manifest dir) of each selected test artifact, in order."""
    found = []
    for line in cargo_output.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("reason") != "compiler-artifact" or not msg.get("profile", {}).get("test"):
            continue
        exe = msg.get("executable")
        package = names.get(msg.get("manifest_path", ""))
        if not exe or package not in select:
            continue
        targets = select[package]
        if targets is not None and target_label(msg.get("target", {})) not in targets:
            continue
        found.append((exe, str(Path(msg["manifest_path"]).parent)))
    return found


def child_env(home: str, tmp: str) -> dict[str, str]:
    env = {k: os.environ[k] for k in PASS_ENV if k in os.environ}
    env.update(HOME=home, TMPDIR=tmp, USER="ci", LOGNAME="ci")
    return env


def private_dirs(root: str, n: int, uid: int) -> tuple[str, str]:
    """A fresh HOME and TMPDIR under ROOT for binary N, owned by UID, mode 0700."""
    dirs = []
    for kind in ("home", "tmp"):
        d = os.path.join(root, f"{n}-{kind}")
        os.mkdir(d, 0o700)
        os.chown(d, uid, uid)
        dirs.append(d)
    return dirs[0], dirs[1]


def setpriv(uid: int) -> list[str]:
    return ["setpriv", f"--reuid={uid}", f"--regid={uid}", "--clear-groups", "--"]


def run() -> int:
    if os.geteuid() != 0:
        print("unprivileged tests: skipped, not root (the main run is already unprivileged)")
        return 0
    meta = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                          stdout=subprocess.PIPE, text=True, check=False)
    if meta.returncode:
        return meta.returncode
    cmd = ["cargo", "test", "--workspace", "--no-run",
           "--message-format=json-render-diagnostics"]
    build = subprocess.run(cmd, stdout=subprocess.PIPE, text=True, check=False)
    if build.returncode:
        return build.returncode
    bins = test_binaries(build.stdout, package_names(meta.stdout), SELECT)
    if not bins:
        print("unprivileged tests: cargo listed no selected test binaries", file=sys.stderr)
        return 1
    results = os.environ.get("RESULTS_DIR")
    report = None
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        report = Path(results, "rust-unprivileged.json").open("w", encoding="utf-8")
    # Root-owned, 0711: uid UID can enter it but not list it or add to it.
    root = tempfile.mkdtemp(prefix="terrarium-ci-unpriv-")
    os.chmod(root, 0o711)
    failed = []
    try:
        args = []
        if report:
            args = ["-Z", "unstable-options", "--format", "json", "--report-time"]
        for n, (exe, cwd) in enumerate(bins):
            env = child_env(*private_dirs(root, n, UID))
            if report:
                env["RUSTC_BOOTSTRAP"] = "1"
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
        shutil.rmtree(root, ignore_errors=True)
    if failed:
        print(f"unprivileged tests: {len(failed)} binaries failed as uid {UID}:", file=sys.stderr)
        for exe in failed:
            print(f"  {exe}", file=sys.stderr)
        return 1
    print(f"unprivileged tests: {len(bins)} binaries passed as uid {UID}")
    return 0


if __name__ == "__main__":
    sys.exit(run())

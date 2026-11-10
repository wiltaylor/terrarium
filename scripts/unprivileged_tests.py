"""Run selected test binaries again as an unprivileged user (TER-579, TER-611).

Merge jobs run as root, the release CI as uid 1001: tests that depend on the
user passed in the merge suite and failed in the release CI (TER-494,
TER-575). This step reuses the test binaries that the root run of `ci::test`
built (`cargo test --workspace --no-run`, the same command and env, so no
rebuild), keeps those that SELECT names, and runs each of them as UID with
`setpriv`, each with its own private HOME, all binaries at once. TMPDIR is
not set: the tests use the shared, sticky, root-owned /tmp like the release
CI, which is what TER-575 failed on (a private TMPDIR hid that failure).
Doctests are not run. Without root it skips: the main run is then already
unprivileged. As root it fails early when UID has no passwd entry (OpenSSH
needs one); the host needs the user (useradd, see the message).
"""

from __future__ import annotations

import json
import os
import pwd
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


def child_env(home: str) -> dict[str, str]:
    env = {k: os.environ[k] for k in PASS_ENV if k in os.environ}
    env.update(HOME=home, USER="ci", LOGNAME="ci")
    return env


def private_home(root: str, n: int, uid: int) -> str:
    """A fresh HOME under ROOT for binary N, owned by UID, mode 0700."""
    d = os.path.join(root, f"{n}h")
    os.mkdir(d, 0o700)
    os.chown(d, uid, uid)
    return d


def has_user(uid: int) -> bool:
    try:
        pwd.getpwuid(uid)
    except KeyError:
        return False
    return True


def setpriv(uid: int) -> list[str]:
    return ["setpriv", f"--reuid={uid}", f"--regid={uid}", "--clear-groups", "--"]


def run() -> int:
    if os.geteuid() != 0:
        print("unprivileged tests: skipped, not root (the main run is already unprivileged)")
        return 0
    if not has_user(UID):
        print(f"unprivileged tests: uid {UID} has no passwd entry; OpenSSH and other tests need"
              f" one. Add it on this host: useradd -u {UID} -M -s /usr/sbin/nologin"
              " terrarium-ci", file=sys.stderr)
        return 1
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
    root = tempfile.mkdtemp(prefix="terrarium-ci-home-")
    os.chmod(root, 0o711)
    failed = []
    running = []
    try:
        args = []
        if report:
            args = ["-Z", "unstable-options", "--format", "json", "--report-time"]
        # The binaries run at the same time, each into its own log; the logs
        # are then printed (and reported) one after another, in cargo's order.
        for n, (exe, cwd) in enumerate(bins):
            env = child_env(private_home(root, n, UID))
            if report:
                env["RUSTC_BOOTSTRAP"] = "1"
            log = open(os.path.join(root, f"{n}.log"), "w+", encoding="utf-8",
                       errors="replace")
            running.append((exe, log, subprocess.Popen(
                setpriv(UID) + [exe, *args], cwd=cwd, env=env, stdout=log)))
        for exe, log, child in running:
            code = child.wait()
            log.seek(0)
            print(f"unprivileged tests: uid {UID}: {exe}", flush=True)
            for line in log:
                print(line, end="", flush=True)
                if report:
                    report.write(line)
            if code:
                failed.append(exe)
    finally:
        for _, log, child in running:
            if child.poll() is None:
                child.kill()
                child.wait()
            log.close()
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

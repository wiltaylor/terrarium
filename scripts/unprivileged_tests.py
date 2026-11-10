"""Run selected test binaries again as an unprivileged user (TER-579, TER-611).

Merge jobs run as root, the release CI as uid 1001: tests that depend on the
user passed in the merge suite and failed in the release CI (TER-494,
TER-575). This step reuses the test binaries that the root run of `ci::test`
built (`cargo test --workspace --no-run`, the same command and env, so no
rebuild), keeps those that SELECT names, and runs each of them as UID with
`setpriv`, each with its own private HOME, at most WIDTH at a time and the
heavy terrariumd binaries (ALONE) on their own. TMPDIR is
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

import rerun_tests

# Package -> the test targets to run ("lib" for the unit tests), None for all.
SELECT: dict[str, frozenset[str] | None] = {
    "terrarium-host": None,
    "terrariumd": frozenset({"lib", "it"}),
}
UID = 1001
# The merge job runs this on a loaded host: timing tests of these binaries
# failed when another binary ran beside them (TER-611 review). They run alone.
ALONE = frozenset({("terrariumd", "lib"), ("terrariumd", "it")})
# The other binaries run at most this many at a time.
WIDTH = 2
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
                  select: dict[str, frozenset[str] | None]) -> list[tuple[str, str, bool]]:
    """(executable, package manifest dir, runs alone) of each selected test artifact, in order."""
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
        label = target_label(msg.get("target", {}))
        targets = select[package]
        if targets is not None and label not in targets:
            continue
        found.append((exe, str(Path(msg["manifest_path"]).parent), (package, label) in ALONE))
    return found


def groups(bins: list[tuple[str, str, bool]], width: int = WIDTH) -> list[list[int]]:
    """The indexes of BINS that run at the same time, group by group, in order.

    A binary that runs alone is a group of its own; the others fill groups of
    at most WIDTH, in cargo's order.
    """
    out: list[list[int]] = []
    shared: list[int] = []
    for n, (_, _, alone) in enumerate(bins):
        if alone:
            out.append([n])
            continue
        if not shared or len(shared) == width:
            shared = []
            out.append(shared)
        shared.append(n)
    return out


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


def rerun(failed: list[tuple[str, str, rerun_tests.Outcomes]], root: str, first_home: int,
          args: list[str], results: str | None) -> bool:
    """Run the failed tests of each failed binary once more, as UID (TER-683).

    Each binary runs again with only its failed tests, from the same
    directory, with the same env and a fresh private HOME. True when every
    rerun test passed: the step then passes.
    """
    step = "test-unprivileged"
    first = rerun_tests.Outcomes()
    for _, _, seen in failed:
        first.started += seen.started
        first.finished += seen.finished
        first.failed.update(seen.failed)
    why = rerun_tests.why_not(first)
    if why is None:
        why = next((w for _, _, seen in failed if (w := rerun_tests.why_not(seen))), None)
    if why:
        print(f"rerun: {step}: no rerun: {why}", flush=True)
        return False
    names = sorted(first.failed)
    print(f"rerun: {step}: {len(names)} failed tests run once more as uid {UID}:"
          f" {', '.join(names)}", flush=True)
    again = rerun_tests.Outcomes()
    worst = 0
    out = Path(results, "rust-unprivileged-rerun.json").open("w", encoding="utf-8") if results else None
    try:
        for n, (exe, cwd, seen) in enumerate(failed):
            env = child_env(private_home(root, first_home + n, UID))
            if out:
                env["RUSTC_BOOTSTRAP"] = "1"
            cmd = setpriv(UID) + [exe, *args, *sorted(seen.failed), "--exact"]
            print(f"unprivileged tests: uid {UID}: rerun: {exe}", flush=True)
            child = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, text=True,
                                   encoding="utf-8", errors="replace", check=False)
            for line in child.stdout.splitlines(keepends=True):
                print(line, end="", flush=True)
                again.feed(line)
                if out:
                    out.write(line)
            again.done()
            worst = worst or child.returncode
    finally:
        if out:
            out.close()
    return rerun_tests.report(step, first, names, again, worst)


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
    # (exe, cwd, results) of each failed binary, for the rerun (TER-683).
    outcomes = []
    running = []
    try:
        args = []
        if report:
            args = ["-Z", "unstable-options", "--format", "json", "--report-time"]
        # The binaries of a group run at the same time, each into its own log;
        # the logs are then printed (and reported) one after another.
        for group in groups(bins):
            running = []
            for n in group:
                exe, cwd, _ = bins[n]
                env = child_env(private_home(root, n, UID))
                if report:
                    env["RUSTC_BOOTSTRAP"] = "1"
                log = open(os.path.join(root, f"{n}.log"), "w+", encoding="utf-8",
                           errors="replace")
                running.append((exe, log, subprocess.Popen(
                    setpriv(UID) + [exe, *args], cwd=cwd, env=env, stdout=log)))
            for n, (exe, log, child) in zip(group, running):
                code = child.wait()
                log.seek(0)
                print(f"unprivileged tests: uid {UID}: {exe}", flush=True)
                seen = rerun_tests.Outcomes()
                for line in log:
                    print(line, end="", flush=True)
                    seen.feed(line)
                    if report:
                        report.write(line)
                log.close()
                if code:
                    failed.append(exe)
                    outcomes.append((exe, bins[n][1], seen.done()))
        running = []
        if failed and rerun_tests.enabled() and rerun(outcomes, root, len(bins), args, results):
            failed = []
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

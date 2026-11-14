"""Stream CI Rust results, with an opt-in nextest/JUnit runner."""

from __future__ import annotations

import os
import json
from pathlib import Path
import shutil
import subprocess
import sys
import threading

import rerun_tests


def reported(cmd: list[str], results: str, env: dict[str, str], filename: str = "doctests.json",
             outcomes: rerun_tests.Outcomes | None = None) -> int:
    """Tee this invocation, not previous appended results, and keep its status."""
    with Path(results, filename).open("a", encoding="utf-8") as out:
        return teed(cmd, env, out, outcomes)


def teed(cmd: list[str], env: dict[str, str] | None, out=None,
         outcomes: rerun_tests.Outcomes | None = None) -> int:
    """Run `cmd`, print its stdout as it comes, and copy it to `out` and `outcomes`.

    With `outcomes`, cargo's stderr goes through too (printed to stderr as
    it comes): it names each test run and each failure (TER-683).
    """
    with subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, encoding="utf-8",
                          errors="replace", env=env,
                          stderr=subprocess.PIPE if outcomes is not None else None) as child:
        assert child.stdout is not None
        errors = None
        if outcomes is not None:
            errors = threading.Thread(target=cargo_stderr, args=(child.stderr, outcomes))
            errors.start()
        for line in child.stdout:
            if out is not None:
                out.write(line)
                out.flush()
            if outcomes is not None:
                outcomes.feed(line)
            print(line, end="", flush=True)
        code = child.wait()
        if errors is not None:
            errors.join()
        return code


def cargo_stderr(stream, outcomes: rerun_tests.Outcomes) -> None:
    for line in stream:
        print(line, end="", file=sys.stderr, flush=True)
        outcomes.cargo(line)


def junit_source() -> Path:
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], text=True))
    # nextest's store.dir defaults to workspace-root/target/nextest, even
    # when Cargo's compilation artifacts use a separate CARGO_TARGET_DIR.
    return Path(meta["workspace_root"], "target", "nextest", "ci", "junit.xml")


def copy_junit(results: str, source: Path) -> None:
    if source.exists():
        dest = Path(results, "rust.xml")
        index = 2
        while dest.exists():
            dest = Path(results, f"rust-{index}.xml")
            index += 1
        shutil.copyfile(source, dest)


def run_nextest(args: list[str]) -> int:
    code = subprocess.call(["just", "ci::install-nextest"])
    if code:
        return code
    cmd = ["cargo", "nextest", "run", "--profile", "ci", *args]
    results = os.environ.get("RESULTS_DIR")
    source = junit_source() if results else None
    if source is not None:
        source.unlink(missing_ok=True)
    # nextest streams status and panic output normally while writing JUnit.
    code = subprocess.call(cmd)
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        assert source is not None
        copy_junit(results, source)
    # Target-specific cargo test never ran doctests. Whole-crate runs did.
    if not any(a in args for a in ("--lib", "--bins", "--test", "--bin", "--tests", "--all-targets")):
        doc = ["cargo", "test", "--doc", *args]
        if results:
            doc_code = reported(
                doc + ["--", "-Z", "unstable-options", "--format", "json", "--report-time"],
                results, dict(os.environ, RUSTC_BOOTSTRAP="1"))
        else:
            doc_code = subprocess.call(doc)
        code = code or doc_code
    return code


JSON = ["-Z", "unstable-options", "--format", "json", "--report-time"]
# Cargo arguments that select test targets: the run then has no doctests.
TARGETS = ("--lib", "--bins", "--test", "--bin", "--tests", "--all-targets", "--doc",
           "--example", "--examples", "--bench", "--benches")


def split(cmd: list[str]) -> tuple[list[str], list[str]]:
    """The cargo part of `cmd` and the libtest part after its first `--`."""
    if "--" not in cmd:
        return cmd, []
    i = cmd.index("--")
    return cmd[:i], cmd[i + 1:]


def libtest(cmd: list[str], extra: list[str]) -> list[str]:
    """`cmd` with `extra` put before its own libtest arguments, behind one `--`.

    A second `--` would make libtest read `extra` as test name filters: with
    `-- webhooks::`, the JSON options added `json` as a filter and the run
    took every test with `json` in its name (TER-1261).
    """
    cargo, rest = split(cmd)
    return [*cargo, "--", *extra, *rest]


def run(args: list[str]) -> int:
    if args and args[0] == "--nextest":
        return run_nextest(args[1:])
    cmd = ["cargo", "test", *args]
    results = os.environ.get("RESULTS_DIR")
    first = rerun_tests.Outcomes() if rerun_tests.enabled() else None
    if not results and first is None:
        return subprocess.call(cmd)
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        code = reported(libtest(cmd, JSON), results, dict(os.environ, RUSTC_BOOTSTRAP="1"),
                        "rust.json", first)
    else:
        code = teed(cmd, None, None, first)
    if code == 0 or first is None:
        return code
    return rerun_failed(cmd, results, first.done(), code)


def rerun_failed(cmd: list[str], results: str | None, first: rerun_tests.Outcomes,
                 code: int) -> int:
    """Run the failed tests once more (TER-683, rerun_tests.py).

    Only cargo's own test-failure status (101) may lead to a rerun: a signal
    (a negative status) or any other status stays red, whatever the output.
    """
    if code != rerun_tests.LIBTEST_FAILED:
        what = f"signal {-code}" if code < 0 else f"exit code {code}"
        rerun_tests.refuse("test", first, f"cargo itself ended with {what}, not 101")
        return code
    why = rerun_tests.why_not(first)
    if why:
        rerun_tests.refuse("test", first, why)
        return code
    names = sorted(first.failed)
    print(f"rerun: test: {len(names)} failed tests run once more: {', '.join(names)}", flush=True)
    # The same command, so the same build and env. Without a target
    # selection `--tests` leaves out the doctests: none of them failed.
    cargo, rest = split(cmd)
    again = cmd if any(a in cargo for a in TARGETS) else libtest([*cargo, "--tests"], rest)
    tests = [*names, "--exact"]
    out = rerun_tests.Outcomes()
    if results:
        Path(results, "rust-rerun.json").write_text("", encoding="utf-8")
        rc = reported(libtest(again, [*JSON, *tests]), results, dict(os.environ, RUSTC_BOOTSTRAP="1"),
                      "rust-rerun.json", out)
    else:
        rc = teed(libtest(again, tests), None, None, out)
    return 0 if rerun_tests.report("test", first, names, out.done(), rc) else code


if __name__ == "__main__":
    sys.exit(run(sys.argv[1:]))

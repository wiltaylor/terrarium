"""Stream CI Rust results, with an opt-in nextest/JUnit runner."""

from __future__ import annotations

import os
import json
from pathlib import Path
import shutil
import subprocess
import sys


def reported(cmd: list[str], results: str, env: dict[str, str], filename: str = "doctests.json") -> int:
    """Tee this invocation, not previous appended results, and keep its status."""
    with Path(results, filename).open("a", encoding="utf-8") as out:
        with subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, encoding="utf-8",
                              errors="replace", env=env) as child:
            assert child.stdout is not None
            for line in child.stdout:
                out.write(line)
                out.flush()
                print(line, end="", flush=True)
            return child.wait()


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


def run(args: list[str]) -> int:
    if args and args[0] == "--nextest":
        return run_nextest(args[1:])
    cmd = ["cargo", "test", *args]
    results = os.environ.get("RESULTS_DIR")
    if not results:
        return subprocess.call(cmd)
    Path(results).mkdir(parents=True, exist_ok=True)
    return reported(cmd + ["--", "-Z", "unstable-options", "--format", "json", "--report-time"],
                    results, dict(os.environ, RUSTC_BOOTSTRAP="1"), "rust.json")


if __name__ == "__main__":
    sys.exit(run(sys.argv[1:]))

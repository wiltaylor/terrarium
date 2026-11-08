"""Run CI's Rust tests with nextest JUnit and separate doctest JSON."""

from __future__ import annotations

import os
import json
from pathlib import Path
import shutil
import subprocess
import sys


def reported(cmd: list[str], results: str, env: dict[str, str]) -> int:
    """Tee this invocation, not previous appended results, and keep its status."""
    with Path(results, "doctests.json").open("a", encoding="utf-8") as out:
        with subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, encoding="utf-8",
                              errors="replace", env=env) as child:
            assert child.stdout is not None
            for line in child.stdout:
                out.write(line)
                out.flush()
                print(line, end="", flush=True)
            return child.wait()


def copy_junit(results: str) -> None:
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], text=True))
    source = Path(meta["target_directory"], "nextest", "ci", "junit.xml")
    if source.exists():
        dest = Path(results, "rust.xml")
        index = 2
        while dest.exists():
            dest = Path(results, f"rust-{index}.xml")
            index += 1
        shutil.copyfile(source, dest)


def run(args: list[str]) -> int:
    code = subprocess.call(["just", "ci::install-nextest"])
    if code:
        return code
    cmd = ["cargo", "nextest", "run", "--profile", "ci", *args]
    results = os.environ.get("RESULTS_DIR")
    # nextest streams status and panic output normally while writing JUnit.
    code = subprocess.call(cmd)
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        copy_junit(results)
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


if __name__ == "__main__":
    sys.exit(run(sys.argv[1:]))

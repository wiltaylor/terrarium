"""Run CI's Rust tests with nextest, retaining libtest JSON and doctests."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys


def reported(cmd: list[str], results: str, env: dict[str, str]) -> int:
    """Tee this invocation, not previous appended results, and keep its status."""
    with Path(results, "rust.json").open("a", encoding="utf-8") as out:
        with subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, encoding="utf-8",
                              errors="replace", env=env) as child:
            assert child.stdout is not None
            for line in child.stdout:
                out.write(line)
                out.flush()
                print(line, end="", flush=True)
            return child.wait()


def run(args: list[str]) -> int:
    code = subprocess.call(["just", "ci::install-nextest"])
    if code:
        return code
    cmd = ["cargo", "nextest", "run", "--profile", "ci", *args]
    results = os.environ.get("RESULTS_DIR")
    env = dict(os.environ, NEXTEST_EXPERIMENTAL_LIBTEST_JSON="1")
    if results:
        Path(results).mkdir(parents=True, exist_ok=True)
        code = reported(cmd + ["--message-format", "libtest-json"], results, env)
    else:
        code = subprocess.call(cmd)
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

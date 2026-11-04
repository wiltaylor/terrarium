"""Print one GitHub `::error` annotation for each failed test in a cargo test log.

libtest writes no annotations, and the log of a GitHub job needs a token to
read. The annotations of a check run are public
(`GET /repos/<owner>/<repo>/check-runs/<job id>/annotations`), so the CI test
step runs this on failure: the names of the failed tests can then be read
without a token.

GitHub keeps at most 10 error annotations per step. With more failures, the
first 9 get one annotation each, and the 10th lists the rest.

Usage: python3 scripts/annotate_test_failures.py <cargo test log>
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

# GitHub shows at most this many error annotations of one step.
MAX_ANNOTATIONS = 10
# The panic message in an annotation: this many lines, each cut to LINE_CHARS.
MESSAGE_LINES = 3
LINE_CHARS = 200

# `     Running tests/lifecycle.rs (target/debug/deps/lifecycle-0123abcd)`
# `     Running unittests src/lib.rs (target/debug/deps/terrariumd-0123abcd)`
RUNNING = re.compile(r"^\s+Running (?:unittests )?(\S+) \((?:.*/)?([^/()]+?)(?:-[0-9a-f]{16})?(?:\.exe)?\)\s*$")
# `   Doc-tests terrarium_core`
DOCTESTS = re.compile(r"^\s+Doc-tests (\S+)\s*$")
# `test repo_browse::tests::a_race ... FAILED`
FAILED = re.compile(r"^test (.+?) \.\.\. FAILED\s*$")
# `---- repo_browse::tests::a_race stdout ----`
STDOUT = re.compile(r"^---- (.+?) stdout ----\s*$")
# The colour codes of CARGO_TERM_COLOR=always.
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# `thread 'name' (1234) panicked at crates/x/src/y.rs:10:5:`
PANIC = re.compile(r"^thread '.*?'(?: \(\d+\))? panicked at (.+?):(\d+):\d+:\s*$")


@dataclass
class Failure:
    binary: str
    name: str
    file: str | None = None
    line: int | None = None
    message: str = ""


def parse(text: str) -> list[Failure]:
    """The failed tests of a cargo test log, in the order of the log."""
    binary = "cargo test"
    failures: list[Failure] = []
    # (binary, name) -> failure, for the panic details in the `failures:` part.
    by_key: dict[tuple[str, str], Failure] = {}
    current: Failure | None = None
    lines = ANSI.sub("", text).splitlines()
    for i, raw in enumerate(lines):
        line = raw.rstrip("\r")
        if m := RUNNING.match(line):
            binary = f"{m.group(2)} ({m.group(1)})"
            current = None
            continue
        if m := DOCTESTS.match(line):
            binary = f"{m.group(1)} (doc-tests)"
            current = None
            continue
        if m := FAILED.match(line):
            key = (binary, m.group(1))
            if key not in by_key:
                f = Failure(binary, m.group(1))
                by_key[key] = f
                failures.append(f)
            continue
        if m := STDOUT.match(line):
            current = by_key.get((binary, m.group(1)))
            continue
        if current is not None and current.file is None and (m := PANIC.match(line)):
            current.file, current.line = m.group(1), int(m.group(2))
            current.message = panic_message(lines[i + 1 :])
    return failures


def panic_message(after: list[str]) -> str:
    """The first lines of a panic message (`left`/`right` of an assertion),
    each cut to [`LINE_CHARS`]."""
    out: list[str] = []
    for line in after:
        line = line.strip()
        if not line:
            if out:
                break
            continue
        if line.startswith("note: run with") or STDOUT.match(line):
            break
        out.append(line if len(line) <= LINE_CHARS else line[: LINE_CHARS - 3] + "...")
        if len(out) == MESSAGE_LINES:
            break
    return "\n".join(out)


def escape_data(s: str) -> str:
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_property(s: str) -> str:
    return escape_data(s).replace(":", "%3A").replace(",", "%2C")


def annotation(title: str, message: str, file: str | None = None, line: int | None = None) -> str:
    props = []
    if file:
        props.append(f"file={escape_property(file)}")
        if line:
            props.append(f"line={line}")
    props.append(f"title={escape_property(title)}")
    return f"::error {','.join(props)}::{escape_data(message)}"


def annotations(failures: list[Failure]) -> list[str]:
    """The annotation lines for `failures`: at most [`MAX_ANNOTATIONS`]."""
    out = []
    one_each = failures if len(failures) <= MAX_ANNOTATIONS else failures[: MAX_ANNOTATIONS - 1]
    for f in one_each:
        message = f"{f.name} FAILED"
        if f.message:
            message += f"\n{f.message}"
        out.append(annotation(f.binary, message, f.file, f.line))
    rest = failures[len(one_each) :]
    if rest:
        names = "\n".join(f"{f.binary}: {f.name} FAILED" for f in rest)
        out.append(annotation(f"{len(rest)} more failed tests", names))
    return out


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    text = Path(argv[1]).read_text(errors="replace")
    failures = parse(text)
    for line in annotations(failures):
        print(line)
    print(f"{len(failures)} failed tests", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

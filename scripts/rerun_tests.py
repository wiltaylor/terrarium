"""Rerun the failed tests of a merge job once (TER-683).

A merge job runs the whole suite on a loaded host, and one flaky test made
the whole batch red. When a test step of a merge job (`TERRARIUM_JOB_KIND`
is `merge`) fails, the step reads the names of the failed tests from the
libtest output (the plain console output or the libtest JSON) and runs
exactly those tests once more: the same binaries, the same env, the same
uid. All pass on the rerun: the step passes, and each test that passed on
the rerun is reported to the daemon, which files a flaky-test card for it.
One fails again: the step fails as before.

There is no rerun when the failure is not explained by failed tests: a
build error, a test binary that ended without a result (a crash, a
signal), a failed doctest, or more than MAX_RERUN failed tests.

Each rerun test gets one MARKER line on stdout, with JSON after it: the
step, the test, the first and the second result, and the tail of its first
failure. The daemon reads these lines from the job log (docs/api.md, merge
jobs).
"""

from __future__ import annotations

import json
import os
import re

# The most failed tests that are rerun; more is no flake, no rerun.
MAX_RERUN = 5
# The start of the line that names one rerun test for the daemon.
MARKER = "[terrarium rerun] "
# The bytes of the first failure that a marker line keeps (its end).
TAIL = 1500

RUNNING = re.compile(r"^running \d+ tests?$")
RESULT = re.compile(r"^test result: (ok|FAILED)\. ")
TEST = re.compile(r"^test (.+?) \.\.\. (ok|FAILED)$")
SECTION = re.compile(r"^---- (.+?) std(?:out|err) ----$")
# `src/lib.rs - item (line 12)`: a doctest, which cannot be run alone.
DOCTEST = re.compile(r" - .*\(line \d+\)$")


def enabled() -> bool:
    """Only merge jobs rerun: a card check shows each failure as it is."""
    return os.environ.get("TERRARIUM_JOB_KIND") == "merge"


class Outcomes:
    """The results that libtest printed, plain or JSON, line by line."""

    def __init__(self) -> None:
        self.started = 0
        self.finished = 0
        self.failed: dict[str, str] = {}
        self.passed: set[str] = set()
        self._section: str | None = None
        self._lines: list[str] = []

    def feed(self, line: str) -> None:
        text = line.rstrip("\r\n")
        if text.startswith("{"):
            try:
                event = json.loads(text)
            except ValueError:
                event = None
            if isinstance(event, dict):
                self._event(event)
                return
        self._plain(text)

    def _event(self, e: dict) -> None:
        kind, event = e.get("type"), e.get("event")
        if kind == "suite":
            if event == "started":
                self.started += 1
            elif event in ("ok", "failed"):
                self.finished += 1
        elif kind == "test" and isinstance(e.get("name"), str):
            if event == "failed":
                self.failed[e["name"]] = str(e.get("stdout") or "")
            elif event == "ok":
                self.passed.add(e["name"])

    def _plain(self, text: str) -> None:
        if RUNNING.match(text):
            self._close()
            self.started += 1
        elif RESULT.match(text):
            self._close()
            self.finished += 1
        elif m := TEST.match(text):
            if m.group(2) == "ok":
                self.passed.add(m.group(1))
            else:
                self.failed.setdefault(m.group(1), "")
        elif m := SECTION.match(text):
            self._close()
            self._section = m.group(1)
        elif text == "failures:":
            self._close()
        elif self._section is not None:
            self._lines.append(text)

    def _close(self) -> None:
        """End the captured output of a failed test (plain output)."""
        if self._section is not None and self._section in self.failed:
            self.failed[self._section] += "\n".join(self._lines).strip() + "\n"
        self._section = None
        self._lines = []

    def done(self) -> "Outcomes":
        self._close()
        return self


def why_not(o: Outcomes) -> str | None:
    """Why the failed run `o` gets no rerun, or None when it gets one."""
    if not o.failed:
        return "no test failed (a build error, or a test binary that failed without a test result)"
    if o.started != o.finished:
        return "a test binary ended without a result (a crash or a signal)"
    if any(DOCTEST.search(n) for n in o.failed):
        return "a doctest failed"
    if len(o.failed) > MAX_RERUN:
        return f"{len(o.failed)} tests failed, more than {MAX_RERUN}"
    return None


def result(name: str, again: Outcomes) -> str:
    if name in again.failed:
        return "failed"
    if name in again.passed:
        return "ok"
    return "missing"


def green(names: list[str], again: Outcomes, code: int) -> bool:
    """Did the rerun pass: every rerun test ran and passed, nothing failed."""
    return (code == 0 and not again.failed and again.started == again.finished
            and all(result(n, again) == "ok" for n in names))


def clip(text: str) -> str:
    data = text.encode("utf-8", "replace")
    if len(data) <= TAIL:
        return text
    return data[-TAIL:].decode("utf-8", "ignore")


def report(step: str, first: Outcomes, names: list[str], again: Outcomes, code: int) -> bool:
    """Print the rerun of `names` for people and for the daemon; True when green."""
    ok = green(names, again, code)
    for name in names:
        print(MARKER + json.dumps({
            "step": step, "test": name, "first": "failed",
            "rerun": result(name, again), "tail": clip(first.failed.get(name, "")),
        }, sort_keys=True), flush=True)
    if ok:
        print(f"rerun: {step}: all {len(names)} failed tests passed on the rerun, the step"
              f" passes; flaky: {', '.join(names)}", flush=True)
    else:
        bad = [f"{n} ({result(n, again)})" for n in names if result(n, again) != "ok"]
        what = ", ".join(bad) or f"the rerun itself, exit code {code}"
        print(f"rerun: {step}: failed again: {what}; the step fails", flush=True)
    return ok

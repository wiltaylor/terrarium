"""Rerun the failed tests of the full suite once (TER-683).

The full suite runs on a loaded host, and one flaky test made the whole run
red. In a release validation (`TERRARIUM_VALIDATE_CONTEXT=release`, the
cut run) or a merge job (`TERRARIUM_JOB_KIND=merge`), when a test step
fails, the step reads the names of the failed tests from the libtest
output (the plain console output or the libtest JSON) and runs exactly
those tests once more: the same binaries, the same env, the same uid. All
pass on the rerun: the step passes. One fails again: the step fails as
before. A card check never reruns: there each failure shows as it is.

There is no rerun unless every failure is a failed test (`why_not`): a
build error, any other cargo error, a test binary that ended without a
result or before it printed one (a crash, a signal, a panic before the
tests), an exit code other than libtest's 101, a failed doctest, or more
than MAX_RERUN failed tests keeps the step red at once.

With `RESULTS_DIR` set, each step that failed writes its entry to
`$RESULTS_DIR/reruns.json` (`record`): the step, its outcome, and each
failed test with its first and its rerun result. The file comes from the
bookkeeping of this script only: no line of the test output is copied into
it as a result (a test can print anything). `scripts/flaky_cards.py`
prints the card commands for the tests that passed on the rerun.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import re

# The most failed tests that are rerun; more is no flake, no rerun.
MAX_RERUN = 5
# The bytes of the first failure that reruns.json keeps (its end).
TAIL = 1500
# libtest exits with this code when a test failed.
LIBTEST_FAILED = 101
# The file of the rerun results in RESULTS_DIR.
REPORT = "reruns.json"

RUNNING = re.compile(r"^running \d+ tests?$")
RESULT = re.compile(r"^test result: (ok|FAILED)\. ")
TEST = re.compile(r"^test (.+?) \.\.\. (ok|FAILED)$")
SECTION = re.compile(r"^---- (.+?) std(?:out|err) ----$")
# `src/lib.rs - item (line 12)`: a doctest, which cannot be run alone.
DOCTEST = re.compile(r" - .*\(line \d+\)$")
# Cargo's stderr: each test binary and doctest run it starts.
CARGO_RUN = re.compile(r"^\s+(Running|Doc-tests) \S")
# Cargo's stderr: a test binary or a doctest run that failed.
CARGO_FAILED = re.compile(r"^error: (?:test|doctest) failed, to rerun pass `[^`]*`$")
# Cargo's stderr: the summary of the failed targets, and its list.
CARGO_SUMMARY = re.compile(r"^error: \d+ targets? failed:$")
CARGO_STATUS = re.compile(r"process didn't exit successfully: .*\((exit status: (\d+)|signal: [^)]*)\)\s*$")


def context() -> str | None:
    """The run that reruns: `release` (the cut), `merge`, or None (no rerun)."""
    if os.environ.get("TERRARIUM_VALIDATE_CONTEXT") == "release":
        return "release"
    if os.environ.get("TERRARIUM_JOB_KIND") == "merge":
        return "merge"
    return None


def enabled() -> bool:
    return context() is not None


class Outcomes:
    """The results of one test step: libtest's stdout and the test runs.

    `feed` takes the lines of libtest (plain or JSON). The runs of the test
    binaries come from cargo's stderr (`cargo`) or from the caller that
    starts each binary itself (`ended`). Any end that is not a failed test
    goes to `unexplained`.
    """

    def __init__(self) -> None:
        self.started = 0
        self.finished = 0
        self.failed_suites = 0
        self.failed: dict[str, str] = {}
        self.passed: set[str] = set()
        # The test runs: started, and failed as a failed test does.
        self.tracked = False
        self.runs = 0
        self.failed_runs = 0
        self.unexplained: list[str] = []
        self._section: str | None = None
        self._lines: list[str] = []
        self._summary = False

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
                self.failed_suites += event == "failed"
        elif kind == "test" and isinstance(e.get("name"), str):
            if event == "failed":
                self.failed[e["name"]] = str(e.get("stdout") or "")
            elif event == "ok":
                self.passed.add(e["name"])

    def _plain(self, text: str) -> None:
        if RUNNING.match(text):
            self._close()
            self.started += 1
        elif m := RESULT.match(text):
            self._close()
            self.finished += 1
            self.failed_suites += m.group(1) == "FAILED"
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

    def cargo(self, line: str) -> None:
        """One line of cargo's stderr."""
        self.tracked = True
        text = line.rstrip("\r\n")
        if CARGO_RUN.match(text):
            self.runs += 1
            self._summary = False
        elif CARGO_FAILED.match(text):
            self.failed_runs += 1
        elif CARGO_SUMMARY.match(text):
            self._summary = True
        elif self._summary and re.match(r"^\s+`[^`]*`$", text):
            pass
        elif m := CARGO_STATUS.search(text):
            if m.group(2) != str(LIBTEST_FAILED):
                self.unexplained.append(f"a test run ended with {m.group(1)}")
        elif text.startswith("error"):
            self.unexplained.append(f"cargo: {text[:200]}")

    def ended(self, code: int) -> None:
        """A test binary that the caller ran itself ended with `code`."""
        self.tracked = True
        self.runs += 1
        if code < 0:
            self.unexplained.append(f"a test binary ended with signal {-code}")
        elif code == LIBTEST_FAILED:
            self.failed_runs += 1
        elif code:
            self.unexplained.append(f"a test binary ended with exit code {code}")

    def _close(self) -> None:
        """End the captured output of a failed test (plain output)."""
        if self._section is not None and self._section in self.failed:
            self.failed[self._section] += "\n".join(self._lines).strip() + "\n"
        self._section = None
        self._lines = []

    def done(self) -> "Outcomes":
        self._close()
        return self

    def add(self, other: "Outcomes") -> None:
        """Add the results of another binary of the same step."""
        for name in ("started", "finished", "failed_suites", "runs", "failed_runs"):
            setattr(self, name, getattr(self, name) + getattr(other, name))
        self.tracked = self.tracked or other.tracked
        self.failed.update(other.failed)
        self.passed |= other.passed
        self.unexplained += other.unexplained


def why_not(o: Outcomes) -> str | None:
    """Why the failed step `o` gets no rerun, or None when every failure is a failed test."""
    if o.unexplained:
        return f"a failure that is not a failed test ({o.unexplained[0]})"
    if not o.tracked:
        return "the test runs were not tracked"
    if not o.failed:
        return "no test failed (a build error, or a test run that failed without a test result)"
    if o.runs != o.started or o.started != o.finished:
        return (f"a test run ended without a result (a crash, a signal, or a panic before the"
                f" tests): {o.runs} runs, {o.started} started, {o.finished} with a result")
    if o.failed_runs != o.failed_suites:
        return (f"{o.failed_runs} test runs failed, but {o.failed_suites} of them have a failed"
                f" test result")
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
    """Did the rerun pass: every rerun test ran and passed, nothing else failed."""
    return (code == 0 and not again.failed and not again.unexplained
            and again.started == again.finished
            and all(result(n, again) == "ok" for n in names))


def clip(text: str) -> str:
    data = text.encode("utf-8", "replace")
    if len(data) <= TAIL:
        return text
    return data[-TAIL:].decode("utf-8", "ignore")


def record(step: str, first: Outcomes, outcome: str, reason: str | None = None,
           names: list[str] | None = None, again: Outcomes | None = None) -> None:
    """Write the entry of `step` to RESULTS_DIR/reruns.json, replacing an older one.

    `outcome`: `green` (all passed on the rerun), `red` (one failed again)
    or `no_rerun` (with `reason`). Only the names that this script reran,
    with the result that its own bookkeeping gives each of them.
    """
    results = os.environ.get("RESULTS_DIR")
    if not results:
        return
    path = Path(results, REPORT)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        steps = [s for s in data["steps"] if isinstance(s, dict) and s.get("step") != step]
    except (OSError, ValueError, KeyError, TypeError):
        steps = []
    entry: dict = {"step": step, "outcome": outcome}
    if reason:
        entry["reason"] = reason
    entry["tests"] = [
        {"name": n, "first": "failed", "rerun": result(n, again) if again else None,
         "tail": clip(first.failed.get(n, ""))}
        for n in (names if names is not None else sorted(first.failed))
    ]
    steps.append(entry)
    Path(results).mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(REPORT + ".tmp")
    tmp.write_text(json.dumps({"version": 1, "context": context(), "steps": steps}, indent=2)
                   + "\n", encoding="utf-8")
    tmp.replace(path)


def report(step: str, first: Outcomes, names: list[str], again: Outcomes, code: int) -> bool:
    """Print and record the rerun of `names`; True when it is green."""
    ok = green(names, again, code)
    record(step, first, "green" if ok else "red", names=names, again=again)
    if ok:
        print(f"rerun: {step}: all {len(names)} failed tests passed on the rerun, the step"
              f" passes; flaky: {', '.join(names)}", flush=True)
    else:
        bad = [f"{n} ({result(n, again)})" for n in names if result(n, again) != "ok"]
        what = ", ".join(bad) or f"the rerun itself, exit code {code}"
        print(f"rerun: {step}: failed again: {what}; the step fails", flush=True)
    return ok


def refuse(step: str, first: Outcomes, why: str) -> None:
    """No rerun: print and record why."""
    print(f"rerun: {step}: no rerun: {why}", flush=True)
    record(step, first, "no_rerun", reason=why)

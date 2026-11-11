"""Print the flaky-test card commands for a reruns.json (TER-683).

`scripts/rerun_tests.py` writes `$RESULTS_DIR/reruns.json` in a release
validation (the cut run). Each test there that failed and then passed on
its rerun is a flaky test. This prints, for each of them, a shell snippet
that comments on the open card of that test (label `flaky-test:<hash>`,
not in `done`), or makes one in backlog with the labels `flaky`,
`flaky-test:<hash>`, `version:vnext` and `type:small`. It runs nothing:
the lead reads the output and runs it (`... | sh`).

Usage: python3 scripts/flaky_cards.py <reruns.json> [--source TEXT]
`--source` names the run in the card text, for example `the 0.43.0 cut`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import shlex
import sys

# The open card of a label: the first card of `card ls` that is not done.
OPEN_CARD = ("python3 -c 'import json,sys; print(next((c[\"id\"] for c in json.load(sys.stdin)"
             " if c.get(\"column\") != \"done\"), \"\"))'")


def label(test: str) -> str:
    """The label of the card of `test`: a hash, so any name fits a label."""
    return "flaky-test:" + hashlib.sha256(test.encode("utf-8")).hexdigest()[:16]


def flaky(data: dict) -> list[tuple[str, dict]]:
    """(step, test) of each test that failed and then passed on its rerun."""
    out = []
    for step in data.get("steps", []):
        for test in step.get("tests", []):
            if test.get("first") == "failed" and test.get("rerun") == "ok" and test.get("name"):
                out.append((str(step.get("step", "?")), test))
    return out


def indent(text: str) -> str:
    return "".join(f"    {line}\n" for line in text.splitlines())


def snippet(step: str, test: dict, source: str) -> str:
    name = str(test["name"])
    tag = label(name)
    note = (f"The test `{name}` failed in {source} (step {step}) and passed when it ran once"
            f" more (TER-683, reruns.json).\nThe first failure (end of its output):\n\n"
            f"{indent(str(test.get('tail') or ''))}")
    body = (f"{note}\nMake the test reliable, or find what it races with. A later run where it"
            f" flakes adds a comment here while this card is open.")
    title = f"Flaky test: {name}"[:200]
    return "\n".join([
        f"# {name} ({step})",
        f"open=$(terrarium card ls --label {shlex.quote(tag)} --json | {OPEN_CARD})",
        'if [ -n "$open" ]; then',
        f'  terrarium card comment "$open" -b {shlex.quote(note)}',
        "else",
        f"  terrarium card new {shlex.quote(title)} --label flaky --label {shlex.quote(tag)}"
        f" --label version:vnext --label type:small -b {shlex.quote(body)}",
        "fi",
        "",
    ])


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("reruns")
    ap.add_argument("--source", default="a release validation")
    a = ap.parse_args(argv)
    try:
        data = json.loads(Path(a.reruns).read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        print(f"flaky cards: cannot read {a.reruns}: {e}", file=sys.stderr)
        return 1
    tests = flaky(data)
    if not tests:
        print("# flaky cards: no test passed on a rerun")
        return 0
    print("set -e")
    for step, test in tests:
        print(snippet(step, test, a.source))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

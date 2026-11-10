"""Tests for scripts/release_version.py (TER-613).

Each case builds a throwaway git history and checks the version. While
.github/workflows/ci.yml exists, each case also runs the `calc` step of its
version job on the same history and checks that both give the same result.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts/release_version.py"
WORKFLOW = REPO / ".github/workflows/ci.yml"


def git(cwd, *args):
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )


def make_history(path, steps):
    """steps: ("tag", name) or (subject, [trailer, ...]) or (subject, [trailer, ...], body)."""
    git(path, "init", "-q", "-b", "main")
    git(path, "commit", "-q", "--allow-empty", "-m", "chore: init")
    for step in steps:
        if step[0] == "tag":
            git(path, "tag", step[1])
            continue
        subject, trailers = step[0], step[1]
        args = ["commit", "-q", "--allow-empty", "-m", subject]
        if len(step) > 2:
            args += ["-m", step[2]]
        if trailers:
            # One paragraph: git reads trailers from the last paragraph only.
            args += ["-m", "\n".join(trailers)]
        git(path, *args)


def workflow_calc_script() -> str | None:
    """The run script of the `calc` step of the version job, or None without ci.yml."""
    if not WORKFLOW.exists():
        return None
    lines = WORKFLOW.read_text().splitlines()
    start = lines.index("      - id: calc")
    run = lines.index("        run: |", start)
    body = []
    for line in lines[run + 1 :]:
        if line.strip() and not line.startswith("          "):
            break
        body.append(line)
    return textwrap.dedent("\n".join(body))


class ReleaseVersion(unittest.TestCase):
    def run_both(self, steps, expect_ok=True):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp, "repo")
            repo.mkdir()
            make_history(repo, steps)
            env = {"PATH": os.environ["PATH"], "HOME": tmp}
            proc = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), "--repo", str(repo)],
                env=env,
                capture_output=True,
                text=True,
            )
            script = workflow_calc_script()
            wf = None
            if script is not None:
                out = Path(tmp, "output")
                wf = subprocess.run(
                    ["bash", "-e", "-o", "pipefail", "-c", script],
                    cwd=repo,
                    env=dict(env, GITHUB_OUTPUT=str(out), GITHUB_STEP_SUMMARY=str(Path(tmp, "s"))),
                    capture_output=True,
                    text=True,
                )
            if not expect_ok:
                self.assertNotEqual(proc.returncode, 0, proc.stdout)
                if wf is not None:
                    self.assertNotEqual(wf.returncode, 0, wf.stdout)
                return None
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            values = dict(line.split("=", 1) for line in proc.stdout.splitlines())
            if wf is not None:
                self.assertEqual(wf.returncode, 0, wf.stdout + wf.stderr)
                theirs = dict(line.split("=", 1) for line in out.read_text().splitlines())
                mine = {k: v for k, v in values.items() if k in theirs}
                self.assertEqual(mine, theirs, "release_version.py differs from ci.yml")
            return values

    def test_no_trailer_no_release(self):
        values = self.run_both([("tag", "v0.5.0"), ("feat: a", [])])
        self.assertEqual(values, {"do_release": "false"})

    def test_release_after_alpha_promotes_it(self):
        values = self.run_both(
            [("tag", "v0.5.0-alpha"), ("fix: a", []), ("feat: b", ["release: true"])]
        )
        self.assertEqual(values["version"], "0.5.0")
        self.assertEqual(values["prerelease"], "false")
        self.assertEqual(values["do_release"], "true")
        self.assertEqual(values["bump"], "promoted from v0.5.0-alpha")

    def test_release_after_full_release_bumps(self):
        values = self.run_both([("tag", "v0.5.0"), ("fix: a", []), ("fix: b", ["release: true"])])
        self.assertEqual(values["version"], "0.5.1")
        self.assertEqual(values["prerelease"], "false")

    def test_feat_bumps_the_minor(self):
        values = self.run_both([("tag", "v0.5.3"), ("feat(x): a", []), ("fix: b", ["release: true"])])
        self.assertEqual(values["version"], "0.6.0")

    def test_breaking_bumps_the_minor_in_0x(self):
        values = self.run_both([("tag", "v0.5.3"), ("fix!: a", []), ("fix: b", ["release: true"])])
        self.assertEqual(values["version"], "0.6.0")
        values = self.run_both(
            [("tag", "v0.5.3"), ("fix: a", [], "BREAKING CHANGE: gone"), ("fix: b", ["release: true"])]
        )
        self.assertEqual(values["version"], "0.6.0")

    def test_breaking_bumps_the_major_after_1(self):
        values = self.run_both([("tag", "v1.2.3"), ("feat!: a", ["release: true"])])
        self.assertEqual(values["version"], "2.0.0")

    def test_old_subjects_scan_as_patch(self):
        values = self.run_both([("tag", "v0.5.3"), ("area: a", ["release: true"])])
        self.assertEqual(values["version"], "0.5.4")

    def test_no_tag_starts_from_zero(self):
        values = self.run_both([("feat: a", ["pre-release: true"])])
        self.assertEqual(values["version"], "0.1.0-alpha")
        self.assertEqual(values["last_tag"], "")

    def test_prerelease_after_alpha_bumps_as_before(self):
        values = self.run_both([("tag", "v0.5.0-alpha"), ("fix: a", ["pre-release: true"])])
        self.assertEqual(values["version"], "0.5.1-alpha")
        self.assertEqual(values["prerelease"], "true")

    def test_bump_trailer_after_alpha_still_bumps(self):
        values = self.run_both(
            [("tag", "v0.5.0-alpha"), ("fix: a", ["release: true", "bump: minor"])]
        )
        self.assertEqual(values["version"], "0.6.0")
        self.assertEqual(values["prerelease"], "false")

    def test_a_bad_bump_trailer_fails(self):
        self.run_both([("tag", "v0.5.0"), ("fix: a", ["release: true", "bump: huge"])], expect_ok=False)

    def test_existing_stable_tag_blocks_promotion(self):
        values = self.run_both(
            [("tag", "v0.5.0"), ("fix: a", []), ("tag", "v0.5.0-alpha"), ("fix: b", ["release: true"])]
        )
        self.assertEqual(values["version"], "0.5.1")

    def test_release_and_prerelease_trailers_keep_the_prerelease(self):
        values = self.run_both(
            [("tag", "v0.5.0-alpha"), ("fix: a", ["release: true", "pre-release: true"])]
        )
        self.assertEqual(values["version"], "0.5.1-alpha")
        self.assertEqual(values["prerelease"], "true")


if __name__ == "__main__":
    unittest.main()

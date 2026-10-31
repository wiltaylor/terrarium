"""Tests for harness.py: semver versions and bump kinds, the update plan, the
file edits, and the parsing for `harness::check`.
Run: uv run python -m unittest discover -s scripts -p 'test_harness*.py'"""

import contextlib
import importlib.util
import io
import json
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIX = HERE / "fixtures"
spec = importlib.util.spec_from_file_location("harness", HERE / "harness.py")
h = importlib.util.module_from_spec(spec)
sys.modules["harness"] = h  # dataclasses look the module up
spec.loader.exec_module(h)

V = h.parse_version


def fixture_cfg():
    return h.hv.load(FIX / "harnesses.toml")


class Semver(unittest.TestCase):
    def test_bump_kinds_follow_the_compatibility_rule(self):
        self.assertEqual(h.bump_kind(V("2.1.310"), V("2.1.320")), "patch")
        self.assertEqual(h.bump_kind(V("2.1.310"), V("2.2.0")), "minor")
        self.assertEqual(h.bump_kind(V("2.1.310"), V("3.0.0")), "major")
        # 0.x: the minor part is the compatibility part.
        self.assertEqual(h.bump_kind(V("0.160.0"), V("0.160.3")), "patch")
        self.assertEqual(h.bump_kind(V("0.160.0"), V("0.164.0")), "major")
        self.assertEqual(h.bump_kind(V("0.85.1"), V("1.4.0")), "major")
        self.assertEqual(h.bump_kind(V("1.0.103"), V("1.0.103")), "same")
        self.assertEqual(h.bump_kind(V("1.0.103"), V("1.0.90")), "downgrade")

    def test_version_output_parsing(self):
        self.assertEqual(h.find_version("2.1.310 (Claude Code)\n"), "2.1.310")
        self.assertEqual(h.find_version("codex-cli 0.160.0"), "0.160.0")
        self.assertEqual(h.find_version("GitHub Copilot CLI 1.0.103.\nRun 'copilot update'"), "1.0.103")
        self.assertIsNone(h.find_version("command not found"))


class Plan(unittest.TestCase):
    def test_a_patch_and_a_minor_bump(self):
        p = h.make_plan(fixture_cfg(), "claude", "2.1.320")
        self.assertEqual((p.old, p.new, p.bump, p.pinned), ("2.1.310", "2.1.320", "patch", "2.1.310"))
        self.assertEqual(h.make_plan(fixture_cfg(), "claude", "2.2.4").bump, "minor")

    def test_a_major_bump_is_allowed_without_allow_major(self):
        p = h.make_plan(fixture_cfg(), "codex", "0.164.0")
        self.assertEqual(p.bump, "major")
        self.assertEqual(h.make_plan(fixture_cfg(), "claude", "3.1.0").bump, "major")

    def test_a_downgrade_is_allowed(self):
        p = h.make_plan(fixture_cfg(), "claude", "2.1.300")
        self.assertEqual(p.bump, "downgrade")

    def test_an_extension_plans_like_a_harness(self):
        p = h.make_plan(fixture_cfg(), "copilot-prompt", "0.1.3")
        self.assertEqual((p.kind, p.bump), ("extension", "patch"))

    def test_unknown_names_and_pre_releases_are_refused(self):
        with self.assertRaises(h.PlanError):
            h.make_plan(fixture_cfg(), "gemini", "1.0.0")
        with self.assertRaises(h.PlanError):
            h.make_plan(fixture_cfg(), "claude", "2.1.311-beta.1")

    def test_an_old_toml_with_tested_keys_still_loads(self):
        text = (FIX / "harnesses.toml").read_text().replace(
            'version = "2.1.310"\n', 'version = "2.1.310"\ntested = ">=2.1.310, <2.2.0"\n'
        )
        cfg = h.hv.validate(tomllib.loads(text))
        self.assertEqual(h.make_plan(cfg, "claude", "3.0.0").bump, "major")


class Edits(unittest.TestCase):
    def test_set_table_values_touches_only_the_table(self):
        text = (FIX / "harnesses.toml").read_text()
        new = h.set_table_values(text, "harness.codex", {"version": "0.164.0"})
        cfg = tomllib.loads(new)
        self.assertEqual(cfg["harness"]["codex"]["version"], "0.164.0")
        self.assertEqual(cfg["harness"]["claude"]["version"], "2.1.310")
        self.assertIn('version = "0.164.0" # trailing comment stays', new)
        self.assertIn("# A fixture", new)

    def test_history_row_goes_at_the_end_of_the_table(self):
        p = h.make_plan(fixture_cfg(), "claude", "2.2.4")
        row = h.compat_row(p, "2026-10-25")
        self.assertEqual(row, "| 2026-10-25 | claude | 2.1.310 → 2.2.4 | minor | not run yet |")
        new = h.add_history_row((FIX / "compat.md").read_text(), row)
        lines = new.splitlines()
        i = lines.index(row)
        self.assertTrue(lines[i - 1].startswith("| 2026-10-24 | claude"))
        self.assertEqual(lines[i + 1], "")

    def test_a_compat_md_without_the_section_is_an_error(self):
        with self.assertRaises(h.PlanError):
            h.add_history_row("# nothing\n", "| x |")


class CheckParsing(unittest.TestCase):
    def test_npm_view_array_shape(self):
        v, date = h.parse_npm_view((FIX / "npm-view-array.json").read_text())
        self.assertEqual((v, date), ("0.164.0", "2026-10-23T14:20:11.000Z"))

    def test_npm_view_object_shape(self):
        v, date = h.parse_npm_view((FIX / "npm-view-object.json").read_text())
        self.assertEqual((v, date), ("2.1.310", "2026-10-23T18:00:00.000Z"))

    def test_npm_view_garbage(self):
        self.assertEqual(h.parse_npm_view("npm ERR!"), (None, None))

    def test_npm_ls(self):
        text = (FIX / "npm-ls.json").read_text()
        self.assertEqual(h.parse_npm_ls(text, "copilot-custom-system-prompt"), "0.1.2")
        self.assertIsNone(h.parse_npm_ls("{}", "copilot-custom-system-prompt"))

    def test_judge_flags_a_newer_major_version(self):
        e = {"version": "0.160.0"}
        status, notes = h.judge(e, "0.160.0", "0.164.0", True)
        self.assertEqual((status, notes), ("update available (major)", []))

    def test_judge_flags_a_container_that_is_not_on_the_pin(self):
        e = {"name": "claude", "version": "2.1.310"}
        status, notes = h.judge(e, "2.1.300", "2.1.320", True)
        # The status compares npm with the container, not with the pin.
        self.assertEqual(status, "update available")
        self.assertEqual(
            notes[0],
            "the container has 2.1.300, the pin is 2.1.310 (`just harness::update claude 2.1.310`, or update with `--pin`)",
        )
        self.assertEqual(h.judge(e, "2.1.320", "2.1.320", True)[0], "latest")
        self.assertIn("not found in the container", h.judge(e, None, "2.1.320", True)[1][0])

    def test_judge_status_words(self):
        e = {"version": "2.1.310"}
        self.assertEqual(h.judge(e, None, "2.1.310", False)[0], "latest")
        self.assertEqual(h.judge(e, None, "2.2.4", False)[0], "update available")
        self.assertEqual(h.judge(e, None, "3.0.0", False)[0], "update available (major)")
        self.assertEqual(h.judge(e, None, None, False)[0], "unknown")

    def test_table_renders_every_row(self):
        rows = [h.Row("claude", "harness", "p", "2.1.310", "2.1.310", None, "2.1.310", "2026-10-23T18:00:00Z", "latest")]
        out = h.render_table(rows, True)
        self.assertEqual(out.splitlines()[0].split(), ["HARNESS", "PINNED", "CONTAINER", "HOST", "LATEST", "PUBLISHED", "STATUS"])
        self.assertNotIn("TESTED", out)
        self.assertIn("2026-10-23", out)
        self.assertNotIn("harness::latest", out)

    def test_table_footer_names_both_commands(self):
        rows = [h.Row("codex", "harness", "p", "0.160.0", None, None, "0.164.0", None, "update available (major)")]
        out = h.render_table(rows, False)
        self.assertIn("update available (major)", out)
        self.assertNotIn("range", out)
        self.assertIn("`just harness::latest` updates every harness in place", out)
        self.assertIn("`just harness::update <name>` updates one", out)
        self.assertIn("Neither rebuilds the image", out)


class Latest(unittest.TestCase):
    # The fixture: claude has a minor bump, codex a major bump, and the
    # copilot-prompt extension is at the latest.
    NPM = {"claude": "2.2.4", "codex": "0.164.0", "copilot-prompt": "0.1.2"}

    def test_the_plan_bumps_each_harness_with_a_newer_release(self):
        items = {i.name: i for i in h.plan_latest(fixture_cfg(), self.NPM)}
        self.assertEqual(items["claude"].status, "minor")
        self.assertEqual(items["codex"].status, "major")
        self.assertEqual(items["codex"].plan.new, "0.164.0")
        self.assertEqual(items["copilot-prompt"].status, "latest")
        self.assertIsNone(items["copilot-prompt"].plan)

    def test_npm_without_an_answer_or_behind_the_pin(self):
        items = {i.name: i for i in h.plan_latest(fixture_cfg(), {"claude": "2.1.300", "codex": None, "copilot-prompt": "0.1.2"})}
        self.assertEqual(items["claude"].status, "latest")
        self.assertIn("newer than the npm latest", items["claude"].note)
        self.assertEqual(items["codex"].status, "unknown")

    def run_dry(self, npm):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = h.run_latest(fixture_cfg(), npm, dry_run=True)
        return rc, out.getvalue()

    def test_the_dry_run_prints_the_plan_table_and_changes_nothing(self):
        before = (FIX / "harnesses.toml").read_text()
        rc, out = self.run_dry(self.NPM)
        self.assertEqual(rc, 0)
        self.assertEqual((FIX / "harnesses.toml").read_text(), before)
        lines = out.splitlines()
        self.assertIn("copilot-prompt: latest (0.1.2)", lines)
        self.assertIn("claude (@anthropic-ai/claude-code): 2.1.310 -> 2.2.4 (minor)", lines)
        self.assertIn("codex (@openai/codex): 0.160.0 -> 0.164.0 (major)", lines)
        self.assertFalse(any("WARNING" in l for l in lines), lines)
        self.assertIn("dry run: nothing changed", out)
        table = lines[lines.index("plan (dry run: nothing changed):") + 1 :]
        self.assertEqual(table[0].split(), ["HARNESS", "FROM", "TO", "CHANGE"])
        self.assertEqual(table[1].split()[:5], ["claude", "2.1.310", "2.2.4", "minor", "bump"])
        self.assertEqual(table[2].split()[:5], ["codex", "0.160.0", "0.164.0", "major", "bump"])
        self.assertEqual(table[3].split(), ["copilot-prompt", "0.1.2", "0.1.2", "latest"])
        self.assertIn("  just ci::compat claude", lines)
        self.assertIn("  just ci::compat codex", lines)
        self.assertNotIn("  just ci::compat copilot", lines)

    def test_nothing_newer_means_nothing_to_do(self):
        rc, out = self.run_dry({"claude": "2.1.310", "codex": "0.160.0", "copilot-prompt": "0.1.2"})
        self.assertEqual(rc, 0)
        self.assertIn("claude: latest (2.1.310)", out)
        self.assertIn("nothing to update", out)
        self.assertNotIn("ci::compat", out)


class InstalledPlan(unittest.TestCase):
    def test_the_bump_is_from_the_container_version(self):
        # The container is already on 2.2.0; 2.2.4 is a patch from there.
        p = h.make_plan(fixture_cfg(), "claude", "2.2.4", current="2.2.0")
        self.assertEqual((p.old, p.pinned, p.bump), ("2.2.0", "2.1.310", "patch"))
        row = h.compat_row(p, "2026-10-26")
        self.assertIn("| claude | 2.1.310 → 2.2.4 | minor |", row)

    def test_a_major_from_the_container_is_allowed(self):
        p = h.make_plan(fixture_cfg(), "claude", "3.0.0", current="2.1.400")
        self.assertEqual((p.old, p.bump), ("2.1.400", "major"))


class Manifest(unittest.TestCase):
    def test_parse_manifest(self):
        text = '{"claude": {"package": "@anthropic-ai/claude-code", "version": "2.1.310", "installed_at": "2026-10-26T00:00:00Z"}, "x": 1}'
        self.assertEqual(h.parse_manifest(text), {"claude": "2.1.310"})
        self.assertEqual(h.parse_manifest("not json"), {})
        self.assertEqual(h.parse_manifest("[]"), {})

    def test_check_reads_the_manifest_then_falls_back_to_version(self):
        calls = []

        def fake_run(cmd, timeout=60, **kw):
            calls.append(cmd)
            if cmd[-2:] == ["cat", h.MANIFEST]:
                out = '{"claude": {"package": "p", "version": "2.1.320", "installed_at": "t"}}'
            elif cmd[-2:] == ["codex", "--version"]:
                out = "codex-cli 0.160.0\n"
            else:
                out = ""
            return subprocess.CompletedProcess(cmd, 0, out, "")

        with mock.patch.object(h, "run", fake_run):
            found = h.container_versions(fixture_cfg())
        self.assertEqual(found, {"claude": "2.1.320", "codex": "0.160.0"})
        self.assertEqual(calls[0][:7], h.EXEC)
        self.assertNotIn(["claude", "--version"], [c[-2:] for c in calls])


class FakeContainer:
    """Replaces the docker calls of harness.py: the stack is up, the
    container has `versions`, and the CLI answers from `answers`."""

    def __init__(self, versions, answers):
        self.versions = versions
        self.answers = list(answers)
        self.calls = []

    def cli(self, args):
        self.calls.append(args)
        code, body = self.answers.pop(0)
        return code, json.dumps(body), ""

    def __enter__(self):
        self.stack = contextlib.ExitStack()
        self.stack.enter_context(mock.patch.object(h, "container_running", lambda: True))
        self.stack.enter_context(mock.patch.object(h, "container_versions", lambda cfg: dict(self.versions)))
        self.stack.enter_context(mock.patch.object(h, "container_cli", self.cli))
        return self

    def __exit__(self, *a):
        self.stack.close()


class Update(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        d = Path(self.tmp.name)
        self.toml = d / "harnesses.toml"
        self.md = d / "compat.md"
        shutil.copy(FIX / "harnesses.toml", self.toml)
        shutil.copy(FIX / "compat.md", self.md)
        self.patches = contextlib.ExitStack()
        self.patches.enter_context(mock.patch.object(h.hv, "TOML", self.toml))
        self.patches.enter_context(mock.patch.object(h, "COMPAT_MD", self.md))
        self.patches.enter_context(mock.patch.object(h.hv, "load", lambda path=None: h.hv.validate(tomllib.loads(self.toml.read_text()))))
        self.before = (self.toml.read_text(), self.md.read_text())

    def tearDown(self):
        self.patches.close()
        self.tmp.cleanup()

    def update(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = h.main(["update", *argv])
        return rc, out.getvalue(), err.getvalue()

    def ok(self, name, frm, to):
        return 0, {"harness": name, "package": "p", "from": frm, "to": to, "output": "added 1 package"}

    def test_update_calls_the_cli_in_the_container(self):
        with FakeContainer({"claude": "2.1.310"}, [self.ok("claude", "2.1.310", "2.1.320")]) as c:
            rc, out, _ = self.update("claude", "2.1.320")
        self.assertEqual(rc, 0, out)
        self.assertEqual(c.calls, [["admin", "harness", "update", "claude", "--version", "2.1.320"]])
        self.assertIn("added 1 package", out)
        self.assertIn("claude: 2.1.310 -> 2.1.320", out)
        self.assertEqual((self.toml.read_text(), self.md.read_text()), self.before)

    def test_force_goes_to_the_cli(self):
        with FakeContainer({}, [self.ok("codex", "0.160.0", "0.160.2")]) as c:
            rc, _, _ = self.update("codex", "0.160.2", "--force")
        self.assertEqual(rc, 0)
        self.assertEqual(c.calls[0][-1], "--force")

    def test_pin_writes_the_toml_and_the_compat_row_after_success(self):
        with FakeContainer({"claude": "2.1.310"}, [self.ok("claude", "2.1.310", "2.2.4")]):
            rc, out, _ = self.update("claude", "2.2.4", "--pin")
        self.assertEqual(rc, 0, out)
        cfg = tomllib.loads(self.toml.read_text())
        self.assertEqual(cfg["harness"]["claude"]["version"], "2.2.4")
        self.assertNotIn("tested", cfg["harness"]["claude"])
        self.assertIn("| claude | 2.1.310 → 2.2.4 | minor | not run yet |", self.md.read_text())
        self.assertIn("A fresh install gets the new version", out)

    def test_a_major_bump_needs_no_flag_and_allow_major_is_accepted(self):
        for extra in ([], ["--allow-major"]):
            with FakeContainer({"codex": "0.160.0"}, [self.ok("codex", "0.160.0", "1.0.0")]) as c:
                rc, out, err = self.update("codex", "1.0.0", *extra)
            self.assertEqual(rc, 0, out + err)
            self.assertEqual(c.calls, [["admin", "harness", "update", "codex", "--version", "1.0.0"]])
            self.assertNotIn("WARNING", out)

    def test_a_conflict_prints_the_message_and_changes_no_file(self):
        busy = (1, {"error": {"code": "conflict", "message": "another harness update runs now; try again when it ends"}})
        with FakeContainer({"claude": "2.1.310"}, [busy]):
            rc, _, err = self.update("claude", "2.1.320", "--pin")
        self.assertEqual(rc, 1)
        self.assertIn("another harness update runs now", err)
        self.assertEqual((self.toml.read_text(), self.md.read_text()), self.before)

    def test_the_note_on_running_agents_prints(self):
        note = "1 live agent(s) run another version of claude: terra/worker. Each keeps its version and moves to 2.1.320 at its next restart"
        done = (0, {"harness": "claude", "from": "2.1.310", "to": "2.1.320", "output": "", "note": note})
        with FakeContainer({"claude": "2.1.310"}, [done]):
            rc, out, _ = self.update("claude", "2.1.320")
        self.assertEqual(rc, 0, out)
        self.assertIn("moves to 2.1.320 at its next restart", out)

    def test_an_npm_failure_prints_the_output(self):
        bad = (1, {"error": {"code": "invalid", "message": "npm install failed", "details": {"output": "npm error notarget"}}})
        with FakeContainer({"claude": "2.1.310"}, [bad]):
            rc, out, err = self.update("claude", "2.1.399", "--pin")
        self.assertEqual(rc, 1)
        self.assertIn("npm error notarget", out)
        self.assertIn("npm install failed", err)
        self.assertEqual((self.toml.read_text(), self.md.read_text()), self.before)

    def test_dry_run_calls_nothing(self):
        with FakeContainer({"claude": "2.1.310"}, []) as c:
            rc, out, _ = self.update("claude", "2.1.320", "--dry-run", "--pin")
        self.assertEqual(rc, 0)
        self.assertEqual(c.calls, [])
        self.assertIn("dry run: nothing changed", out)
        self.assertEqual((self.toml.read_text(), self.md.read_text()), self.before)

    def test_a_stopped_stack_is_an_error(self):
        with mock.patch.object(h, "container_running", lambda: False):
            rc, _, err = self.update("claude", "2.1.320")
        self.assertEqual(rc, 1)
        self.assertIn("run `just up`", err)

    def test_latest_calls_once_per_planned_item_and_pins_the_successes(self):
        npm = {"claude": "2.2.4", "codex": "0.164.0", "copilot-prompt": "0.1.2"}
        answers = [self.ok("claude", "2.1.310", "2.2.4"), (1, {"error": {"code": "invalid", "message": "npm install failed"}})]
        out = io.StringIO()
        with FakeContainer({}, answers) as c, contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            rc = h.run_latest(fixture_cfg(), npm, dry_run=False, pin=True)
        self.assertEqual(rc, 1)
        self.assertEqual(
            c.calls,
            [
                ["admin", "harness", "update", "claude", "--version", "2.2.4"],
                ["admin", "harness", "update", "codex", "--version", "0.164.0"],
            ],
        )
        cfg = tomllib.loads(self.toml.read_text())
        self.assertEqual(cfg["harness"]["claude"]["version"], "2.2.4")
        self.assertEqual(cfg["harness"]["codex"]["version"], "0.160.0")
        self.assertIn("codex: npm install failed", out.getvalue())


if __name__ == "__main__":
    unittest.main()

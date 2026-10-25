"""Tests for harness.py: semver ranges, the update plan (range widening and the
major guard), the file edits, and the parsing for `harness::check`.
Run: uv run python -m unittest discover -s scripts -p 'test_harness*.py'"""

import contextlib
import importlib.util
import io
import sys
import tomllib
import unittest
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
    def test_ranges_match_like_rust_semver(self):
        r = ">=2.1.310, <2.2.0"
        self.assertTrue(h.matches(V("2.1.310"), r))
        self.assertTrue(h.matches(V("2.1.999"), r))
        self.assertFalse(h.matches(V("2.1.309"), r))
        self.assertFalse(h.matches(V("2.2.0"), r))
        self.assertFalse(h.matches(V("2.1.311-beta.1"), r))
        self.assertTrue(h.matches(V("1.4.0"), "^1.2"))
        self.assertFalse(h.matches(V("0.86.0"), "^0.85.1"))
        self.assertTrue(h.matches(V("0.85.9"), "~0.85.1"))
        self.assertTrue(h.matches(V("1.0.0"), "=1.0.0"))

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
    def test_a_patch_inside_the_range_keeps_the_range(self):
        p = h.make_plan(fixture_cfg(), "claude", "2.1.320", False)
        self.assertEqual((p.bump, p.tested_new), ("patch", ">=2.1.310, <2.2.0"))
        self.assertEqual(p.warnings, [])

    def test_a_minor_bump_widens_only_the_upper_bound(self):
        p = h.make_plan(fixture_cfg(), "claude", "2.2.4", False)
        self.assertEqual(p.bump, "minor")
        self.assertEqual(p.tested_new, ">=2.1.310, <2.3.0")
        self.assertTrue(h.matches(V("2.2.4"), p.tested_new))
        self.assertTrue(h.matches(V("2.1.310"), p.tested_new))

    def test_a_major_bump_needs_allow_major(self):
        with self.assertRaises(h.PlanError) as e:
            h.make_plan(fixture_cfg(), "claude", "3.0.0", False)
        self.assertIn("--allow-major", str(e.exception))
        with self.assertRaises(h.PlanError):
            h.make_plan(fixture_cfg(), "codex", "0.164.0", False)

    def test_the_major_refusal_says_what_to_run(self):
        with self.assertRaises(h.PlanError) as e:
            h.make_plan(fixture_cfg(), "codex", "0.164.0", False)
        msg = str(e.exception)
        self.assertIn("codex 0.160.0 -> 0.164.0 is a major bump", msg)
        self.assertIn("just harness::update codex --allow-major", msg)
        self.assertIn("just harness::latest", msg)

    def test_an_allowed_major_bump_resets_the_range_and_warns(self):
        p = h.make_plan(fixture_cfg(), "codex", "0.164.0", True)
        self.assertEqual((p.bump, p.tested_new), ("major", ">=0.164.0, <0.165.0"))
        self.assertEqual(len(p.warnings), 1)
        self.assertIn("hooks, flags", p.warnings[0])
        p = h.make_plan(fixture_cfg(), "claude", "3.1.0", True)
        self.assertEqual(p.tested_new, ">=3.1.0, <3.2.0")

    def test_a_downgrade_below_the_range_is_refused(self):
        with self.assertRaises(h.PlanError) as e:
            h.make_plan(fixture_cfg(), "claude", "2.1.300", False)
        self.assertIn("edit harnesses.toml", str(e.exception))

    def test_an_extension_has_no_range(self):
        p = h.make_plan(fixture_cfg(), "copilot-prompt", "0.1.3", False)
        self.assertEqual((p.bump, p.tested_old, p.tested_new), ("patch", None, None))

    def test_unknown_names_and_pre_releases_are_refused(self):
        with self.assertRaises(h.PlanError):
            h.make_plan(fixture_cfg(), "gemini", "1.0.0", False)
        with self.assertRaises(h.PlanError):
            h.make_plan(fixture_cfg(), "claude", "2.1.311-beta.1", False)


class Edits(unittest.TestCase):
    def test_set_table_values_touches_only_the_table(self):
        text = (FIX / "harnesses.toml").read_text()
        new = h.set_table_values(text, "harness.codex", {"version": "0.164.0", "tested": ">=0.164.0, <0.165.0"})
        cfg = tomllib.loads(new)
        self.assertEqual(cfg["harness"]["codex"]["version"], "0.164.0")
        self.assertEqual(cfg["harness"]["codex"]["tested"], ">=0.164.0, <0.165.0")
        self.assertEqual(cfg["harness"]["claude"]["version"], "2.1.310")
        self.assertIn('tested = ">=0.164.0, <0.165.0" # trailing comment stays', new)
        self.assertIn("# A fixture", new)

    def test_history_row_goes_at_the_end_of_the_table(self):
        p = h.make_plan(fixture_cfg(), "claude", "2.2.4", False)
        row = h.compat_row(p, "2026-10-25")
        self.assertEqual(
            row,
            "| 2026-10-25 | claude | 2.1.310 → 2.2.4 | minor | `>=2.1.310, <2.2.0` → `>=2.1.310, <2.3.0` | not run yet |",
        )
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
        e = {"version": "0.160.0", "tested": ">=0.160.0, <0.161.0"}
        in_range, status, notes = h.judge(e, "0.160.0", "0.159.0", "0.164.0", True)
        self.assertEqual((in_range, status), (False, "update available (major)"))
        self.assertEqual(len(notes), 1)
        self.assertIn("the host has 0.159.0", notes[0])

    def test_judge_flags_a_container_that_is_not_on_the_pin(self):
        e = {"version": "2.1.310", "tested": ">=2.1.310, <2.2.0"}
        in_range, status, notes = h.judge(e, "2.1.300", None, "2.1.320", True)
        self.assertEqual((in_range, status), (True, "update available"))
        self.assertIn("run `just up`", notes[0])

    def test_judge_status_words(self):
        e = {"version": "2.1.310", "tested": ">=2.1.310, <2.2.0"}
        self.assertEqual(h.judge(e, None, None, "2.1.310", False)[1], "latest")
        # A minor bump outside the range is still on the same major.
        self.assertEqual(h.judge(e, None, None, "2.2.4", False)[:2], (False, "update available"))
        self.assertEqual(h.judge(e, None, None, "3.0.0", False)[1], "update available (major)")
        self.assertEqual(h.judge(e, None, None, None, False)[1], "unknown")
        ext = {"version": "0.1.2", "tested": None}
        self.assertEqual(h.judge(ext, None, None, "0.1.3", False)[:2], (None, "update available"))

    def test_table_renders_every_row(self):
        rows = [h.Row("claude", "harness", "p", "2.1.310", ">=2.1.310, <2.2.0", "2.1.310", None, "2.1.310", "2026-10-23T18:00:00Z", True, "latest")]
        out = h.render_table(rows, True)
        self.assertIn("HARNESS", out.splitlines()[0])
        self.assertIn("LATEST IN TESTED RANGE", out.splitlines()[0])
        self.assertIn("2026-10-23", out)
        self.assertNotIn("harness::latest", out)

    def test_table_footer_names_both_commands(self):
        rows = [h.Row("codex", "harness", "p", "0.160.0", ">=0.160.0, <0.161.0", None, None, "0.164.0", None, False, "update available (major)")]
        out = h.render_table(rows, False)
        self.assertIn("update available (major)", out)
        self.assertNotIn("outside range", out)
        self.assertIn("`just harness::latest` pulls every harness", out)
        self.assertIn("`just harness::update <name>` pulls one", out)


class Latest(unittest.TestCase):
    # The fixture: claude has a minor bump, codex a major bump, and the
    # copilot-prompt extension is at the latest.
    NPM = {"claude": "2.2.4", "codex": "0.164.0", "copilot-prompt": "0.1.2"}

    def test_the_plan_bumps_each_harness_with_a_newer_release(self):
        items = {i.name: i for i in h.plan_latest(fixture_cfg(), self.NPM)}
        self.assertEqual(items["claude"].status, "minor")
        self.assertEqual(items["claude"].plan.tested_new, ">=2.1.310, <2.3.0")
        self.assertEqual(items["codex"].status, "major")
        self.assertEqual(items["codex"].plan.tested_new, ">=0.164.0, <0.165.0")
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
            rc = h.run_latest(fixture_cfg(), npm, dry_run=True, no_rollout=False, image="terrarium:test")
        return rc, out.getvalue()

    def test_the_dry_run_prints_the_plan_table_and_changes_nothing(self):
        before = (FIX / "harnesses.toml").read_text()
        rc, out = self.run_dry(self.NPM)
        self.assertEqual(rc, 0)
        self.assertEqual((FIX / "harnesses.toml").read_text(), before)
        lines = out.splitlines()
        self.assertIn("copilot-prompt: latest (0.1.2)", lines)
        self.assertIn("claude (@anthropic-ai/claude-code): 2.1.310 -> 2.2.4 (minor)", lines)
        self.assertIn("  WARNING: codex 0.160.0 -> 0.164.0 is a major bump: hooks and flags may have changed", lines)
        self.assertEqual(sum("WARNING" in l for l in lines), 1)
        self.assertIn("dry run: nothing changed", out)
        table = lines[lines.index("plan (dry run: nothing changed):") + 1 :]
        self.assertEqual(table[0].split(), ["HARNESS", "FROM", "TO", "CHANGE", "TESTED", "RANGE"])
        self.assertEqual(table[1].split()[:5], ["claude", "2.1.310", "2.2.4", "minor", "bump"])
        self.assertEqual(table[2].split()[:5], ["codex", "0.160.0", "0.164.0", "major", "bump"])
        self.assertEqual(table[3].split(), ["copilot-prompt", "0.1.2", "0.1.2", "latest", "-"])
        self.assertIn("  just ci::compat claude", lines)
        self.assertIn("  just ci::compat codex", lines)
        self.assertNotIn("  just ci::compat copilot", lines)

    def test_nothing_newer_means_nothing_to_do(self):
        rc, out = self.run_dry({"claude": "2.1.310", "codex": "0.160.0", "copilot-prompt": "0.1.2"})
        self.assertEqual(rc, 0)
        self.assertIn("claude: latest (2.1.310)", out)
        self.assertIn("nothing to update", out)
        self.assertNotIn("ci::compat", out)


if __name__ == "__main__":
    unittest.main()

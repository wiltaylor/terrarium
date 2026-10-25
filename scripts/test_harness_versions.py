"""Tests for harness-versions.py: harnesses.toml -> docker build args, and the
Dockerfile check and sync. Run: uv run python -m unittest discover -s scripts -p 'test_harness*.py'"""

import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIX = HERE / "fixtures"
spec = importlib.util.spec_from_file_location("harness_versions", HERE / "harness-versions.py")
hv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hv)


class BuildArgs(unittest.TestCase):
    def test_fixture_toml_gives_one_build_arg_per_pin_in_file_order(self):
        cfg = hv.load(FIX / "harnesses.toml")
        self.assertEqual(
            hv.build_args(cfg),
            [
                "--build-arg", "NODE_IMAGE=node:22.0.0-slim",
                "--build-arg", "CLAUDE_VERSION=2.1.310",
                "--build-arg", "CODEX_VERSION=0.160.0",
                "--build-arg", "COPILOT_PROMPT_VERSION=0.1.2",
            ],
        )

    def test_arg_names(self):
        self.assertEqual(hv.arg_name("claude"), "CLAUDE_VERSION")
        self.assertEqual(hv.arg_name("copilot-prompt"), "COPILOT_PROMPT_VERSION")

    def test_a_table_without_a_key_is_an_error(self):
        with self.assertRaises(hv.PinError):
            hv.validate({"node": {"image": "x"}, "harness": {"claude": {"package": "p", "version": "1.0.0"}}})
        with self.assertRaises(hv.PinError):
            hv.validate({"harness": {}})

    def test_main_prints_the_args(self):
        import contextlib
        import io

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = hv.main(["build-args", "--toml", str(FIX / "harnesses.toml")])
        self.assertEqual(rc, 0)
        self.assertTrue(out.getvalue().startswith("--build-arg NODE_IMAGE=node:22.0.0-slim --build-arg CLAUDE_VERSION=2.1.310"))


class Dockerfile(unittest.TestCase):
    def setUp(self):
        self.cfg = hv.load(FIX / "harnesses.toml")
        self.text = (FIX / "Dockerfile").read_text()

    def test_check_names_each_drifted_default(self):
        problems = hv.check_dockerfile(self.cfg, self.text)
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("ARG NODE_IMAGE=node:21.0.0-slim, harnesses.toml: node:22.0.0-slim", problems[0])
        self.assertIn("ARG CLAUDE_VERSION=2.1.300, harnesses.toml: 2.1.310", problems[1])

    def test_sync_rewrites_the_defaults_and_then_check_passes(self):
        new = hv.sync_dockerfile(self.cfg, self.text)
        self.assertIn("ARG CLAUDE_VERSION=2.1.310\n", new)
        self.assertIn("ARG NODE_IMAGE=node:22.0.0-slim\n", new)
        self.assertIn('"@anthropic-ai/claude-code@${CLAUDE_VERSION}"', new)
        self.assertEqual(hv.check_dockerfile(self.cfg, new), [])

    def test_sync_changes_only_the_values(self):
        new = hv.sync_dockerfile(self.cfg, self.text)
        self.assertEqual(len(new.splitlines()), len(self.text.splitlines()))
        self.assertEqual(hv.sync_dockerfile(self.cfg, new), new)
        real = hv.DOCKERFILE.read_text()
        self.assertEqual(hv.sync_dockerfile(hv.load(hv.TOML), real), real)

    def test_a_missing_install_line_is_reported(self):
        text = self.text.replace('"@openai/codex@${CODEX_VERSION}" \\\n', "")
        problems = hv.check_dockerfile(self.cfg, hv.sync_dockerfile(self.cfg, text))
        self.assertEqual(problems, ['docker/Dockerfile does not install "@openai/codex@${CODEX_VERSION}"'])

    def test_the_real_dockerfile_is_in_step_with_the_real_toml(self):
        cfg = hv.load(hv.TOML)
        self.assertEqual(hv.check_dockerfile(cfg, hv.DOCKERFILE.read_text()), [])


if __name__ == "__main__":
    unittest.main()

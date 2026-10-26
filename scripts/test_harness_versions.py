"""Tests for harness-versions.py: reading harnesses.toml, and the Dockerfile
check and sync. Run: uv run python -m unittest discover -s scripts -p 'test_harness*.py'"""

import contextlib
import importlib.util
import io
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIX = HERE / "fixtures"
spec = importlib.util.spec_from_file_location("harness_versions", HERE / "harness-versions.py")
hv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hv)


class Load(unittest.TestCase):
    def test_a_table_without_a_key_is_an_error(self):
        with self.assertRaises(hv.PinError):
            hv.validate({"node": {"image": "x"}, "harness": {"claude": {"package": "p", "version": "1.0.0"}}})
        with self.assertRaises(hv.PinError):
            hv.validate({"harness": {}})

    def test_packages_lists_harnesses_then_extensions(self):
        cfg = hv.load(FIX / "harnesses.toml")
        self.assertEqual(hv.packages(cfg), ["@anthropic-ai/claude-code", "@openai/codex", "copilot-custom-system-prompt"])

    def test_build_args_is_gone(self):
        self.assertFalse(hasattr(hv, "build_args"))
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            hv.main(["build-args", "--toml", str(FIX / "harnesses.toml")])


class Dockerfile(unittest.TestCase):
    def setUp(self):
        self.cfg = hv.load(FIX / "harnesses.toml")
        self.text = (FIX / "Dockerfile").read_text()

    def test_check_names_a_drifted_node_image(self):
        problems = hv.check_dockerfile(self.cfg, self.text)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("ARG NODE_IMAGE=node:21.0.0-slim, harnesses.toml: node:22.0.0-slim", problems[0])

    def test_sync_rewrites_the_node_image_and_then_check_passes(self):
        new = hv.sync_dockerfile(self.cfg, self.text)
        self.assertIn("ARG NODE_IMAGE=node:22.0.0-slim\n", new)
        self.assertEqual(len(new.splitlines()), len(self.text.splitlines()))
        self.assertEqual(hv.check_dockerfile(self.cfg, new), [])
        self.assertEqual(hv.sync_dockerfile(self.cfg, new), new)
        real = hv.DOCKERFILE.read_text()
        self.assertEqual(hv.sync_dockerfile(hv.load(hv.TOML), real), real)

    def test_an_npm_install_of_a_pinned_package_is_reported(self):
        good = hv.sync_dockerfile(self.cfg, self.text)
        bad = good + 'RUN npm install -g --no-fund \\\n        "@openai/codex@0.160.0" \\\n    && true\n'
        problems = hv.check_dockerfile(self.cfg, bad)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("installs @openai/codex with `npm install -g`", problems[0])
        # npm ci of the dashboard, or a global install of another package, is fine.
        self.assertEqual(hv.check_dockerfile(self.cfg, good + "RUN npm install -g corepack\n"), [])

    def test_a_missing_harnesses_toml_copy_is_reported(self):
        good = hv.sync_dockerfile(self.cfg, self.text)
        text = good.replace("COPY harnesses.toml /etc/terrarium/harnesses.toml\n", "")
        self.assertEqual(
            hv.check_dockerfile(self.cfg, text),
            ["docker/Dockerfile does not copy harnesses.toml to /etc/terrarium/harnesses.toml"],
        )

    def test_the_real_dockerfile_is_in_step_with_the_real_toml(self):
        cfg = hv.load(hv.TOML)
        self.assertEqual(hv.check_dockerfile(cfg, hv.DOCKERFILE.read_text()), [])

    def test_main_check_exit_codes(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(hv.main(["check", "--toml", str(FIX / "harnesses.toml"), "--dockerfile", str(FIX / "Dockerfile")]), 1)
            self.assertEqual(hv.main(["check"]), 0)


if __name__ == "__main__":
    unittest.main()

"""Tests for scripts/touch_build_inputs.py (TER-1315)."""

import contextlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

import touch_build_inputs as tbi

OLD = 1_000_000_000  # 2001, older than any checkout
BUILD_RS = (
    'fn main() {\n'
    '    println!("cargo::rerun-if-changed=build.rs");\n'
    '    println!("cargo::rerun-if-changed=src/input.txt");\n'
    '    println!("cargo:rerun-if-changed=migrations");\n'
    '    println!("cargo::rerun-if-changed=missing.txt");\n'
    '    println!("cargo::rerun-if-changed={}", "formatted");\n'
    '}\n'
)


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args], check=True, capture_output=True, text=True,
    ).stdout


class TouchBuildInputsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        crate = self.root / "crates/a"
        (crate / "src").mkdir(parents=True)
        (crate / "migrations").mkdir()
        (crate / "build.rs").write_text(BUILD_RS)
        (crate / "src/input.txt").write_text("x\n")
        (crate / "src/other.rs").write_text("\n")
        (crate / "migrations/1.sql").write_text("SELECT 1;\n")
        (self.root / "build.rs").write_text("fn main() {}\n")
        stale = self.root / "target/debug/build/x/build.rs"
        stale.parent.mkdir(parents=True)
        stale.write_text("fn main() {}\n")
        for path in self.root.rglob("*"):
            os.utime(path, (OLD, OLD))
        self.crate = crate
        self.stale = stale

    def tearDown(self):
        self.tmp.cleanup()

    def mtime(self, rel: str) -> float:
        return (self.root / rel).stat().st_mtime

    def assert_touched(self, touched):
        expect = {
            self.root / "build.rs",
            self.crate / "build.rs",
            self.crate / "src/input.txt",
            self.crate / "migrations",
        }
        self.assertEqual(set(touched), expect)
        for path in expect:
            self.assertGreater(path.stat().st_mtime, OLD + 1, path)
        self.assertEqual(self.mtime("crates/a/src/other.rs"), OLD)
        self.assertEqual(self.mtime("crates/a/migrations/1.sql"), OLD)
        self.assertEqual(self.stale.stat().st_mtime, OLD)

    @unittest.skipUnless(shutil.which("git"), "requires git")
    def test_git_checkout_touches_tracked_build_scripts_and_keeps_status_clean(self):
        (self.root / ".gitignore").write_text("target/\n")
        git(self.root, "init", "-q")
        git(self.root, "add", "-A")
        git(self.root, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init")
        untracked = self.crate / "sub/build.rs"
        untracked.parent.mkdir()
        untracked.write_text("fn main() {}\n")
        os.utime(untracked, (OLD, OLD))
        before = git(self.root, "rev-parse", "HEAD")
        with contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(tbi.main(["touch_build_inputs.py", str(self.root)]), 0)
        self.assertIn("4 paths", out.getvalue())
        self.assert_touched(tbi.touch_all(self.root))
        self.assertEqual(untracked.stat().st_mtime, OLD)
        self.assertEqual(git(self.root, "status", "--porcelain", "--untracked-files=no"), "")
        self.assertEqual(git(self.root, "rev-parse", "HEAD"), before)

    def test_without_git_walks_the_tree_and_skips_target(self):
        self.assert_touched(tbi.touch_all(self.root))

    def test_ci_recipes_touch_build_inputs_before_cargo(self):
        mod = (Path(__file__).resolve().parents[1] / ".just/ci/mod.just").read_text()
        step = "python scripts/touch_build_inputs.py"
        for recipe in ("validate", "validate-affected"):
            body = mod.split(f"\n{recipe}: ", 1)[1].split("\n\n", 1)[0]
            self.assertIn(step, body, recipe)
            first_run = min(
                body.index(s) for s in ("just ci::", "scripts/affected.py") if s in body
            )
            self.assertLess(body.index(step), first_run, recipe)


if __name__ == "__main__":
    unittest.main()

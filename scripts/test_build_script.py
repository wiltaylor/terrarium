"""Regression coverage for build-script reuse across checkout paths (TER-751)."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class BuildScriptTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("rustc"), "requires rustc")
    def test_reused_build_script_reads_active_checkout(self):
        source = Path(__file__).resolve().parents[1] / "crates/terrariumd/build.rs"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first, second, out = (root / name for name in ("first", "second", "out"))
            for checkout in (first, second):
                (checkout / "migrations").mkdir(parents=True)
                (checkout / "migrations/first.sql").write_text("SELECT 1;\n")
            (second / "migrations/new.sql").write_text("SELECT 2;\n")
            out.mkdir()
            executable = root / "build-script"
            env = dict(os.environ, CARGO_MANIFEST_DIR=str(first))
            subprocess.run(
                ["rustc", "--edition=2024", str(source), "-o", str(executable)],
                env=env, check=True,
            )
            for checkout in (first, second, first):
                env.update(CARGO_MANIFEST_DIR=str(checkout), OUT_DIR=str(out))
                env.pop("TERRARIUM_TEST_BUILD", None)
                result = subprocess.run(
                    [str(executable)], cwd=checkout, env=env,
                    check=True, capture_output=True, text=True,
                )
                generated = (out / "named_migrations.rs").read_text()
                self.assertIn(str(checkout / "migrations/first.sql"), generated)
                self.assertEqual('"new.sql"' in generated, checkout == second)
                self.assertIn("cargo::rerun-if-changed=migrations", result.stdout)
                self.assertIn(str(checkout), (out / "dashboard.rs").read_text())


if __name__ == "__main__":
    unittest.main()

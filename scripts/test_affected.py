"""Tests for scripts/affected.py: the affected set of a change (TER-100)."""

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("affected", HERE / "affected.py")
affected = importlib.util.module_from_spec(spec)
sys.modules["affected"] = affected
spec.loader.exec_module(affected)

ROOT = Path("/repo")

# The workspace graph as in Cargo.toml (path dependencies of any kind).
GRAPH = {
    "terrarium-core": ("crates/terrarium-core", []),
    "terrarium-version": ("crates/terrarium-version", []),
    "terrarium-adapter-claude": ("crates/adapters/claude", ["terrarium-core"]),
    "terrarium-runner": ("crates/terrarium-runner",
                         ["terrarium-core", "terrarium-version", "terrarium-adapter-claude"]),
    "terrarium-host": ("crates/terrarium-host", ["terrarium-core", "terrarium-version"]),
    "terrarium-updater": ("crates/terrarium-updater", ["terrarium-version"]),
    "terrariumd": ("crates/terrariumd",
                   ["terrarium-core", "terrarium-version", "terrarium-runner",
                    "terrarium-adapter-claude", "terrarium-host", "terrarium-updater",
                    "terrariumd"]),
    "terrarium": ("crates/terrarium", ["terrarium-core", "terrarium-version"]),
    "terrarium-compat": ("crates/terrarium-compat",
                         ["terrarium-core", "terrarium-runner", "terrarium-adapter-claude"]),
}

# terrariumd test targets and the text of each.
DAEMON_TESTS = {
    "jobs": "use terrariumd::store::jobs::{self, JobKind};\nterrariumd::hosts::serve(s)",
    "board": "mod common;\n",
    "calendar": "terrariumd::calendar::tick(&s)",
    "card_reminder": "terrariumd::calendar::tick(&s)",
    "spawns": "terrariumd::spawn::prompt_info()",
    "hosts": "terrariumd::hosts::serve(s)",
}
COMMON = "terrariumd::settings::Settings\nterrariumd::start(x)\nterrariumd::supervisor::Fake"


def metadata():
    packages = []
    for name, (d, deps) in GRAPH.items():
        targets = [{"kind": ["lib"], "name": name, "src_path": f"{ROOT}/{d}/src/lib.rs"}]
        if name == "terrariumd":
            targets += [{"kind": ["test"], "name": t, "src_path": f"{ROOT}/{d}/tests/{t}.rs"}
                        for t in DAEMON_TESTS]
        packages.append({
            "id": f"path+file://{ROOT}/{d}#{name}",
            "name": name,
            "manifest_path": f"{ROOT}/{d}/Cargo.toml",
            "targets": targets,
            "dependencies": [{"name": x, "path": f"{ROOT}/{GRAPH[x][0]}"} for x in deps]
            + [{"name": "serde", "path": None}],
        })
    return {"packages": packages, "workspace_members": [p["id"] for p in packages]}


def read(path):
    p = Path(path)
    if p.name == "mod.rs":
        return COMMON
    return DAEMON_TESTS[p.stem]


CRATES = affected.workspace(metadata(), ROOT)


def plan(*files):
    return affected.plan(list(files), CRATES, read=read, common=["crates/terrariumd/tests/common/mod.rs"])


class Graph(unittest.TestCase):
    def test_workspace_reads_path_deps_and_test_targets(self):
        self.assertEqual(CRATES["terrariumd"].dir, "crates/terrariumd/")
        self.assertNotIn("terrariumd", CRATES["terrariumd"].deps)
        self.assertNotIn("serde", CRATES["terrarium"].deps)
        self.assertEqual(CRATES["terrariumd"].tests["jobs"], "crates/terrariumd/tests/jobs.rs")

    def test_reverse_deps(self):
        self.assertEqual(affected.reverse_deps(CRATES, {"terrarium-updater"}),
                         {"terrarium-updater", "terrariumd"})


class Samples(unittest.TestCase):
    """The four sample changes of the card's acceptance."""

    def test_a_terrariumd_only_change_runs_its_targets(self):
        p = plan("crates/terrariumd/src/jobs.rs", "crates/terrariumd/src/store/jobs.rs")
        self.assertFalse(p.full)
        self.assertEqual(p.whole, set())
        self.assertEqual(p.daemon_targets, {"jobs"})
        self.assertEqual(p.clippy(), ["terrariumd"])
        self.assertEqual(affected.commands(p), [
            ["just", "ci::fmt-check"],
            ["cargo", "clippy", "-p", "terrariumd", "--all-targets", "--", "-D", "warnings"],
            ["cargo", "test", "-p", "terrariumd", "--lib", "--bins", "--test", "jobs",
             "--no-fail-fast"],
        ])

    def test_a_terrarium_core_change_runs_it_and_every_reverse_dep(self):
        p = plan("crates/terrarium-core/src/config.rs")
        self.assertFalse(p.full)
        self.assertEqual(p.whole, set(GRAPH) - {"terrarium-version", "terrarium-updater"})
        self.assertIsNone(p.daemon_targets)
        test = affected.commands(p)[2]
        self.assertEqual(test[:2], ["cargo", "test"])
        self.assertIn("terrariumd", test)
        self.assertNotIn("--test", test)

    def test_a_dashboard_only_change_runs_no_rust(self):
        p = plan("dashboard/src/App.tsx", "dashboard/package.json")
        self.assertEqual(affected.commands(p),
                         [["just", "ci::fmt-check"], ["just", "ci::dashboard-check"]])

    def test_a_cargo_lock_change_runs_the_full_suite(self):
        p = plan("crates/terrariumd/src/jobs.rs", "Cargo.lock")
        self.assertTrue(p.full)
        self.assertEqual(affected.commands(p), [["just", "ci::validate"]])


class Rules(unittest.TestCase):
    def test_no_list_is_the_full_suite(self):
        p = affected.plan(None, {})
        self.assertTrue(p.full)
        self.assertEqual(affected.commands(p), [["just", "ci::validate"]])

    def test_build_and_gate_files_are_the_full_suite(self):
        for f in ["Cargo.toml", "crates/terrarium/Cargo.toml", "crates/terrariumd/build.rs",
                  "justfile", ".just/ci/mod.just", ".github/workflows/ci.yml",
                  "rust-toolchain.toml", ".cargo/config.toml", "scripts/affected.py",
                  "clippy.toml"]:
            with self.subTest(f=f):
                self.assertTrue(plan(f).full)

    def test_an_unknown_area_is_the_full_suite(self):
        p = plan("docker/Dockerfile")
        self.assertTrue(p.full)
        self.assertIn("docker/Dockerfile", p.reasons[0])

    def test_docs_run_only_fmt(self):
        p = plan("docs/api.md", "README.md", "LICENSE", "crates/terrariumd/NOTES.md")
        self.assertEqual(affected.commands(p), [["just", "ci::fmt-check"]])

    def test_scripts_run_scripts_test(self):
        p = plan("scripts/deploy.py")
        self.assertEqual(affected.commands(p),
                         [["just", "ci::fmt-check"], ["just", "ci::scripts-test"]])

    def test_a_module_named_by_tests_runs_those_targets(self):
        self.assertEqual(plan("crates/terrariumd/src/calendar/mod.rs").daemon_targets,
                         {"calendar", "card_reminder"})
        self.assertEqual(plan("crates/terrariumd/src/hosts.rs").daemon_targets, {"hosts", "jobs"})

    def test_a_plural_target_name_matches(self):
        self.assertEqual(plan("crates/terrariumd/src/spawn.rs").daemon_targets, {"spawns"})
        self.assertEqual(plan("crates/terrariumd/src/api/board.rs").daemon_targets, {"board"})

    def test_shared_and_unmatched_modules_run_all_targets(self):
        for f in ["lib.rs", "main.rs", "db.rs", "error.rs", "policy.rs", "api/mod.rs",
                  "store/mod.rs", "settings.rs", "supervisor.rs", "nothing_uses_me.rs"]:
            with self.subTest(f=f):
                p = plan(f"crates/terrariumd/src/{f}")
                self.assertIsNone(p.daemon_targets)
                self.assertIn("terrariumd", p.whole)

    def test_a_test_file_runs_only_its_target(self):
        self.assertEqual(plan("crates/terrariumd/tests/board.rs").daemon_targets, {"board"})
        for f in ["tests/common/mod.rs", "tests/fixtures/x.json", "tests/new_one.rs"]:
            with self.subTest(f=f):
                self.assertIsNone(plan(f"crates/terrariumd/{f}").daemon_targets)

    def test_a_dependency_of_terrariumd_runs_all_its_targets(self):
        p = plan("crates/terrarium-updater/src/lib.rs")
        self.assertEqual(p.whole, {"terrarium-updater", "terrariumd"})
        self.assertIsNone(p.daemon_targets)

    def test_a_leaf_crate_runs_alone(self):
        p = plan("crates/terrarium/src/main.rs")
        self.assertEqual(p.whole, {"terrarium"})
        self.assertIsNone(p.daemon_targets)

    def test_files_outside_crates_map_to_their_readers(self):
        p = plan("defaults/prompts/default.md")
        # A Markdown file is docs, wherever it is.
        self.assertEqual(p.whole, set())
        p = plan("defaults/harness/claude.json")
        self.assertEqual(p.whole, {"terrariumd"})
        p = plan("examples/fleet/terrarium.fleet.toml")
        self.assertIn("terrarium-core", p.whole)
        p = plan("harnesses.toml")
        self.assertIn("terrarium-core", p.whole)
        self.assertTrue(p.scripts)

    def test_targets_join_across_files(self):
        p = plan("crates/terrariumd/src/jobs.rs", "crates/terrariumd/tests/board.rs",
                 "dashboard/x.ts")
        self.assertEqual(p.daemon_targets, {"jobs", "board"})
        self.assertTrue(p.dashboard)


class Cli(unittest.TestCase):
    def test_read_changed(self):
        with tempfile.TemporaryDirectory() as d:
            f = os.path.join(d, "changed")
            Path(f).write_text("a.rs\n\nb.rs\n")
            self.assertEqual(affected.read_changed(f), ["a.rs", "b.rs"])
            self.assertIsNone(affected.read_changed(os.path.join(d, "missing")))
            self.assertIsNone(affected.read_changed(None))

    def test_plan_json_shape(self):
        d = plan("crates/terrariumd/src/jobs.rs").to_dict()
        self.assertEqual(json.loads(json.dumps(d))["daemon_targets"], ["jobs"])


if __name__ == "__main__":
    unittest.main()

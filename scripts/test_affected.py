"""Tests for scripts/affected.py: the affected set of a change (TER-100)."""

import contextlib
import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
    "accounts": "terrariumd::accounts::write_credential(s)",
    # Scans the sources of every crate; it names no terrariumd module.
    "no_token_endpoint": "const FORBIDDEN: &[&str] = &[];\n",
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


# The crate sources that embed files from outside their crate, as in the repo.
SOURCES = {
    "crates/terrariumd/src/api/install.rs":
        'const SCRIPT: &str = include_str!("../../../../scripts/install-host.sh");\n'
        'const UNIT: &str = include_str!("../../../terrarium-host/terrarium-host.service");\n',
    "crates/terrariumd/src/probes/zai.rs":
        'const F: &str = include_str!("../../tests/fixtures/probes/zai.json");\n',
    "crates/terrarium-core/src/fleet.rs":
        'const EXAMPLE: &str = include_str!("../../../examples/fleet/terrarium.fleet.toml");\n',
    "crates/terrarium/src/main.rs": 'include_bytes!(concat!("a", "b"))\n',
}
EMBEDDED = affected.embeds(CRATES, SOURCES)


def plan(*files, embedded=None):
    return affected.plan(list(files), CRATES, read=read,
                         common=["crates/terrariumd/tests/common/mod.rs"],
                         embedded=EMBEDDED if embedded is None else embedded)


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
        self.assertEqual(p.daemon_targets, {"jobs", "no_token_endpoint"})
        self.assertEqual(p.clippy(), ["terrariumd"])
        self.assertEqual(affected.commands(p), [
            ["just", "ci::fmt-check"],
            ["cargo", "clippy", "-p", "terrariumd", "--all-targets", "--", "-D", "warnings"],
            ["cargo", "test", "-p", "terrariumd", "--lib", "--bins", "--test", "jobs",
             "--test", "no_token_endpoint", "--no-fail-fast"],
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
        p = plan("docs/api.md", "README.md", "LICENSE", "spikes/x/NOTES.md", "docker/README.md")
        self.assertEqual(affected.commands(p), [["just", "ci::fmt-check"]])

    def test_markdown_in_an_area_follows_the_area(self):
        p = plan("crates/terrarium/README.md")
        self.assertEqual(p.whole, {"terrarium"})
        self.assertTrue(plan("dashboard/README.md").dashboard)
        self.assertTrue(plan("scripts/README.md").scripts)

    def test_scripts_run_scripts_test(self):
        p = plan("scripts/deploy.py")
        self.assertEqual(affected.commands(p),
                         [["just", "ci::fmt-check"], ["just", "ci::scripts-test"]])

    def test_a_module_named_by_tests_runs_those_targets(self):
        self.assertEqual(plan("crates/terrariumd/src/calendar/mod.rs").daemon_targets,
                         {"calendar", "card_reminder", "no_token_endpoint"})
        self.assertEqual(plan("crates/terrariumd/src/hosts.rs").daemon_targets,
                         {"hosts", "jobs", "no_token_endpoint"})

    def test_a_plural_target_name_matches(self):
        self.assertEqual(plan("crates/terrariumd/src/spawn.rs").daemon_targets,
                         {"spawns", "no_token_endpoint"})
        self.assertEqual(plan("crates/terrariumd/src/api/board.rs").daemon_targets,
                         {"board", "no_token_endpoint"})

    def test_a_terrariumd_module_change_runs_the_guard(self):
        # (a) TER-179: src/accounts.rs names targets that do not scan the crates.
        p = plan("crates/terrariumd/src/accounts.rs")
        self.assertFalse(p.full)
        self.assertEqual(p.daemon_targets, {"accounts", "no_token_endpoint"})
        self.assertEqual(p.whole, set())
        self.assertIn("--test", affected.commands(p)[-1])
        self.assertIn("no_token_endpoint", affected.commands(p)[-1])

    def test_a_src_change_of_another_crate_runs_only_the_guard(self):
        # (b) TER-179: terrariumd does not depend on these crates, so terrariumd
        # is in the plan with the guard target alone.
        for f, crate in [("crates/terrarium/src/cli.rs", "terrarium"),
                         ("crates/terrarium-compat/src/lib.rs", "terrarium-compat")]:
            with self.subTest(f=f):
                p = plan(f)
                self.assertFalse(p.full)
                self.assertEqual(p.whole, {crate})
                self.assertEqual(p.daemon_targets, {"no_token_endpoint"})
                self.assertEqual(p.clippy(), sorted({crate, "terrariumd"}))
                self.assertEqual(affected.commands(p)[-1], [
                    "cargo", "test", "-p", "terrariumd", "--lib", "--bins",
                    "--test", "no_token_endpoint", "--no-fail-fast"])

    def test_a_non_src_change_of_another_crate_does_not_run_the_guard(self):
        for f in ["crates/terrarium/tests/cli.rs", "crates/terrarium/README.md",
                  "crates/terrarium/Cargo.toml.bak"]:
            with self.subTest(f=f):
                p = plan(f)
                self.assertEqual(p.whole, {"terrarium"})
                self.assertIsNone(p.daemon_targets)

    def test_the_guard_needs_the_target_on_disk(self):
        # Without the target, a src change runs all of terrariumd's targets.
        crates = affected.workspace(metadata(), ROOT)
        del crates["terrariumd"].tests["no_token_endpoint"]
        p = affected.plan(["crates/terrarium/src/cli.rs"], crates, read=read,
                          common=["crates/terrariumd/tests/common/mod.rs"], embedded={})
        self.assertIn("terrariumd", p.whole)
        self.assertIsNone(p.daemon_targets)

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
        self.assertEqual(p.daemon_targets, {"no_token_endpoint"})

    def test_files_outside_crates_map_to_their_readers(self):
        # terrariumd tests render the prompts under defaults/: a Markdown
        # file there runs all terrariumd targets.
        for f in ["defaults/prompts/default.md", "defaults/skills/x/SKILL.md"]:
            with self.subTest(f=f):
                p = plan(f)
                self.assertEqual(p.whole, {"terrariumd"})
                self.assertIsNone(p.daemon_targets)
        p = plan("defaults/harness/claude.json")
        self.assertEqual(p.whole, {"terrariumd"})
        p = plan("examples/fleet/terrarium.fleet.toml")
        self.assertIn("terrarium-core", p.whole)
        p = plan("harnesses.toml")
        self.assertIn("terrarium-core", p.whole)
        self.assertTrue(p.scripts)

    def test_embeds_finds_files_from_outside_the_crate(self):
        self.assertEqual(EMBEDDED, {
            "scripts/install-host.sh": {"crates/terrariumd/src/api/install.rs"},
            "crates/terrarium-host/terrarium-host.service": {"crates/terrariumd/src/api/install.rs"},
            "examples/fleet/terrarium.fleet.toml": {"crates/terrarium-core/src/fleet.rs"},
        })

    def test_an_embedded_script_runs_scripts_test_and_its_embedder(self):
        p = plan("scripts/install-host.sh")
        self.assertTrue(p.scripts)
        # src/api/install.rs is module `install`: no target matches, so all.
        self.assertIn("terrariumd", p.whole)
        self.assertIsNone(p.daemon_targets)
        self.assertIn(["just", "ci::scripts-test"], affected.commands(p))
        self.assertIn("terrariumd", p.clippy())
        # Without the embed, a scripts change runs only the scripts tests.
        p = plan("scripts/install-host.sh", embedded={})
        self.assertEqual(p.whole, set())

    def test_an_embedded_file_of_another_crate_runs_the_embedder(self):
        p = plan("crates/terrarium-host/terrarium-host.service")
        self.assertIn("terrarium-host", p.whole)
        self.assertIn("terrariumd", p.whole)

    def test_targets_join_across_files(self):
        p = plan("crates/terrariumd/src/jobs.rs", "crates/terrariumd/tests/board.rs",
                 "dashboard/x.ts")
        self.assertEqual(p.daemon_targets, {"jobs", "board", "no_token_endpoint"})
        self.assertTrue(p.dashboard)


class Cli(unittest.TestCase):
    def test_read_changed(self):
        with tempfile.TemporaryDirectory() as d:
            f = os.path.join(d, "changed")
            Path(f).write_text("a.rs\n\nb.rs\n")
            self.assertEqual(affected.read_changed(f), ["a.rs", "b.rs"])
            self.assertIsNone(affected.read_changed(os.path.join(d, "missing")))
            self.assertIsNone(affected.read_changed(None))

    def test_run_stops_at_the_first_failure(self):
        calls = []

        def call(cmd, **kw):
            calls.append(cmd)
            return 3 if cmd[:2] == ["just", "ci::scripts-test"] else 0

        cmds = [["just", "ci::fmt-check"], ["just", "ci::scripts-test"], ["just", "ci::dashboard-check"]]
        with mock.patch.object(affected.subprocess, "call", call), \
                mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("RESULTS_DIR", None)
            self.assertEqual(affected.run(cmds), 3)
        self.assertEqual(calls, cmds[:2])

    def test_run_routes_rust_tests_through_shared_runner(self):
        seen = []

        def call(cmd, stdout=None, env=None):
            seen.append((cmd, env.get("RUSTC_BOOTSTRAP") if env else None))
            if stdout is not None:
                stdout.write(b'{"type":"suite"}\n')
            return 0

        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(affected.subprocess, "call", call), \
                mock.patch.dict(os.environ, {"RESULTS_DIR": d}):
            cmds = [["just", "ci::fmt-check"],
                    ["cargo", "test", "-p", "terrarium", "--no-fail-fast"],
                    ["cargo", "test", "-p", "terrariumd", "--lib", "--bins", "--no-fail-fast"]]
            self.assertEqual(affected.run(cmds), 0)
        self.assertEqual(seen[0], (["just", "ci::fmt-check"], None))
        self.assertEqual(seen[1][0][1:], ["scripts/rust_tests.py", "-p", "terrarium", "--no-fail-fast"])
        self.assertEqual(seen[2][0][1:], ["scripts/rust_tests.py", "-p", "terrariumd", "--lib", "--bins", "--no-fail-fast"])

    def test_main_run_clears_old_rust_reports(self):
        with tempfile.TemporaryDirectory() as d:
            changed = Path(d, "changed")
            changed.write_text("docs/api.md\n")
            results = Path(d, "results")
            results.mkdir()
            Path(results, "rust.json").write_text("stale\n")
            calls = []
            meta = json.dumps({"packages": [], "workspace_members": []})
            with mock.patch.object(affected.subprocess, "run",
                                   return_value=mock.Mock(stdout=meta)), \
                    mock.patch.object(affected.subprocess, "call",
                                      lambda cmd, **kw: calls.append(cmd) or 0), \
                    mock.patch.dict(os.environ, {"RESULTS_DIR": str(results)}), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(affected.main(["--files", str(changed), "--run"]), 0)
            self.assertFalse(Path(results, "rust.json").exists())
            self.assertEqual(calls, [["just", "ci::fmt-check"]])

    def test_plan_json_shape(self):
        d = plan("crates/terrariumd/src/jobs.rs").to_dict()
        self.assertEqual(json.loads(json.dumps(d))["daemon_targets"], ["jobs", "no_token_endpoint"])


if __name__ == "__main__":
    unittest.main()

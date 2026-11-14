"""Checks for the unprivileged rerun of the selected tests (TER-579, TER-611)."""

import json
import os
import tempfile
import unittest
from unittest import mock

import unprivileged_tests as ut


HOST = "/src/r/crates/terrarium-host/Cargo.toml"
DAEMON = "/src/r/crates/terrariumd/Cargo.toml"
OTHER = "/src/r/crates/terrarium-cli/Cargo.toml"
NAMES = {HOST: "terrarium-host", DAEMON: "terrariumd", OTHER: "terrarium-cli"}


def artifact(exe, test=True, manifest=HOST, kind=("lib",), name="terrarium_host"):
    return json.dumps({"reason": "compiler-artifact", "executable": exe,
                       "manifest_path": manifest, "profile": {"test": test},
                       "target": {"kind": list(kind), "name": name}})


class UnprivilegedTests(unittest.TestCase):
    def test_only_test_executables_are_listed(self):
        out = "\n".join([
            "Compiling terrarium-host",
            artifact("/t/deps/terrarium_host-1"),
            artifact("/t/debug/terrarium-host", test=False),
            artifact(None),
            json.dumps({"reason": "build-finished", "success": True}),
            artifact("/t/deps/native_helper-2"),
        ])
        self.assertEqual(ut.test_binaries(out, NAMES, ut.SELECT), [
            ("/t/deps/terrarium_host-1", "/src/r/crates/terrarium-host", False),
            ("/t/deps/native_helper-2", "/src/r/crates/terrarium-host", False),
        ])

    def test_terrariumd_runs_only_its_lib_and_it_targets(self):
        out = "\n".join([
            artifact("/t/deps/terrariumd-1", manifest=DAEMON, name="terrariumd"),
            artifact("/t/deps/it-2", manifest=DAEMON, kind=("test",), name="it"),
            artifact("/t/deps/settings_env-3", manifest=DAEMON, kind=("test",),
                     name="settings_env"),
            artifact("/t/deps/terrariumd-4", manifest=DAEMON, kind=("bin",), name="terrariumd"),
            artifact("/t/deps/terrarium_cli-5", manifest=OTHER, name="terrarium_cli"),
            artifact("/t/deps/unknown-6", manifest="/elsewhere/Cargo.toml"),
        ])
        self.assertEqual(ut.test_binaries(out, NAMES, ut.SELECT), [
            ("/t/deps/terrariumd-1", "/src/r/crates/terrariumd", True),
            ("/t/deps/it-2", "/src/r/crates/terrariumd", True),
        ])

    def test_the_it_and_lib_binaries_never_run_beside_another(self):
        # Cargo's order as in a real build: host binaries around the terrariumd ones.
        out = "\n".join([
            artifact("/t/deps/terrarium_host-1"),
            artifact("/t/deps/reservation_capacity-2", kind=("test",), name="reservation_capacity"),
            artifact("/t/deps/it-3", manifest=DAEMON, kind=("test",), name="it"),
            artifact("/t/deps/limits_systemd-4", kind=("test",), name="limits_systemd"),
            artifact("/t/deps/terrariumd-5", manifest=DAEMON, name="terrariumd"),
            artifact("/t/deps/stub_daemon-6", kind=("test",), name="stub_daemon"),
            artifact("/t/deps/native_helper-7", kind=("test",), name="native_helper"),
            artifact("/t/deps/version_output-8", kind=("test",), name="version_output"),
        ])
        bins = ut.test_binaries(out, NAMES, ut.SELECT)
        plan = ut.groups(bins)
        exes = [[bins[n][0] for n in group] for group in plan]
        self.assertIn(["/t/deps/it-3"], exes)
        self.assertIn(["/t/deps/terrariumd-5"], exes)
        for group in exes:
            self.assertLessEqual(len(group), ut.WIDTH)
            if "/t/deps/it-3" in group or "/t/deps/terrariumd-5" in group:
                self.assertEqual(len(group), 1, group)
        # Each binary runs exactly once.
        self.assertEqual(sorted(n for group in plan for n in group), list(range(len(bins))))

    def test_groups_keep_the_order_and_fill_to_the_width(self):
        bins = [("a", "", False), ("b", "", False), ("it", "", True), ("c", "", False),
                ("d", "", False), ("e", "", False)]
        self.assertEqual(ut.groups(bins, width=2), [[0, 1], [2], [3, 4], [5]])
        self.assertEqual(ut.groups(bins, width=1), [[0], [1], [2], [3], [4], [5]])

    def test_package_names_come_from_cargo_metadata(self):
        meta = json.dumps({"packages": [{"name": "terrariumd", "manifest_path": DAEMON}]})
        self.assertEqual(ut.package_names(meta), {DAEMON: "terrariumd"})

    def test_the_child_gets_its_home_and_not_the_job_env(self):
        env = {"PATH": "/usr/bin", "TERRARIUM_TOKEN": "x", "HOME": "/root", "RUST_BACKTRACE": "1",
               "TMPDIR": "/root/tmp", "RUSTUP_HOME": "/r", "RUSTUP_TOOLCHAIN": "1.97.1",
               "CARGO_HOME": "/c", "CARGO_TARGET_DIR": "/t", "RUST_TEST_THREADS": "8"}
        with mock.patch.dict(os.environ, env, clear=True):
            child = ut.child_env("/tmp/h")
        # No TMPDIR: the tests use the shared /tmp, like the release CI (TER-575).
        self.assertEqual(child, {"PATH": "/usr/bin", "RUST_BACKTRACE": "1", "RUSTUP_HOME": "/r",
                                 "RUSTUP_TOOLCHAIN": "1.97.1", "CARGO_HOME": "/c",
                                 "RUST_TEST_THREADS": "8", "HOME": "/tmp/h", "USER": "ci", "LOGNAME": "ci"})

    def test_each_binary_gets_a_fresh_private_home(self):
        with tempfile.TemporaryDirectory() as root, \
                mock.patch.object(ut.os, "chown") as chown:
            homes = [ut.private_home(root, n, 1001) for n in (0, 1)]
            for d in homes:
                self.assertTrue(os.path.isdir(d))
                self.assertEqual(os.stat(d).st_mode & 0o777, 0o700)
                self.assertEqual(os.path.dirname(d), root)
        self.assertEqual(len(set(homes)), 2)
        self.assertEqual([c.args[1:] for c in chown.call_args_list], [(1001, 1001)] * 2)

    def test_setpriv_drops_uid_gid_and_groups(self):
        self.assertEqual(ut.setpriv(1001), [
            "setpriv", "--reuid=1001", "--regid=1001", "--clear-groups", "--"])

    def test_as_root_without_a_passwd_entry_it_fails_before_the_build(self):
        with mock.patch.object(ut.os, "geteuid", return_value=0), \
                mock.patch.object(ut, "has_user", return_value=False), \
                mock.patch.object(ut.subprocess, "run") as run:
            self.assertEqual(ut.run(), 1)
        run.assert_not_called()

    def test_has_user_reads_the_passwd_database(self):
        with mock.patch.object(ut.pwd, "getpwuid", side_effect=KeyError(1001)):
            self.assertFalse(ut.has_user(1001))
        with mock.patch.object(ut.pwd, "getpwuid", return_value=object()):
            self.assertTrue(ut.has_user(1001))

    def test_without_root_it_skips(self):
        with mock.patch.object(ut.os, "geteuid", return_value=1001), \
                mock.patch.object(ut.subprocess, "run") as run:
            self.assertEqual(ut.run(), 0)
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()

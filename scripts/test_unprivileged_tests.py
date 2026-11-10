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
            ("/t/deps/terrarium_host-1", "/src/r/crates/terrarium-host"),
            ("/t/deps/native_helper-2", "/src/r/crates/terrarium-host"),
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
            ("/t/deps/terrariumd-1", "/src/r/crates/terrariumd"),
            ("/t/deps/it-2", "/src/r/crates/terrariumd"),
        ])

    def test_package_names_come_from_cargo_metadata(self):
        meta = json.dumps({"packages": [{"name": "terrariumd", "manifest_path": DAEMON}]})
        self.assertEqual(ut.package_names(meta), {DAEMON: "terrariumd"})

    def test_the_child_gets_its_dirs_and_not_the_job_env(self):
        env = {"PATH": "/usr/bin", "TERRARIUM_TOKEN": "x", "HOME": "/root", "RUST_BACKTRACE": "1",
               "TMPDIR": "/tmp", "RUSTUP_HOME": "/r", "RUSTUP_TOOLCHAIN": "1.97.1",
               "CARGO_HOME": "/c", "CARGO_TARGET_DIR": "/t"}
        with mock.patch.dict(os.environ, env, clear=True):
            child = ut.child_env("/tmp/h", "/tmp/t")
        self.assertEqual(child, {"PATH": "/usr/bin", "RUST_BACKTRACE": "1", "RUSTUP_HOME": "/r",
                                 "RUSTUP_TOOLCHAIN": "1.97.1", "CARGO_HOME": "/c",
                                 "HOME": "/tmp/h", "TMPDIR": "/tmp/t", "USER": "ci",
                                 "LOGNAME": "ci"})

    def test_each_binary_gets_fresh_private_dirs(self):
        with tempfile.TemporaryDirectory() as root, \
                mock.patch.object(ut.os, "chown") as chown:
            first = ut.private_dirs(root, 0, 1001)
            second = ut.private_dirs(root, 1, 1001)
            for d in first + second:
                self.assertTrue(os.path.isdir(d))
                self.assertEqual(os.stat(d).st_mode & 0o777, 0o700)
                self.assertTrue(d.startswith(root + os.sep))
        self.assertEqual(len(set(first + second)), 4)
        self.assertEqual([c.args[1:] for c in chown.call_args_list], [(1001, 1001)] * 4)

    def test_setpriv_drops_uid_gid_and_groups(self):
        self.assertEqual(ut.setpriv(1001), [
            "setpriv", "--reuid=1001", "--regid=1001", "--clear-groups", "--"])

    def test_without_root_it_skips(self):
        with mock.patch.object(ut.os, "geteuid", return_value=1001), \
                mock.patch.object(ut.subprocess, "run") as run:
            self.assertEqual(ut.run(), 0)
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()

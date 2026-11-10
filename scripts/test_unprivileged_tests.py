"""Checks for the unprivileged rerun of the terrarium-host tests (TER-579)."""

import json
import os
import unittest
from unittest import mock

import unprivileged_tests as ut


def artifact(exe, test=True, manifest="/src/r/crates/terrarium-host/Cargo.toml"):
    return json.dumps({"reason": "compiler-artifact", "executable": exe,
                       "manifest_path": manifest, "profile": {"test": test}})


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
        self.assertEqual(ut.test_binaries(out), [
            ("/t/deps/terrarium_host-1", "/src/r/crates/terrarium-host"),
            ("/t/deps/native_helper-2", "/src/r/crates/terrarium-host"),
        ])

    def test_the_child_gets_its_home_and_not_the_job_env(self):
        env = {"PATH": "/usr/bin", "TERRARIUM_TOKEN": "x", "HOME": "/root", "RUST_BACKTRACE": "1"}
        with mock.patch.dict(os.environ, env, clear=True):
            child = ut.child_env("/tmp/h")
        self.assertEqual(child, {"PATH": "/usr/bin", "RUST_BACKTRACE": "1", "HOME": "/tmp/h",
                                 "USER": "ci", "LOGNAME": "ci"})

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

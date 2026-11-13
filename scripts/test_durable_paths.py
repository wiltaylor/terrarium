"""Tests for the durable spool root walk (crates/terrariumd/src/executor/durable_paths.py).

TER-1129: on an ssh target without work_root, a root-owned `/` and no `/src`,
the spool falls back to ~/.terrarium-work, or fails with an error that names
--work-root.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

import errno
import importlib.util
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

PATHS = Path(__file__).resolve().parent.parent / "crates/terrariumd/src/executor/durable_paths.py"


def load():
    spec = importlib.util.spec_from_file_location("durable_paths", PATHS)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeRoot:
    """A temporary `/` with /home/build. When `locked`, the agent user may
    not create anything directly in it (a root-owned `/`). The check uses
    the directory identity, so it also holds when the tests run as root."""

    def __init__(self, test):
        self.temp = tempfile.TemporaryDirectory()
        test.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        self.fds = []
        test.addCleanup(lambda: [os.close(fd) for fd in self.fds])
        (self.path / "home/build").mkdir(parents=True)
        self.locked = {self.ino(self.path)}
        self.module = load()
        self.module.ROOT = str(self.path)
        real = os.mkdir

        def mkdir(name, mode=0o777, *, dir_fd=None):
            # Like Linux: an existing name is EEXIST before any permission check.
            if dir_fd is not None and os.fstat(dir_fd).st_ino in self.locked:
                if os.path.lexists(os.path.join(f"/proc/self/fd/{dir_fd}", name)):
                    raise FileExistsError(errno.EEXIST, "File exists", name)
                raise PermissionError(errno.EACCES, "Permission denied", name)
            return real(name, mode, dir_fd=dir_fd)

        patcher = mock.patch.object(self.module.os, "mkdir", mkdir)
        patcher.start()
        test.addCleanup(patcher.stop)
        env = mock.patch.dict(os.environ, {"HOME": "/home/build"})
        env.start()
        test.addCleanup(env.stop)

    @staticmethod
    def ino(path):
        return os.stat(path).st_ino

    def lock(self, rel):
        self.locked.add(self.ino(self.path / rel))

    def resolve(self, mode="run", fallback="1", card="0", token="tok"):
        fd = self.module.resolve("/src/terrarium", card, token, mode, fallback, "box")
        self.fds.append(fd)
        return os.fstat(fd)


class SpoolFallback(unittest.TestCase):
    def setUp(self):
        self.fs = FakeRoot(self)
        self.home = self.fs.path / "home/build/.terrarium-work"

    def test_root_owned_slash_and_missing_src_use_the_home_fallback(self):
        meta = self.fs.resolve()
        self.assertEqual(meta.st_ino, self.fs.ino(self.home))
        self.assertEqual(stat.S_IMODE(os.stat(self.home).st_mode), 0o700)
        self.assertFalse((self.fs.path / "src").exists())

    def test_src_exists_but_is_not_writable(self):
        (self.fs.path / "src").mkdir()
        self.fs.lock("src")
        self.assertEqual(self.fs.resolve().st_ino, self.fs.ino(self.home))
        self.assertFalse((self.fs.path / "src/terrarium").exists())

    def test_writable_default_is_still_used_first(self):
        self.fs.locked.clear()
        meta = self.fs.resolve()
        self.assertEqual(meta.st_ino, self.fs.ino(self.fs.path / "src/terrarium"))
        self.assertFalse(self.home.exists())

    def test_a_claim_in_the_fallback_stays_there_when_the_default_becomes_writable(self):
        self.fs.resolve()
        (self.home / ".terrarium-runs/tok").mkdir(parents=True)
        self.fs.locked.clear()
        self.assertEqual(self.fs.resolve().st_ino, self.fs.ino(self.home))
        self.assertFalse((self.fs.path / "src").exists())

    def test_cancel_finds_the_claim_in_the_fallback(self):
        self.fs.resolve()
        (self.home / ".terrarium-runs/tok").mkdir(parents=True)
        self.assertEqual(self.fs.resolve(mode="cancel").st_ino, self.fs.ino(self.home))

    def test_set_work_root_never_falls_back_and_names_the_flag(self):
        with self.assertRaises(RuntimeError) as caught:
            self.fs.resolve(fallback="0")
        message = str(caught.exception)
        self.assertIn("work_root /src/terrarium is not writable", message)
        self.assertIn("terrarium admin target edit box --work-root <absolute path>", message)
        self.assertFalse(self.home.exists())

    def test_failed_fallback_names_both_paths_and_the_flag(self):
        self.fs.lock("home/build")
        with self.assertRaises(RuntimeError) as caught:
            self.fs.resolve()
        message = str(caught.exception)
        self.assertIn("/src/terrarium", message)
        self.assertIn("/home/build/.terrarium-work", message)
        self.assertIn("--work-root", message)

    def test_symlinked_fallback_is_refused(self):
        outside = self.fs.path / "outside"
        outside.mkdir(mode=0o700)
        self.home.symlink_to(outside)
        with self.assertRaises(RuntimeError):
            self.fs.resolve()
        self.assertEqual(list(outside.iterdir()), [])

    def test_group_writable_fallback_is_refused(self):
        self.home.mkdir()
        os.chmod(self.home, 0o770)
        with self.assertRaisesRegex(RuntimeError, "group/other writable"):
            self.fs.resolve()

    def test_no_absolute_home_names_the_flag(self):
        with mock.patch.dict(os.environ, {"HOME": "relative/home"}):
            with self.assertRaises(RuntimeError) as caught:
                self.fs.resolve()
        message = str(caught.exception)
        self.assertIn("cannot use /src/terrarium", message)
        self.assertIn("no absolute home for the fallback", message)
        self.assertIn("--work-root <absolute path>", message)
        self.assertFalse(self.home.exists())

    def test_missing_roots_outside_a_run_create_nothing(self):
        self.fs.locked.clear()
        for mode in ("cancel", "read"):
            with self.assertRaises(RuntimeError) as caught:
                self.fs.resolve(mode=mode)
            message = str(caught.exception)
            self.assertIn("/src/terrarium", message)
            self.assertIn("/home/build/.terrarium-work", message)
            self.assertIn("--work-root", message)
        self.assertFalse((self.fs.path / "src").exists())
        self.assertFalse(self.home.exists())

    def test_missing_set_work_root_outside_a_run_stays_enoent(self):
        with self.assertRaises(OSError) as caught:
            self.fs.resolve(mode="cancel", fallback="0")
        self.assertEqual(caught.exception.errno, errno.ENOENT)

    def test_symlinked_home_component_is_named(self):
        # /home -> var/home, as on Fedora Atomic or FreeBSD.
        (self.fs.path / "var").mkdir()
        (self.fs.path / "home").rename(self.fs.path / "var/home")
        (self.fs.path / "home").symlink_to("var/home")
        with self.assertRaises(RuntimeError) as caught:
            self.fs.resolve()
        message = str(caught.exception)
        self.assertIn("/home is a symlink and the fallback refuses symlinked components", message)
        self.assertIn("--work-root", message)
        self.assertFalse((self.fs.path / "var/home/build/.terrarium-work").exists())

    def assert_refused(self, label, reason, fallback):
        with self.assertRaises(RuntimeError) as caught:
            self.fs.resolve(fallback=fallback)
        message = str(caught.exception)
        self.assertIn(label + " is refused", message)
        self.assertIn(reason, message)
        self.assertIn("terrarium admin target edit box --work-root <absolute path>", message)
        self.assertFalse(self.home.exists())

    def test_refused_roots_never_fall_back_and_name_the_flag(self):
        outside = self.fs.path / "outside"
        outside.mkdir(mode=0o700)
        src = self.fs.path / "src"
        src.mkdir()
        for fallback, label in (("1", "/src/terrarium"), ("0", "work_root /src/terrarium")):
            with self.subTest(fallback=fallback, case="symlink"):
                (src / "terrarium").symlink_to(outside)
                self.assert_refused(label, "terrarium'", fallback)
                (src / "terrarium").unlink()
            (src / "terrarium").mkdir(mode=0o700)
            with self.subTest(fallback=fallback, case="group writable"):
                os.chmod(src / "terrarium", 0o770)
                self.assert_refused(label, "terrarium: group/other writable", fallback)
            with self.subTest(fallback=fallback, case="foreign owner"):
                os.chmod(src / "terrarium", 0o700)
                with mock.patch.object(self.fs.module.os, "geteuid", lambda: os.getuid() + 1):
                    self.assert_refused(label, "terrarium: foreign owner", fallback)
            (src / "terrarium").rmdir()
        self.assertEqual(list(outside.iterdir()), [])

    def test_card_roots_never_fall_back(self):
        with self.assertRaises(OSError) as caught:
            self.fs.resolve(card="1")
        self.assertEqual(caught.exception.errno, errno.ENOENT)
        self.assertFalse(self.home.exists())

    def test_main_creates_a_private_spool_in_the_fallback(self):
        cwd = os.getcwd()
        self.addCleanup(os.chdir, cwd)
        argv = ["x", "/src/terrarium", "owner", "0", "tok", "run", "true", "1", "box"]
        launched = []
        module = self.fs.module
        with mock.patch.object(module.sys, "argv", argv), \
                mock.patch.object(module.os, "dup2"), \
                mock.patch.object(module.os, "execv", lambda *a: launched.append(a)), \
                mock.patch.dict(os.environ):
            module.main()
        self.assertEqual(len(launched), 1)
        runs = self.home / ".terrarium-runs"
        for path in (self.home, runs, runs / "tok"):
            self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o700, path)
        self.assertEqual((runs / ".terrarium-owner").read_text(), "owner")
        self.assertEqual((runs / "tok/.terrarium-owner").read_text(), "tok")
        self.assertEqual(stat.S_IMODE(os.stat(runs / ".terrarium-owner").st_mode), 0o600)


if __name__ == "__main__":
    unittest.main()

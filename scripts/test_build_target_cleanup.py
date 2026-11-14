"""The build target cleanup (TER-1282): selection, dry run, apply, cap, floor."""

import fcntl
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import build_target_cleanup as btc


def run(*argv):
    out = io.StringIO()
    rc = btc.main(list(argv), out=out)
    return rc, out.getvalue()


class Tree(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.root = base / "src"
        self.proc = base / "proc"
        self.outside = base / "outside"
        for d in (self.root, self.proc, self.outside):
            d.mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def mk(self, name, size=4096):
        p = self.root / name
        p.mkdir()
        (p / "blob").write_bytes(b"x" * size)
        return p

    def mkfile(self, name):
        p = self.root / name
        p.write_text("log\n")
        return p

    def fake_proc(self, pid, cwd=None, target=None):
        d = self.proc / str(pid)
        d.mkdir()
        if cwd:
            os.symlink(cwd, d / "cwd")
        env = b"PATH=/bin\0" + (b"CARGO_TARGET_DIR=" + str(target).encode() + b"\0" if target else b"")
        (d / "environ").write_bytes(env)

    def clean(self, *extra):
        return run("--root", str(self.root), "--json", "clean", "--proc", str(self.proc), *extra)


class CleanSelection(Tree):
    def layout(self):
        names = {
            # bound agent grok-1192 of TER-1192: every grok-1192 entry
            "grok-1192-TER-1192-dev": "remove",
            "grok-1192-TER-1192-8f87cd64": "remove",
            "grok-1192-TER-1192-8f87cd64-results": "remove",
            "grok-1192-h9-x": "remove",
            # bound agent with a lowercase card in the name
            "win-274-ter-274-18f80670": "remove",
            "win-274-target": "remove",
            # a long-lived worker: only the card, target and dev entries
            "worker-TER-5-abc": "remove",
            "worker-target-2": "remove",
            "worker-dev": "remove",
            "worker-TER-6-abc": "absent",
            "worker-notes": "absent",
            # other agents and the shared entries
            "grok-11920-TER-11920-x": "absent",
            "svc-1264-dev": "keep",
            "terrarium-cardcheck-target-2": "absent",
            "terrarium-dev-target": "absent",
            "terrarium": "absent",
        }
        for n in names:
            self.mk(n)
        self.mkfile("grok-1192-after.log")
        names["grok-1192-after.log"] = "remove"
        self.mkfile("terrarium-cardcheck-2.lock")
        names["terrarium-cardcheck-2.lock"] = "absent"
        return names

    def args(self):
        return ["--live", "lead,svc-1264", "--done", "grok-1192:TER-1192", "--done", "win-274:TER-274",
                "--done", "worker:TER-5", "--done", "svc-1264:TER-1264"]

    def test_dry_run_selects_and_deletes_nothing(self):
        names = self.layout()
        rc, out = self.clean(*self.args())
        self.assertEqual(rc, 0)
        doc = json.loads(out)
        self.assertFalse(doc["apply"])
        got = {Path(e["path"]).name: e["action"] for e in doc["entries"]}
        want = {n: a for n, a in names.items() if a != "absent"}
        self.assertEqual(got, want)
        for n in names:
            self.assertTrue((self.root / n).exists(), n)
        removed = [e for e in doc["entries"] if e["action"] == "remove"]
        self.assertEqual(doc["freed_bytes"], sum(e["bytes"] for e in removed))
        self.assertGreater(doc["freed_bytes"], 0)
        live = next(e for e in doc["entries"] if e["path"].endswith("svc-1264-dev"))
        self.assertEqual(live["reason"], "live agent svc-1264")

    def test_apply_removes_only_the_selection(self):
        names = self.layout()
        rc, out = self.clean(*self.args(), "--apply")
        self.assertEqual(rc, 0)
        doc = json.loads(out)
        self.assertTrue(doc["apply"])
        for n, a in names.items():
            self.assertEqual((self.root / n).exists(), a != "remove", n)

    def test_text_log_names_the_freed_space(self):
        self.mk("grok-1192-dev", size=8192)
        rc, out = run("--root", str(self.root), "clean", "--proc", str(self.proc),
                      "--live", "lead", "--done", "grok-1192:TER-1192")
        self.assertEqual(rc, 0)
        self.assertIn("remove", out)
        self.assertIn("grok-1192-dev", out)
        self.assertIn("dry run, would free:", out.splitlines()[-1])

    def test_symlink_is_kept_and_its_target_outside_untouched(self):
        (self.outside / "keepme").write_text("data")
        os.symlink(self.outside, self.root / "grok-1192-link")
        rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 0)
        entry = json.loads(out)["entries"][0]
        self.assertEqual((entry["action"], entry["reason"]), ("keep", "symlink"))
        self.assertTrue((self.outside / "keepme").exists())
        self.assertTrue((self.root / "grok-1192-link").is_symlink())

    def test_running_process_cwd_or_target_dir_keeps_the_entry(self):
        a = self.mk("grok-1192-TER-1192-aaa")
        b = self.mk("grok-1192-target")
        c = self.mk("grok-1192-TER-1192-ccc")
        (a / "sub").mkdir()
        self.fake_proc(100, cwd=a / "sub")
        self.fake_proc(101, target=b)
        rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 0)
        self.assertTrue(a.exists())
        self.assertTrue(b.exists())
        self.assertFalse(c.exists())
        kept = {Path(e["path"]).name for e in json.loads(out)["entries"] if e["action"] == "keep"}
        self.assertEqual(kept, {a.name, b.name})

    def test_process_that_starts_before_the_delete_keeps_the_entry(self):
        a = self.mk("grok-1192-dev")
        calls = []
        real = btc.busy_paths

        def late(proc):
            calls.append(1)
            return real(proc) if len(calls) == 1 else ([a], [])

        with mock.patch.object(btc, "busy_paths", late):
            rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 0)
        self.assertTrue(a.exists())
        self.assertEqual(json.loads(out)["freed_bytes"], 0)

    def test_live_list_is_required_and_names_are_checked(self):
        with self.assertRaises(SystemExit):
            run("--root", str(self.root), "clean", "--done", "grok-1192:TER-1192")
        for bad in ("grok-1192", "../x:TER-1", "grok:../x", "terrarium-cardcheck:TER-2", ":TER-1"):
            rc, _ = self.clean("--live", "lead", "--done", bad)
            self.assertEqual(rc, 2, bad)
        for live in ("a/b", "", " ", " , "):
            rc, _ = self.clean("--live", live, "--done", "grok-1192:TER-1192")
            self.assertEqual(rc, 2, repr(live))

    def test_unreadable_process_keeps_everything_and_says_so(self):
        a = self.mk("grok-1192-dev")
        (self.proc / "200").mkdir()  # no cwd link: readlink fails
        (self.proc / "200" / "status").write_text("Name:\tjanitor\nUmask:\t0077\nUid:\t1001\t1001\t1001\t1001\n")
        real = os.readlink

        def readlink(path):
            if str(path).startswith(str(self.proc / "200")):
                raise PermissionError(13, "Permission denied")
            return real(path)

        with mock.patch.object(btc.os, "readlink", readlink):
            rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 0)
        self.assertTrue(a.exists())
        entry = json.loads(out)["entries"][0]
        self.assertEqual(entry["action"], "keep")
        self.assertIn("cannot read the cwd or environ of 1 processes", entry["reason"])
        self.assertIn("200 janitor uid 1001", entry["reason"])
        with mock.patch.object(btc.os, "readlink", readlink):
            rc, _ = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--ignore-pid", "x1")
            self.assertEqual(rc, 2)
            rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply", "--ignore-pid", "200")
        self.assertEqual(rc, 0)
        self.assertFalse(a.exists())

    def test_ended_process_is_ignored(self):
        a = self.mk("grok-1192-dev")
        (self.proc / "201").mkdir()  # no cwd link: the process ended
        rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 0)
        self.assertFalse(a.exists())

    def test_delete_error_goes_on_and_exits_1(self):
        a = self.mk("grok-1192-TER-1192-a")
        b = self.mk("grok-1192-TER-1192-b")
        c = self.mk("grok-1192-TER-1192-c")
        real = btc.remove

        def remove(path):
            if path == b:
                raise PermissionError(13, "Permission denied", str(path))
            real(path)

        with mock.patch.object(btc, "remove", remove):
            rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 1)
        doc = json.loads(out)
        acts = {Path(e["path"]).name: e["action"] for e in doc["entries"]}
        self.assertEqual(acts, {a.name: "remove", b.name: "error", c.name: "remove"})
        self.assertEqual(doc["errors"], 1)
        self.assertFalse(a.exists())
        self.assertTrue(b.exists())
        self.assertFalse(c.exists())
        err = next(e for e in doc["entries"] if e["action"] == "error")
        self.assertIn("PermissionError", err["reason"])
        with mock.patch.object(btc, "remove", remove):
            b2 = self.mk("grok-1192-TER-1192-b2")
            rc, text = run("--root", str(self.root), "clean", "--proc", str(self.proc),
                           "--live", "lead", "--done", "grok-1192:TER-1192", "--apply")
        self.assertEqual(rc, 1)
        self.assertIn("errors: 1 entries could not be removed", text)
        self.assertFalse(b2.exists())

    def test_refuses_slash_and_relative_roots(self):
        for root in ("/", "src", str(Path.home())):
            rc, _ = run("--root", root, "clean", "--live", "lead", "--done", "grok-1192:TER-1192")
            self.assertEqual(rc, 2, root)

    def test_bound_rule_needs_the_card_number(self):
        self.mk("grok-1192-other")
        self.mk("grok-1192-TER-7-x")
        rc, out = self.clean("--live", "lead", "--done", "grok-1192:TER-7")
        got = {Path(e["path"]).name for e in json.loads(out)["entries"]}
        self.assertEqual(got, {"grok-1192-TER-7-x"})


class Cap(Tree):
    def target(self, name, size):
        t = self.root / name
        (t / "debug" / "deps").mkdir(parents=True)
        (t / "debug" / ".cargo-lock").write_text("")
        (t / "debug" / "deps" / "big").write_bytes(b"x" * size)
        (t / "CACHEDIR.TAG").write_text("tag")
        return t

    def cap(self, *extra):
        return run("--root", str(self.root), "--json", "cap", "--proc", str(self.proc),
                   "--cap-gib", str(64 * 1024 / btc.GIB), *extra)

    def test_over_cap_cleaned_only_with_apply_under_lock(self):
        big = self.target("terrarium-cardcheck-target-2", 200 * 1024)
        small = self.target("terrarium-cardcheck-target-3", 1024)
        dev = self.target("terrarium-dev-target", 200 * 1024)
        for s in (2, 3):
            (self.root / f"terrarium-cardcheck-{s}.lock").write_text("")
        rc, out = self.cap()
        self.assertEqual(rc, 0)
        acts = {Path(e["path"]).name: e["action"] for e in json.loads(out)["entries"]}
        self.assertEqual(acts, {big.name: "clean", small.name: "keep", dev.name: "clean"})
        self.assertTrue((big / "debug" / "deps" / "big").exists())
        rc, out = self.cap("--apply")
        self.assertEqual(rc, 0)
        for t in (big, dev):
            self.assertTrue(t.is_dir())
            self.assertFalse((t / "debug" / "deps").exists())
            self.assertTrue((t / "debug" / ".cargo-lock").exists())
            self.assertTrue((t / "CACHEDIR.TAG").exists())
        self.assertTrue((small / "debug" / "deps" / "big").exists())
        self.assertGreater(json.loads(out)["freed_bytes"], 0)

    def test_held_slot_or_cargo_lock_skips_the_dir(self):
        slot_t = self.target("terrarium-cardcheck-target-2", 200 * 1024)
        cargo_t = self.target("terrarium-queue-target-1", 200 * 1024)
        (self.root / "terrarium-cardcheck-2.lock").write_text("")
        (self.root / "terrarium-queue-1.lock").write_text("")
        with open(self.root / "terrarium-cardcheck-2.lock") as a, open(cargo_t / "debug" / ".cargo-lock") as b:
            fcntl.flock(a, fcntl.LOCK_EX)
            fcntl.flock(b, fcntl.LOCK_EX)
            rc, out = self.cap("--apply")
        self.assertEqual(rc, 0)
        for e in json.loads(out)["entries"]:
            self.assertEqual((e["action"], e["reason"]), ("keep", "over the cap, a lock is held"))
        self.assertTrue((slot_t / "debug" / "deps" / "big").exists())
        self.assertTrue((cargo_t / "debug" / "deps" / "big").exists())

    def test_dev_target_needs_its_own_lock_free(self):
        dev = self.target("terrarium-dev-target", 200 * 1024)
        lock = self.root / "terrarium-dev-target.lock"
        self.assertFalse(lock.exists())
        rc, _ = self.cap()  # a dry run makes no lock file
        self.assertFalse(lock.exists())
        lock.write_text("")
        with open(lock) as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            rc, out = self.cap("--apply")
        self.assertEqual(json.loads(out)["entries"][0]["reason"], "over the cap, a lock is held")
        self.assertTrue((dev / "debug" / "deps" / "big").exists())
        lock.unlink()
        rc, out = self.cap("--apply")  # makes and takes its own lock
        self.assertEqual(rc, 0)
        self.assertTrue(lock.exists())
        self.assertFalse((dev / "debug" / "deps").exists())

    def test_held_cargo_lock_of_dev_target_skips(self):
        dev = self.target("terrarium-dev-target", 200 * 1024)
        with open(dev / "debug" / ".cargo-lock") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            rc, out = self.cap("--apply")
        self.assertEqual(json.loads(out)["entries"][0]["action"], "keep")
        self.assertTrue((dev / "debug" / "deps" / "big").exists())

    def test_busy_dev_target_is_kept(self):
        dev = self.target("terrarium-dev-target", 200 * 1024)
        self.fake_proc(300, cwd=dev / "debug")
        rc, out = self.cap("--apply")
        entry = json.loads(out)["entries"][0]
        self.assertEqual((entry["action"], entry["reason"]), ("keep", "over the cap, used by a running process"))
        self.assertTrue((dev / "debug" / "deps" / "big").exists())

    def test_new_profile_dir_with_a_cargo_lock_is_left_alone(self):
        dev = self.target("terrarium-dev-target", 200 * 1024)
        real = btc.cargo_locks
        calls = []

        def late(target):
            calls.append(target)
            found = real(target)
            if len(calls) == 2:  # right after the keep set: a cargo starts `release`
                (dev / "release").mkdir()
                (dev / "release" / ".cargo-lock").write_text("")
                (dev / "release" / "out").write_bytes(b"x")
            return found

        with mock.patch.object(btc, "cargo_locks", late):
            rc, out = self.cap("--apply")
        self.assertEqual(rc, 1)
        entry = json.loads(out)["entries"][0]
        self.assertEqual(entry["action"], "error")
        self.assertIn("a new .cargo-lock appeared", entry["errors"][0])
        self.assertTrue((dev / "release" / "out").exists())
        self.assertFalse((dev / "debug" / "deps").exists())

    def test_missing_slot_lock_skips_and_other_dirs_are_ignored(self):
        t = self.target("terrarium-cardcheck-target-4", 200 * 1024)
        self.mk("grok-1192-target", size=200 * 1024)
        rc, out = self.cap("--apply")
        entries = json.loads(out)["entries"]
        self.assertEqual(len(entries), 1)
        self.assertEqual(entries[0]["action"], "keep")
        self.assertTrue((t / "debug" / "deps" / "big").exists())
        self.assertTrue((self.root / "grok-1192-target" / "blob").exists())


class Floor(Tree):
    def test_floor(self):
        with mock.patch.object(btc, "free_pct", return_value=(12.5, 1 * btc.GIB, 8 * btc.GIB)):
            rc, out = run("--root", str(self.root), "floor")
        self.assertEqual(rc, 1)
        self.assertIn("refused", out)
        self.assertIn("20% floor", out)
        self.assertIn("terrarium-dev-target", out)
        with mock.patch.object(btc, "free_pct", return_value=(25.0, 2 * btc.GIB, 8 * btc.GIB)):
            rc, out = run("--root", str(self.root), "floor")
        self.assertEqual(rc, 0)
        self.assertTrue(out.startswith("ok:"))
        with mock.patch.object(btc, "free_pct", return_value=(25.0, 2 * btc.GIB, 8 * btc.GIB)):
            rc, _ = run("--root", str(self.root), "floor", "--min-free-pct", "30")
        self.assertEqual(rc, 1)

    def test_real_statvfs(self):
        pct, free, total = btc.free_pct(str(self.root))
        self.assertGreater(total, 0)
        self.assertTrue(0 <= pct <= 100)


if __name__ == "__main__":
    unittest.main()

"""Retention never operates on production paths in these tests."""
import datetime as dt
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import subprocess
import sys
import time
import unittest
from unittest.mock import patch
import zipfile

SPEC = importlib.util.spec_from_file_location("hot", Path(__file__).resolve().parents[1] / "docker/hot-swap.py")
HOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOT)
NOW = dt.datetime(2026, 11, 12, 20, tzinfo=dt.timezone.utc)


class RetentionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.state = Path(self.tmp.name)
        self.directory = self.state / "backups"
        self.directory.mkdir()
        env = patch.dict(os.environ, {"TERRARIUM_STATE_DIR": str(self.state), "TERRARIUM_BACKUP_DIR": str(self.directory)})
        env.start()
        self.addCleanup(env.stop)
        self.token = "A" * 26

    def retain(self, protected, now=NOW):
        HOT.retention_control("prepare", {"token": self.token, "protected": protected, "expires_at": time.time() + 10})
        return HOT.retain_backups(protected, now, token=self.token)

    def backup(self, age, hour=12, kind="full", base=None):
        created = (NOW - dt.timedelta(days=age)).replace(hour=hour)
        name = f"terrarium-backup-{created:%Y%m%dT%H%M%SZ}-{kind}.zip"
        with zipfile.ZipFile(self.directory / name, "w") as archive:
            archive.writestr("backup.json", json.dumps({"format": "terrarium-backup", "version": 1,
                "kind": kind, "created_at": created.isoformat(), "base": {"name": base}}))
        return name

    def test_newest_five_plus_latest_per_utc_day_for_fourteen_days(self):
        names = [self.backup(age, hour) for age in range(20) for hour in (8, 12, 16)]
        protected = self.backup(30)
        protected_snapshot = self.directory / (protected + ".hot-swap.sqlite")
        protected_snapshot.write_bytes(b"recovery")
        snapshot = self.directory / (names[-1] + ".hot-swap.sqlite")
        snapshot.write_bytes(b"snapshot")
        all_full = sorted(names, reverse=True)
        keep = set(all_full[:5]) | {name for name in names if "T160000Z" in name and name >= "terrarium-backup-20261030"} | {protected}
        # Multiple bounded passes drain the eligible set.
        for _ in range(8):
            result = self.retain([str(self.directory / protected)])
            self.assertLessEqual(len(result["removed"]), 32)
        self.assertEqual({p.name for p in self.directory.glob("*.zip")}, keep)
        self.assertFalse(snapshot.exists())
        self.assertEqual(protected_snapshot.read_bytes(), b"recovery")
        journal = [json.loads(line) for line in (self.directory / ".retention-audit.jsonl").read_text().splitlines()]
        deleted = [r for r in journal if r["action"] == "backup.prune"]
        self.assertEqual(len(deleted), len(names) - len(keep) + 2)
        self.assertTrue(all(isinstance(r["size"], int) for r in deleted))

    def test_diff_bases_unknown_files_and_future_backups_survive(self):
        base = self.backup(40)
        self.backup(1, kind="diff", base=base)
        future = self.backup(-2)
        for age in range(8):
            self.backup(age)
        unknown = self.directory / "manual.zip"
        unknown.write_bytes(b"not ours")
        self.retain([])
        self.assertTrue((self.directory / base).exists())
        self.assertTrue((self.directory / future).exists())
        self.assertEqual(unknown.read_bytes(), b"not ours")

    def test_every_future_backup_later_today_is_kept(self):
        names = [self.backup(0, hour) for hour in range(12, 18)]
        before_noon = NOW.replace(hour=11)
        result = self.retain([], before_noon)
        self.assertEqual(result["removed"], [])
        self.assertTrue(all((self.directory / name).exists() for name in names))

    def test_symlink_never_deletes_outside_directory(self):
        outside = self.state / "outside"
        outside.write_bytes(b"keep")
        (self.directory / "terrarium-backup-20200101T000000Z-full.zip").symlink_to(outside)
        with self.assertRaises(OSError):
            self.retain([])
        self.assertEqual(outside.read_bytes(), b"keep")

    def test_creator_flock_serializes_retention(self):
        for age in range(20):
            self.backup(age)
        started = threading.Event()
        finished = threading.Event()
        errors = []
        HOT.retention_control("prepare", {"token": self.token, "protected": [], "expires_at": time.time() + 10})
        def retain():
            started.set()
            try:
                HOT.retain_backups([], NOW, token=self.token)
            except Exception as e:
                errors.append(e)
            finally:
                finished.set()
        with (self.directory / ".retention.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            worker = threading.Thread(target=retain)
            worker.start()
            self.assertTrue(started.wait(2))
            self.assertFalse(finished.wait(.1))
            self.assertEqual(len(list(self.directory.glob("*.zip"))), 20)
        worker.join(5)
        self.assertTrue(finished.is_set())
        self.assertEqual(errors, [])

    def test_preview_has_exact_names_dates_sizes_and_total_without_deletion(self):
        names = [self.backup(age) for age in range(20)]
        result = HOT.retain_backups([], NOW, preview=True)
        self.assertTrue(result["preview"])
        self.assertEqual(result["total_files"], len(result["preview"]))
        self.assertEqual(result["total_bytes"], sum((self.directory / r["name"]).stat().st_size for r in result["preview"]))
        self.assertTrue(all("created_at" in r for r in result["preview"]))
        self.assertTrue(all((self.directory / name).exists() for name in names))

    def test_revocation_prevents_delayed_start_and_prepare(self):
        names = [self.backup(age) for age in range(20)]
        HOT.retention_control("prepare", {"token":self.token, "protected":[], "expires_at":time.time() + 10})
        HOT.retention_control("cancel", {"token":self.token})
        with self.assertRaisesRegex(RuntimeError, "revoked"):
            HOT.retention_control("prepare", {"token":self.token, "protected":[], "expires_at":time.time() + 10})
        result = HOT.retain_backups([], NOW, token=self.token)
        self.assertTrue(result["stopped"])
        self.assertEqual(result["removed"], [])
        self.assertTrue(all((self.directory / name).exists() for name in names))

    def test_authorization_and_protections_are_reread_before_each_unlink(self):
        names = [self.backup(age) for age in range(20)]
        HOT.retention_control("prepare", {"token":self.token, "protected":[], "expires_at":time.time() + 10})
        original = HOT.retention_authorized
        calls = []
        def protect(directory, token):
            calls.append(token)
            if len(calls) == 2:
                HOT.retention_json(directory, ".retention-control.json", {"token":token, "protected":names, "expires_at":time.time() + 10})
            return original(directory, token)
        with patch.object(HOT, "retention_authorized", side_effect=protect):
            result = HOT.retain_backups([], NOW, token=self.token)
        self.assertEqual(result["removed"], [])
        self.assertGreater(len(calls), 2)
        def revoke(directory, token):
            (directory / (".retention-" + token + ".revoked")).touch()
            return original(directory, token)
        with patch.object(HOT, "retention_authorized", side_effect=revoke):
            self.assertTrue(HOT.retain_backups([], NOW, token=self.token)["stopped"])

    def test_first_pass_never_adds_a_name_after_concurrent_create(self):
        # Older same-day backups avoid the daily keeper masking the fifth.
        names = [self.backup(20, hour) for hour in range(8, 14)]
        preview = HOT.retain_backups([], NOW, preview=True)
        confirmed = {item["name"] for item in preview["preview"]}
        fifth = sorted(names, reverse=True)[4]
        self.assertNotIn(fifth, confirmed)
        self.backup(0)
        HOT.retention_control("prepare", {"token":self.token, "protected":[],
            "expires_at":time.time() + 10, "first_pass":sorted(confirmed)})
        result = HOT.retain_backups([], NOW, token=self.token)
        removed = {item["name"] for item in result["removed"]}
        self.assertEqual(removed, confirmed)
        self.assertTrue((self.directory / fifth).exists())
        self.assertIn(fifth, {item["name"] for item in HOT.retain_backups([], NOW, preview=True)["preview"]})

    def test_expired_lease_stops_a_paused_helper_without_unlink(self):
        names = [self.backup(age) for age in range(20)]
        HOT.retention_control("prepare", {"token":self.token, "protected":[], "expires_at":time.time() + 10})
        original = HOT.retention_authorized
        checked = []
        def expire(directory, token):
            checked.append(token)
            if len(checked) == 1:
                return original(directory, token)
            with patch.object(HOT.time, "time", return_value=time.time() + 11):
                return original(directory, token)
        with patch.object(HOT, "retention_authorized", side_effect=expire):
            result = HOT.retain_backups([], NOW, token=self.token)
        self.assertTrue(result["stopped"])
        self.assertEqual(result["removed"], [])
        self.assertTrue(all((self.directory / name).exists() for name in names))

    def test_repair_inventory_holds_every_leftover_snapshot_with_release_keys(self):
        name = self.backup(40)
        (self.directory / (name + ".hot-swap.sqlite")).write_bytes(b"recovery")
        self.assertEqual(HOT.retention_inventory(), {"snapshot:" + name:str(self.directory / name)})

    def test_cancel_terminates_exact_worker_waiting_on_backup_flock(self):
        for age in range(20):
            self.backup(age)
        HOT.retention_control("prepare", {"token":self.token, "protected":[], "expires_at":time.time() + 10})
        with (self.directory / ".retention.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            child = subprocess.Popen([sys.executable, str(SPEC.origin), "retain-backups", self.token], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                identity_path = self.directory / (".retention-" + self.token + ".worker.json")
                deadline = time.monotonic() + 5
                while not identity_path.exists() and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertTrue(identity_path.exists())
                identity = HOT.retention_control("cancel", {"token":self.token})
                self.assertEqual(identity["pid"], child.pid)
                child.communicate(timeout=5)
                self.assertEqual(child.returncode, -9)
            finally:
                if child.poll() is None:
                    child.kill()
                child.communicate()
        self.assertEqual(len(list(self.directory.glob("*.zip"))), 20)


if __name__ == "__main__":
    unittest.main()

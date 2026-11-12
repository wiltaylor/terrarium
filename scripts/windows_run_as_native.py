"""TER-272 acceptance, only inside an explicitly approved isolated lab window.

Run with pytest and --junitxml. Missing fixture/evidence is failure, never skip.
This runner creates only per-run SMB/copy files. It changes no accounts, grants,
services, host policy, secrets, domain policy, UAC or firewall configuration.
Meta provisions and recovers the exact approved fixture manifest separately.
"""
from __future__ import annotations

import concurrent.futures
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


def literal(value: str) -> str:
    if "\0" in value or "\n" in value or "\r" in value:
        raise ValueError("control character in fixture literal")
    return "'" + value.replace("'", "''") + "'"


def validate_fixture(fixture: dict) -> dict:
    required = ("approved_run_id", "daemon_url", "production_daemon_url", "commit",
                "namespace", "probe", "share_directory", "local_directory",
                "admin_sentinel", "hkcu_path", "hkcu_value", "dc_receipt",
                "a", "b", "c_filtered", "c_full", "wrong_password_target",
                "disabled_account_target", "wrong_password_attempts_approved")
    if any(key not in fixture for key in required):
        raise ValueError("incomplete native fixture inventory")
    if not re.fullmatch(r"[0-9a-f]{40}", fixture["commit"]):
        raise ValueError("full pushed commit required")
    if not re.fullmatch(r"ter272_[0-9a-f]{8}_[0-9]{12}", fixture["namespace"]):
        raise ValueError("dedicated fixture namespace required")
    if fixture["namespace"][7:15] != fixture["commit"][:8]:
        raise ValueError("fixture namespace does not match commit")
    if not fixture["approved_run_id"] or fixture["daemon_url"] == fixture["production_daemon_url"]:
        raise ValueError("isolated approved endpoint required")
    if not fixture["daemon_url"].startswith("https://"):
        raise ValueError("isolated TLS endpoint required")
    share = fixture["share_directory"]
    if share != "\\\\dc01.lab.test\\share\\" + fixture["namespace"]:
        raise ValueError("dedicated inventoried DC share subdirectory required")
    if fixture["namespace"] not in fixture["local_directory"]:
        raise ValueError("dedicated local fixture directory required")
    if fixture["wrong_password_attempts_approved"] != 1:
        raise ValueError("single wrong-password attempt needs lockout-policy approval")
    for name in ("a", "b", "c_filtered", "c_full"):
        entry = fixture[name]
        if any(key not in entry for key in ("target", "account", "sid", "username", "domain", "scratch")):
            raise ValueError("incomplete account inventory")
        if not re.fullmatch(r"S-1-5-(?:[0-9]+-)*[0-9]+", entry["sid"]):
            raise ValueError("account SID required")
        if len(entry["username"]) > 20 or "agenttest" in entry["username"].lower():
            raise ValueError("dedicated Windows-compatible account names required")
    if fixture["a"]["sid"] == fixture["b"]["sid"]:
        raise ValueError("ordinary accounts must be distinct")
    if fixture["c_filtered"]["sid"] != fixture["c_full"]["sid"]:
        raise ValueError("filtered and full fixture must use the same account SID")
    return fixture


class WindowsRunAsNative(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path = os.environ.get("TERRARIUM_NATIVE_RUN_AS_FIXTURE")
        if not path:
            raise RuntimeError("approved native fixture file required")
        cls.fixture = validate_fixture(json.loads(Path(path).read_text()))
        if os.environ.get("TERRARIUM_URL") != cls.fixture["daemon_url"]:
            raise RuntimeError("CLI endpoint must exactly match approved isolated daemon")
        if os.environ.get("TERRARIUM_NATIVE_APPROVED_RUN") != cls.fixture["approved_run_id"]:
            raise RuntimeError("native window execution is not approved")

    def cli(self, *arguments, success=True):
        # No passwords in argv, output, assertions or result artifacts. Targets
        # resolve their narrowly granted secrets in the isolated daemon only.
        result = subprocess.run(["terrarium", *arguments], capture_output=True,
                                text=True, timeout=90, check=False)
        if success:
            self.assertEqual(result.returncode, 0, "native CLI command failed (inspect redacted run log)")
        else:
            self.assertNotEqual(result.returncode, 0, "negative case unexpectedly succeeded")
        return result.stdout

    def ps(self, account, script, success=True):
        return self.cli("exec", self.fixture[account]["target"], "--", "powershell.exe",
                        "-NoProfile", "-NonInteractive", "-Command",
                        "$ErrorActionPreference='Stop'; " + script, success=success)

    def snapshot(self, account, pty=False):
        arguments = ["exec"] + (["--tty"] if pty else [])
        output = self.cli(*arguments, self.fixture[account]["target"], "--",
                          self.fixture["probe"], "identity-probe")
        return json.loads(output.strip())

    def check_ordinary(self, account, snapshot):
        expected = self.fixture[account]
        self.assertEqual(snapshot["user_sid"], expected["sid"])
        self.assertFalse(snapshot["elevated"])
        self.assertFalse(snapshot["administrators_enabled"])
        environment = snapshot["environment"]
        self.assertEqual(environment["USERNAME"].lower(), expected["username"].lower())
        self.assertEqual(environment["USERDOMAIN"].lower(), expected["domain"].lower())
        self.assertNotIn("systemprofile", environment["USERPROFILE"].lower())
        self.assertTrue(environment["TEMP"])

    def test_ordinary_token_profile_hkcu_and_environment(self):
        for name in ("a", "b"):
            self.check_ordinary(name, self.snapshot(name))
        owner = self.ps("a", "$o=(Get-Acl -LiteralPath $env:USERPROFILE).Owner; "
                        "([Security.Principal.NTAccount]$o).Translate([Security.Principal.SecurityIdentifier]).Value")
        self.assertEqual(owner.strip(), self.fixture["a"]["sid"])
        hkcu = self.ps("a", "Get-ItemPropertyValue -LiteralPath " + literal(self.fixture["hkcu_path"])
                       + " -Name 'ter272'")
        self.assertEqual(hkcu.strip(), self.fixture["hkcu_value"])

    def test_conpty_retains_user_token(self):
        self.check_ordinary("a", self.snapshot("a", pty=True))

    def test_parallel_identities_do_not_cross(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            a = pool.submit(self.snapshot, "a")
            b = pool.submit(self.snapshot, "b")
            self.check_ordinary("a", a.result())
            self.check_ordinary("b", b.result())

    def test_host_policy_filters_and_elevates_same_admin_sid(self):
        filtered, full = self.snapshot("c_filtered"), self.snapshot("c_full")
        self.assertEqual(filtered["user_sid"], full["user_sid"])
        self.assertEqual(filtered["user_sid"], self.fixture["c_full"]["sid"])
        self.assertEqual(filtered["elevation_type"], 3)  # TokenElevationTypeLimited
        self.assertFalse(filtered["elevated"])
        self.assertFalse(filtered["administrators_enabled"])
        self.assertTrue(filtered["administrators_deny_only"])
        self.assertTrue(full["elevated"])
        self.assertTrue(full["administrators_enabled"])
        sentinel = literal(self.fixture["admin_sentinel"])
        self.ps("c_filtered", "[IO.File]::ReadAllBytes(" + sentinel + ") | Out-Null", success=False)
        self.ps("c_full", "[IO.File]::ReadAllBytes(" + sentinel + ") | Out-Null")

    def test_copy_and_smb_authority_in_both_directions(self):
        for directory, suffix in ((self.fixture["local_directory"], "local"),
                                  (self.fixture["share_directory"], "smb")):
            remote = directory + "\\copy-" + suffix + ".bin"
            with tempfile.TemporaryDirectory(prefix="ter272-") as temporary:
                source, destination = Path(temporary) / "input.bin", Path(temporary) / "output.bin"
                content = os.urandom(2 * 1024 * 1024 + 31)  # non-secret fixture bytes
                source.write_bytes(content)
                target = self.fixture["a"]["target"]
                self.cli("cp", str(source), target + ":" + remote)
                self.cli("cp", target + ":" + remote, str(destination))
                self.assertEqual(hashlib.sha256(destination.read_bytes()).digest(), hashlib.sha256(content).digest())
                owner = self.ps("a", "$o=(Get-Acl -LiteralPath " + literal(remote) + ").Owner; "
                                "([Security.Principal.NTAccount]$o).Translate([Security.Principal.SecurityIdentifier]).Value")
                self.assertEqual(owner.strip(), self.fixture["a"]["sid"])
                self.cli("cp", self.fixture["b"]["target"] + ":" + remote,
                         str(Path(temporary) / "denied.bin"), success=False)
                self.cli("cp", str(source), self.fixture["b"]["target"] + ":" + remote, success=False)
                self.cli("cp", target + ":" + remote, str(destination))
                self.assertEqual(destination.read_bytes(), content)
                if suffix == "smb":
                    sha = self.ps("a", "(Get-FileHash -Algorithm SHA256 -LiteralPath " + literal(remote) + ").Hash")
                    self.assertEqual(sha.strip().lower(), hashlib.sha256(content).hexdigest())
                    executed = directory + "\\exec-smb.bin"
                    self.ps("a", "[IO.File]::WriteAllBytes(" + literal(executed) + ", [byte[]](1,7,9,2))")
                    self.cli("cp", target + ":" + executed, str(destination))
                    self.assertEqual(destination.read_bytes(), bytes([1, 7, 9, 2]))
                    # Meta's independent DC observer starts before the suite.
                    # Keep an A-authenticated SMB open long enough to record it.
                    self.ps("a", "$f=[IO.File]::OpenRead(" + literal(remote)
                            + "); try { Start-Sleep -Seconds 15 } finally { $f.Dispose() }")

    def test_timeout_kills_immediate_detached_grandchild_for_each_token(self):
        for account in ("a", "b", "c_filtered", "c_full"):
            marker = self.fixture[account]["scratch"] + "\\timeout-grandchild.txt"
            quoted = literal(marker)
            # Prove this account can write here before interpreting absence as
            # containment. Never stop any process by executable name.
            self.ps(account, "if (Test-Path -LiteralPath " + quoted + ") { throw 'dirty fixture' }; "
                    "[IO.File]::WriteAllText(" + quoted + ", 'writable'); Remove-Item -LiteralPath " + quoted)
            child = "Start-Sleep -Seconds 5; [IO.File]::WriteAllText(" + quoted + ", 'survived')"
            encoded = base64.b64encode(child.encode("utf-16le")).decode("ascii")
            parent = "Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -EncodedCommand " + encoded + "'; Start-Sleep -Seconds 30"
            self.cli("exec", "--timeout", "1", self.fixture[account]["target"], "--",
                     "powershell.exe", "-NoProfile", "-NonInteractive", "-Command", parent, success=False)
            absent = self.ps(account, "Start-Sleep -Seconds 7; Test-Path -LiteralPath " + quoted)
            self.assertEqual(absent.strip(), "False", "grandchild survived session timeout")

    def test_wrong_password_once_and_disabled_account_fail_closed(self):
        for key in ("wrong_password_target", "disabled_account_target"):
            self.cli("exec", self.fixture[key], "--", self.fixture["probe"], "identity-probe", success=False)

    def test_dc_observer_receipt_is_present_and_matches_identity(self):
        # Meta supplies this read-only DC-side observation during the window.
        # It must be independent of the client and name this run's files/SID.
        receipt = json.loads(Path(self.fixture["dc_receipt"]).read_text())
        self.assertEqual(receipt["namespace"], self.fixture["namespace"])
        self.assertEqual(receipt["account_sid"], self.fixture["a"]["sid"])
        self.assertTrue(receipt["smb_session_observed"])
        self.assertTrue(receipt["smb_open_file_observed"])
        self.assertEqual(receipt["file_owner_sid"], self.fixture["a"]["sid"])


if __name__ == "__main__":
    unittest.main()

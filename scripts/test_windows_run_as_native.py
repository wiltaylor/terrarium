"""Pure fixture validation tests. Never starts Windows or Terrarium commands."""
import unittest
import copy
from windows_run_as_native import account_name, literal, validate_fixture


class NativeFixtureValidation(unittest.TestCase):
    def fixture(self):
        commit, run = "f" * 40, "20261113T023000Z"
        namespace = "ter272_ffffffff_" + run
        fixture = dict(approved_run_id=run, daemon_url="http://127.0.0.1:18480",
                       production_daemon_url="https://production.invalid", commit=commit,
                       namespace=namespace, probe="C:\\fixture\\probe.exe",
                       share_directory="\\\\dc01.lab.test\\share\\" + namespace,
                       local_directory="C:\\fixture\\" + namespace + "\\a",
                       admin_sentinel="C:\\fixture\\admin.bin", host_config_path="C:\\fixture\\host.toml",
                       hkcu_path="HKCU:\\Software\\Terrarium\\" + namespace, hkcu_value="fixture",
                       dc_receipt="receipt.json", wrong_password_target="fixture-wrong",
                       wrong_password_attempts_approved=1)
        for number, key in enumerate(("a", "b", "c_filtered", "c_full", "d_disabled")):
            role = key[0]
            username = account_name(commit, run, role)
            domain = "WINHOST" if role in "cd" else "LAB"
            qualifier = "." if key == "c_filtered" else domain
            fixture[key] = dict(target="fixture-" + key, account=qualifier + "\\" + username,
                                username=username, domain=domain, sid="S-1-5-21-1-2-3-" + str(number + 1000),
                                scratch="C:\\fixture\\" + namespace + "\\" + role)
        fixture["c_full"]["sid"] = fixture["c_filtered"]["sid"]
        return fixture

    def test_complete_synthetic_inventory_is_accepted(self):
        fixture = self.fixture()
        self.assertIs(validate_fixture(fixture), fixture)

    def test_unsafe_inventory_variants_fail_closed(self):
        for field, value in (("daemon_url", "http://0.0.0.0:18480"),
                             ("daemon_url", "http://127.0.0.1:8080"),
                             ("daemon_url", "http://user:password@127.0.0.1:18480"),
                             ("namespace", "baseline"), ("approved_run_id", "later"),
                             ("commit", "fffffff"), ("wrong_password_attempts_approved", 2),
                             ("share_directory", "\\\\dc01.lab.test\\share")):
            fixture = copy.deepcopy(self.fixture())
            fixture[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                validate_fixture(fixture)

    def test_account_names_fit_sam_and_bind_role_commit_and_run(self):
        for role in "abcd":
            name = account_name("f" * 40, "20261113T023000Z", role)
            self.assertEqual(len(name), 20)
            self.assertTrue(name.endswith(role))
            self.assertTrue(name.startswith("t272_fffffff_"))
        self.assertNotEqual(account_name("f" * 40, "20261113T023000Z", "a"),
                            account_name("f" * 40, "20261113T023001Z", "a"))
        with self.assertRaises(ValueError):
            account_name("f" * 40, "20261113T023000Z", "admin")

    def test_literals_escape_quotes_and_reject_control_characters(self):
        self.assertEqual(literal("C:\\a'b"), "'C:\\a''b'")
        for value in ("a\0b", "a\nb", "a\rb"):
            with self.assertRaises(ValueError):
                literal(value)

    def test_missing_inventory_fails_closed(self):
        for fixture in ({}, {"namespace": "ter272"}, {"daemon_url": "https://example.invalid"}):
            with self.assertRaisesRegex(ValueError, "incomplete"):
                validate_fixture(fixture)

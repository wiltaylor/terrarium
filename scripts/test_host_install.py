"""Tests for scripts/host-install.py: the inputs, the generated files, and
the ssh and scp commands. Nothing here dials a host.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import shlex
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("host_install", HERE / "host-install.py")
assert _spec and _spec.loader
hi = importlib.util.module_from_spec(_spec)
sys.modules["host_install"] = hi  # dataclasses look the module up here
_spec.loader.exec_module(hi)


def settings(**kw) -> "hi.Settings":
    base = dict(
        target="wil@lab1.lan",
        name="lab1",
        daemon_url="wss://terrarium.lan:8082",
        fingerprint="AA:BB",
        service_user="terrarium",
        binary=Path("/repo/target/host/terrarium-host"),
        unit_template=hi.REPO / "crates/terrarium-host/terrarium-host.service",
        terrarium=["terrarium"],
    )
    base.update(kw)
    return hi.Settings(**base)


def remote(argv: list[str]) -> list[str]:
    """The command that ssh runs on the host, split again."""
    assert argv[0] == "ssh"
    return shlex.split(argv[-1])


class Inputs(unittest.TestCase):
    def test_default_name(self):
        self.assertEqual(hi.default_name("wil@Lab1.lan"), "lab1")
        self.assertEqual(hi.default_name("example.local"), "example")
        self.assertEqual(hi.default_name("10.0.0.7"), "10")

    def test_check_name(self):
        self.assertEqual(hi.check_name("lab-1"), "lab-1")
        for bad in ("", "Lab", "a.b", "-x", "x" * 64):
            with self.assertRaises(hi.InstallError):
                hi.check_name(bad)

    def test_daemon_url(self):
        r = hi.resolve_daemon_url
        self.assertEqual(r("terrarium.lan", {}), "wss://terrarium.lan:8082")
        self.assertEqual(r("wss://t.lan:9000", {}), "wss://t.lan:9000")
        self.assertEqual(r(None, {"TERRARIUM_HOST_DAEMON_URL": "t.lan"}), "wss://t.lan:8082")
        # The parameter wins over the env variable.
        self.assertEqual(r("a.lan", {"TERRARIUM_HOST_DAEMON_URL": "b.lan"}), "wss://a.lan:8082")
        self.assertEqual(
            r(None, {"TERRARIUM_PUBLIC_URL": "https://terrarium.example.com"}),
            "wss://terrarium.example.com:8082",
        )
        # localhost is no use to a remote machine.
        with self.assertRaises(hi.InstallError):
            r(None, {"TERRARIUM_PUBLIC_URL": "http://localhost:8080"})
        with self.assertRaises(hi.InstallError):
            r(None, {})
        with self.assertRaises(hi.InstallError):
            r("ftp://x", {})

    def test_fingerprint_order(self):
        with tempfile.TemporaryDirectory() as d:
            cert = Path(d) / "host-cert.pem"
            url = "wss://t:8082"
            # No cert, no file: no pin.
            self.assertIsNone(hi.resolve_fingerprint(None, cert, False, url, False))
            # The side file, in the form `host::cert` prints.
            (Path(d) / "host-cert.fingerprint").write_text('server_fingerprint_sha256 = "AB:CD"\n')
            self.assertEqual(hi.resolve_fingerprint(None, cert, False, url, False), "AB:CD")
            # The flag wins; --no-pin and ws:// give none.
            self.assertEqual(hi.resolve_fingerprint("EF", cert, False, url, False), "EF")
            self.assertIsNone(hi.resolve_fingerprint("EF", cert, True, url, False))
            self.assertIsNone(hi.resolve_fingerprint("EF", cert, False, "ws://t:8082", False))


class Files(unittest.TestCase):
    def test_config(self):
        text = hi.render_config(settings())
        self.assertIn('daemon_url = "wss://terrarium.lan:8082"\n', text)
        self.assertIn('token_file = "/etc/terrarium-host/token"\n', text)
        self.assertIn('name = "lab1"\n', text)
        self.assertIn('server_fingerprint_sha256 = "AA:BB"\n', text)
        self.assertNotIn("server_fingerprint", hi.render_config(settings(fingerprint=None)))

    def test_unit_sets_the_user(self):
        unit = hi.render_unit((hi.REPO / "crates/terrarium-host/terrarium-host.service").read_text(), "wil")
        self.assertIn("\nUser=wil\n", unit)
        self.assertIn("\nGroup=wil\n", unit)
        self.assertIn("ExecStart=/usr/local/bin/terrarium-host --config /etc/terrarium-host/config.toml run", unit)


class Commands(unittest.TestCase):
    def test_probe_uses_sudo_and_reads_state(self):
        argv = hi.probe_cmd(settings())
        self.assertEqual(argv[:2], ["ssh", "wil@lab1.lan"])
        cmd = remote(argv)
        self.assertEqual(cmd[:4], ["sudo", "-n", "sh", "-c"])
        self.assertIn("test -e /etc/terrarium-host/token && echo token", cmd[4])
        st = hi.parse_probe("config\nuser\n")
        self.assertEqual((st.config, st.token, st.unit, st.user), (True, False, False, True))

    def test_root_needs_no_sudo(self):
        s = settings(target="root@lab1")
        self.assertEqual(remote(hi.probe_cmd(s))[0], "sh")
        self.assertEqual(remote(hi.write_token_cmd(s))[:4], ["runuser", "-u", "terrarium", "--"])

    def test_upload(self):
        s = settings(ssh_opts=["-p", "2222", "-i", "key"])
        cmds = hi.upload_cmds(s, Path("/tmp/u"), Path("/tmp/c"))
        self.assertEqual(
            cmds,
            [
                ["scp", "-P", "2222", "-i", "key", "-q", "/repo/target/host/terrarium-host", "wil@lab1.lan:.terrarium-host.bin.new"],
                ["scp", "-P", "2222", "-i", "key", "-q", "/tmp/u", "wil@lab1.lan:.terrarium-host.service.new"],
                ["scp", "-P", "2222", "-i", "key", "-q", "/tmp/c", "wil@lab1.lan:.terrarium-host.config.toml.new"],
            ],
        )
        # An existing config is not uploaded.
        self.assertEqual(len(hi.upload_cmds(s, Path("/tmp/u"), None)), 2)
        # ssh keeps -p.
        self.assertEqual(hi.probe_cmd(s)[:5], ["ssh", "-p", "2222", "-i", "key"])

    def test_setup_new_host(self):
        script = remote(hi.setup_cmd(settings(), hi.RemoteState(), True))[-1]
        self.assertIn("useradd --system --user-group", script)
        self.assertIn("install -m 0755 -o root -g root .terrarium-host.bin.new /usr/local/bin/terrarium-host", script)
        self.assertIn("install -m 0644 -o root -g root .terrarium-host.service.new /etc/systemd/system/terrarium-host.service", script)
        self.assertIn("install -d -m 0700 -o terrarium -g terrarium /etc/terrarium-host", script)
        self.assertIn(".terrarium-host.config.toml.new /etc/terrarium-host/config.toml", script)
        self.assertTrue(script.endswith("systemctl daemon-reload"))

    def test_setup_rerun_keeps_user_and_config(self):
        state = hi.RemoteState(config=True, token=True, unit=True, user=True)
        script = remote(hi.setup_cmd(settings(), state, False))[-1]
        self.assertNotIn("useradd", script)
        self.assertNotIn("/etc/terrarium-host/config.toml", script)
        # The binary and the unit are always replaced.
        self.assertIn("/usr/local/bin/terrarium-host", script)

    def test_token_commands(self):
        s = settings()
        self.assertEqual(hi.token_cmd(s, False), ["terrarium", "--json", "admin", "host", "new", "lab1", "--enrol"])
        self.assertEqual(hi.token_cmd(s, True), ["terrarium", "--json", "admin", "host", "rotate", "lab1", "--enrol"])
        self.assertEqual(
            remote(hi.write_token_cmd(s)),
            ["sudo", "-n", "-u", "terrarium", "/usr/local/bin/terrarium-host", "token-file", "/etc/terrarium-host/token"],
        )

    def test_start_and_check(self):
        s = settings()
        self.assertEqual(remote(hi.start_cmd(s))[-1], "systemctl enable terrarium-host && systemctl restart terrarium-host")
        self.assertEqual(
            remote(hi.check_cmd(s))[-5:],
            ["/usr/local/bin/terrarium-host", "--config", "/etc/terrarium-host/config.toml", "check", "--probe"],
        )


class DryRun(unittest.TestCase):
    def run_main(self, *argv: str) -> tuple[int, str]:
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
            code = hi.main(list(argv))
        return code, buf.getvalue()

    def test_dry_run_prints_the_plan(self):
        code, out = self.run_main(
            "example.local", "demo", "--dry-run", "--daemon-url", "terrarium.lan", "--fingerprint", "AA:BB"
        )
        self.assertEqual(code, 0, out)
        lines = [l for l in out.splitlines() if l.startswith("+ ")]
        self.assertTrue(lines[0].startswith("+ ssh example.local "), lines[0])
        self.assertEqual(sum(l.startswith("+ scp ") for l in lines), 3)
        self.assertIn("+ terrarium --json admin host ls", "\n".join(lines).replace("cargo run -q -p terrarium -- ", "terrarium "))
        self.assertTrue(any("admin host new demo --enrol" in l for l in lines))
        self.assertTrue(any(l.endswith("< <enrol token>") for l in lines))
        self.assertTrue(lines[-1].endswith("check --probe'"), lines[-1])
        self.assertIn('#   daemon_url = "wss://terrarium.lan:8082"', out)
        self.assertIn('#   server_fingerprint_sha256 = "AA:BB"', out)
        self.assertNotIn("the_<enrol token>", "\n".join(lines))

    def test_bad_input_exits_2(self):
        code, out = self.run_main("example.local", "Bad.Name", "--dry-run", "--daemon-url", "t")
        self.assertEqual(code, 2)
        self.assertIn("bad host name", out)


if __name__ == "__main__":
    unittest.main()

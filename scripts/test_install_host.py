"""Tests for scripts/install-host.sh, the installer that the daemon serves at
GET /install/host.sh. Nothing here touches the real system.

Each test fills in the placeholders as the daemon does, then runs the
script with TERRARIUM_INSTALL_ROOT set to a temp dir and fake programs
first on PATH: curl (serves a fake dist dir), id, getent, hostname,
useradd, usermod, chown, runuser, systemctl, and uname. The downloaded
"binary" is a shell script that records its arguments. The config and the
unit must be the ones that scripts/host-install.py makes.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import hashlib
import importlib.util
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
SCRIPT = HERE / "install-host.sh"
UNIT = REPO / "crates/terrarium-host/terrarium-host.service"

_spec = importlib.util.spec_from_file_location("host_install", HERE / "host-install.py")
assert _spec and _spec.loader
hi = importlib.util.module_from_spec(_spec)
sys.modules["host_install"] = hi
_spec.loader.exec_module(hi)

TOKEN = "the_" + "ab" * 32
PIN = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99"


def render(url="http://terrarium.lan:8080", host_url="wss://terrarium.lan:8082", pin=PIN, version="0.1.0") -> str:
    """The script as the daemon serves it (api/install.rs, `render_script`)."""
    text = SCRIPT.read_text()
    for key, value in (
        ("@TERRARIUM_URL@", url),
        ("@TERRARIUM_HOST_URL@", host_url),
        ("@TERRARIUM_HOST_PIN@", pin),
        ("@TERRARIUM_VERSION@", version),
        ("@TERRARIUM_HOST_UNIT@", UNIT.read_text().rstrip("\n")),
    ):
        text = text.replace(key, value)
    return text


def write_exe(path: Path, body: str) -> None:
    path.write_text("#!/bin/sh\n" + body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


# The fakes. Each one logs `name args...` to $FAKE_LOG.
FAKES = {
    "curl": r"""
echo "curl $*" >> "$FAKE_LOG"
out=; url=
while [ "$#" -gt 0 ]; do
  case $1 in -o) out=$2; shift 2 ;; -w) shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac
done
arch=${url##*arch=}
case $url in
  */install/terrarium-host.sha256*) src=$FAKE_DIST/terrarium-host-$arch-linux.sha256 ;;
  */install/terrarium-host*) src=$FAKE_DIST/terrarium-host-$arch-linux ;;
  *) src=/nonexistent ;;
esac
if [ -f "$src" ]; then cp "$src" "$out"; printf 200; else
  printf 'no terrarium-host for arch `%s`: this daemon serves x86_64\n' "$arch" > "$out"; printf 404; fi
""",
    "id": r"""
echo "id $*" >> "$FAKE_LOG"
case "$1" in
  -u) if [ -z "$2" ]; then echo "${FAKE_UID:-0}"; exit 0; fi
      grep -qx "$2" "$FAKE_USERS" && echo 1000 && exit 0; exit 1 ;;
  -gn) grep -qx "$2" "$FAKE_USERS" || exit 1; echo "$2" ;;
  -nG) grep -qx "$2" "$FAKE_USERS" || exit 1; printf '%s' "$2"; cat "$FAKE_GROUPS_OF" 2>/dev/null | tr '\n' ' ' | sed 's/^/ /'; echo ;;
esac
""",
    "getent": r"""
echo "getent $*" >> "$FAKE_LOG"
case "$1" in
  group) grep -qx "$2" "$FAKE_SYSGROUPS" ;;
  passwd) grep -qx "$2" "$FAKE_USERS" || exit 2; echo "$2:x:1000:1000::$FAKE_HOME:/bin/sh" ;;
esac
""",
    "hostname": 'echo "Lab1.example.lan"\n',
    "uname": r"""
case "$1" in -m) echo "${FAKE_ARCH:-x86_64}" ;; -s) echo Linux ;; *) echo Linux ;; esac
""",
    "useradd": 'echo "useradd $*" >> "$FAKE_LOG"\n',
    "usermod": 'echo "usermod $*" >> "$FAKE_LOG"\n',
    "chown": 'echo "chown $*" >> "$FAKE_LOG"\n',
    "systemctl": 'echo "systemctl $*" >> "$FAKE_LOG"\n',
    "runuser": r"""
echo "runuser $1 $2" >> "$FAKE_LOG"
shift 3
exec "$@"
""",
}

# The fake host agent: `token-file PATH` stores stdin; `check --probe` passes
# unless FAKE_PROBE_FAIL is set.
FAKE_HOST = r"""#!/bin/sh
echo "terrarium-host $*" >> "$FAKE_LOG"
case "$1" in
  token-file) cat > "$2"; chmod 600 "$2" ;;
  --config) [ -z "$FAKE_PROBE_FAIL" ] && echo "probe:      ok" || { echo "probe: failed"; exit 1; } ;;
esac
"""


class Harness:
    def __init__(self, tmp: Path):
        self.tmp = tmp
        self.root = tmp / "root"
        self.bin = tmp / "fakebin"
        self.dist = tmp / "dist"
        self.home = tmp / "home/wil"
        self.log = tmp / "calls.log"
        for d in (self.root, self.bin, self.dist, self.home, self.root / "run/systemd/system"):
            d.mkdir(parents=True, exist_ok=True)
        for name, body in FAKES.items():
            write_exe(self.bin / name, body)
        self.users = tmp / "users"
        self.users.write_text("root\nwil\n")
        self.sysgroups = tmp / "sysgroups"
        self.sysgroups.write_text("docker\nkvm\n")
        self.groups_of = tmp / "groups_of"
        self.groups_of.write_text("")
        self.log.write_text("")
        self.publish(FAKE_HOST.encode())

    def publish(self, binary: bytes, sha: str | None = None) -> None:
        (self.dist / "terrarium-host-x86_64-linux").write_bytes(binary)
        digest = sha or hashlib.sha256(binary).hexdigest()
        (self.dist / "terrarium-host-x86_64-linux.sha256").write_text(f"{digest}  terrarium-host-x86_64-linux\n")

    def tool(self, rel: str) -> None:
        """Put an executable at a path under the root (or the home)."""
        p = Path(rel) if rel.startswith("/") else self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        write_exe(p, "exit 0\n")

    def run(self, *args: str, script: str | None = None, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
        path = self.tmp / "install-host.sh"
        path.write_text(script if script is not None else render())
        e = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "TERRARIUM_INSTALL_ROOT": str(self.root),
            "SUDO_USER": "wil",
            "FAKE_LOG": str(self.log),
            "FAKE_DIST": str(self.dist),
            "FAKE_USERS": str(self.users),
            "FAKE_SYSGROUPS": str(self.sysgroups),
            "FAKE_GROUPS_OF": str(self.groups_of),
            "FAKE_HOME": str(self.home),
        }
        e.update(env or {})
        return subprocess.run(["sh", str(path), *args], env=e, capture_output=True, text=True, timeout=60)

    def calls(self) -> list[str]:
        return [l for l in self.log.read_text().splitlines() if l]

    def file(self, rel: str) -> Path:
        return self.root / rel.lstrip("/")


def block(out: str, header: str) -> str:
    """The `#   ` lines after a `# write <path>` line of a dry run."""
    lines = out.splitlines()
    start = next(i for i, l in enumerate(lines) if l.startswith(f"# write {header}"))
    body = []
    for l in lines[start + 1 :]:
        if not l.startswith("#   ") and l != "#   ".rstrip():
            break
        body.append(l[4:])
    return "\n".join(body) + "\n"


class InstallHostScript(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="install-host-test.")
        self.h = Harness(Path(self._tmp.name))

    def tearDown(self):
        self._tmp.cleanup()

    def assertNoToken(self, r: subprocess.CompletedProcess[str]) -> None:
        self.assertNotIn(TOKEN, r.stdout)
        self.assertNotIn(TOKEN, r.stderr)

    def mutations(self) -> list[str]:
        return [c for c in self.h.calls() if c.split()[0] in ("curl", "useradd", "usermod", "chown", "systemctl", "runuser", "terrarium-host")]

    # -- the dry run

    def test_dry_run_prints_the_plan_and_changes_nothing(self):
        self.h.tool("usr/bin/docker")
        r = self.h.run("--token", TOKEN, "--dry-run")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertNoToken(r)
        self.assertIn("# install the host agent `lab1`", r.stdout)
        self.assertIn("# daemon_url = wss://terrarium.lan:8082", r.stdout)
        self.assertIn("docker     = yes, vmlab = no, systemd = yes", r.stdout)
        self.assertIn("+ curl -fsSL http://terrarium.lan:8080/install/terrarium-host?arch=x86_64", r.stdout)
        self.assertIn("+ usermod -aG docker wil", r.stdout)
        self.assertIn("token-file /etc/terrarium-host/token  < <enrol token>", r.stdout)
        self.assertIn("+ systemctl restart terrarium-host", r.stdout)
        # Nothing ran, and no file exists.
        self.assertEqual(self.mutations(), [])
        self.assertEqual([p for p in self.h.root.rglob("*") if p.is_file() and p.name != "docker"], [])

    def test_the_config_and_unit_are_the_ones_of_host_install_py(self):
        r = self.h.run("--token", TOKEN, "--dry-run", "--name", "lab1")
        self.assertEqual(r.returncode, 0, r.stderr)
        s = hi.Settings(
            target="wil@lab1", name="lab1", daemon_url="wss://terrarium.lan:8082",
            fingerprint=PIN, service_user="wil", binary=Path("/x"),
            unit_template=UNIT, terrarium=["terrarium"],
        )
        self.assertEqual(block(r.stdout, "/etc/terrarium-host/config.toml"), hi.render_config(s))
        unit = block(r.stdout, "/etc/systemd/system/terrarium-host.service")
        home = str(self.h.root.parent / "home" / "wil")
        self.assertEqual(unit, hi.render_unit(UNIT.read_text(), "wil", home))
        # `%h` is root's home in a system unit: the PATH gets the user's home.
        self.assertIn(f"Environment=PATH=/usr/local/bin:/usr/bin:/bin:{home}/.local/bin:{home}/.cargo/bin", unit)
        self.assertNotIn("%h/", unit.split("Environment=PATH=")[1].splitlines()[0])

    def test_enable_display_matches_host_install_py(self):
        for flag in (False, True):
            args = ["--token", TOKEN, "--dry-run", "--name", "lab1"] + (["--enable-display"] if flag else [])
            r = self.h.run(*args)
            self.assertEqual(r.returncode, 0, r.stderr)
            s = hi.Settings(
                target="wil@lab1", name="lab1", daemon_url="wss://terrarium.lan:8082",
                fingerprint=PIN, service_user="wil", binary=Path("/x"),
                unit_template=UNIT, terrarium=["terrarium"], enable_display=flag,
            )
            config = block(r.stdout, "/etc/terrarium-host/config.toml")
            self.assertEqual(config, hi.render_config(s))
            self.assertEqual("[display]\nenabled = true\n" in config, flag)
            self.assertEqual("can see this machine's screen" in r.stderr, flag)

    def test_enable_display_on_a_system_user_notes_the_session(self):
        r = self.h.run("--token", TOKEN, "--dry-run", "--enable-display", "--user", "terrarium")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("no desktop session", r.stdout)
        r = self.h.run("--token", TOKEN, "--dry-run", "--enable-display")
        self.assertNotIn("no desktop session", r.stdout)

    def test_enable_display_keeps_an_existing_config(self):
        self.assertEqual(self.h.run("--token", TOKEN).returncode, 0)
        cfg = self.h.file("/etc/terrarium-host/config.toml")
        before = cfg.read_text()
        self.assertNotIn("[display]", before)
        r = self.h.run("--enable-display")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(cfg.read_text(), before)
        self.assertIn("add `[display]` with `enabled = true` to it by hand", r.stdout)

    def test_ws_url_gets_no_pin_and_a_bare_name_gets_wss_and_8082(self):
        r = self.h.run("--token", TOKEN, "--dry-run", "--host-url", "ws://127.0.0.1:8082")
        self.assertIn('daemon_url = "ws://127.0.0.1:8082"', r.stdout)
        self.assertNotIn("server_fingerprint_sha256", r.stdout)
        r = self.h.run("--token", TOKEN, "--dry-run", "--host-url", "t.lan", "--no-pin")
        self.assertIn('daemon_url = "wss://t.lan:8082"', r.stdout)
        self.assertNotIn("server_fingerprint_sha256", r.stdout)
        self.assertIn("pin        = none", r.stdout)

    def test_no_host_url_from_the_daemon_stops_with_a_hint(self):
        r = self.h.run("--token", TOKEN, "--dry-run", script=render(host_url="", pin=""))
        self.assertEqual(r.returncode, 2)
        self.assertIn("no host listener URL", r.stderr)
        self.assertIn("--host-url", r.stderr)

    def test_unfilled_script_needs_url(self):
        r = self.h.run("--token", TOKEN, "--dry-run", script=SCRIPT.read_text())
        self.assertEqual(r.returncode, 2)
        self.assertIn("no daemon URL", r.stderr)

    def test_bad_values_stop_before_any_change(self):
        for args, msg in (
            (["--name", "Bad.Name"], "bad host name"),
            (["--user", "Wil;rm"], "bad user name"),
            (["--url", "http://x/'$(id)'"], "bad daemon URL"),
            (["--purge"], "--purge goes with --uninstall"),
            (["--bogus"], "unknown argument"),
        ):
            r = self.h.run("--token", TOKEN, *args)
            self.assertEqual(r.returncode, 2, args)
            self.assertIn(msg, r.stderr, args)
        r = self.h.run("--token", "the_bad token!")
        self.assertEqual(r.returncode, 2)
        self.assertNotIn("the_bad", r.stderr)
        self.assertEqual(self.mutations(), [])

    def test_needs_root(self):
        r = self.h.run("--token", TOKEN, env={"FAKE_UID": "1000"})
        self.assertEqual(r.returncode, 2)
        self.assertIn("run as root", r.stderr)

    def test_new_host_needs_a_token(self):
        r = self.h.run()
        self.assertEqual(r.returncode, 2)
        self.assertIn("no token", r.stderr)
        self.assertEqual(self.mutations(), [])

    # -- the install

    def test_install(self):
        self.h.tool("usr/bin/docker")
        self.h.tool(str(self.h.home / ".local/bin/vmlab"))
        r = self.h.run("--token", TOKEN)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertNoToken(r)
        # The binary is the download, checked.
        self.assertEqual(self.h.file("/usr/local/bin/terrarium-host").read_text(), FAKE_HOST)
        self.assertIn("SHA-256", r.stdout)
        # The config and the token.
        config = self.h.file("/etc/terrarium-host/config.toml").read_text()
        self.assertIn('name = "lab1"', config)
        self.assertEqual(self.h.file("/etc/terrarium-host/token").read_text(), TOKEN + "\n")
        self.assertEqual(oct(self.h.file("/etc/terrarium-host").stat().st_mode & 0o777), "0o700")
        self.assertEqual(oct(self.h.file("/etc/terrarium-host/config.toml").stat().st_mode & 0o777), "0o640")
        unit = self.h.file("/etc/systemd/system/terrarium-host.service").read_text()
        self.assertIn("\nUser=wil\n", unit)
        calls = self.h.calls()
        self.assertIn("usermod -aG docker wil", calls)
        self.assertIn("usermod -aG kvm wil", calls)
        self.assertNotIn("useradd", " ".join(calls))
        self.assertIn("chown wil:wil " + str(self.h.file("/etc/terrarium-host")), calls)
        self.assertIn("chown root:wil " + str(self.h.file("/etc/terrarium-host/config.toml.new")), calls)
        sc = [c for c in calls if c.startswith("systemctl")]
        self.assertEqual(sc, ["systemctl daemon-reload", "systemctl enable terrarium-host", "systemctl restart terrarium-host"])
        # The token went to the service user's token-file, then the probe ran.
        hostcalls = [c for c in calls if c.startswith(("runuser", "terrarium-host"))]
        self.assertEqual(hostcalls[0], "runuser -u wil")
        self.assertTrue(hostcalls[1].startswith("terrarium-host token-file "), hostcalls)
        self.assertTrue(hostcalls[-1].endswith("check --probe"), hostcalls)
        self.assertIn("  docker   yes", r.stdout)
        self.assertIn("  vmlab    yes", r.stdout)
        self.assertIn("  daemon   wss://terrarium.lan:8082", r.stdout)

    def test_second_run_keeps_config_and_token(self):
        self.assertEqual(self.h.run("--token", TOKEN).returncode, 0)
        cfg = self.h.file("/etc/terrarium-host/config.toml")
        cfg.write_text(cfg.read_text() + "# edited\n")
        r = self.h.run("--name", "other")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("keep the existing /etc/terrarium-host/config.toml", r.stdout)
        self.assertIn("keep the existing /etc/terrarium-host/token", r.stdout)
        self.assertIn("names the host `lab1`", r.stderr)
        self.assertIn("# edited", cfg.read_text())
        self.assertEqual(self.h.file("/etc/terrarium-host/token").read_text(), TOKEN + "\n")
        # A new token replaces the file.
        new = "the_" + "cd" * 32
        r = self.h.run("--token", new)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("replace the existing /etc/terrarium-host/token", r.stdout)
        self.assertEqual(self.h.file("/etc/terrarium-host/token").read_text(), new + "\n")

    def test_bad_checksum_installs_nothing(self):
        self.h.publish(FAKE_HOST.encode(), sha="0" * 64)
        r = self.h.run("--token", TOKEN)
        self.assertEqual(r.returncode, 2)
        self.assertIn("not installed", r.stderr)
        self.assertFalse(self.h.file("/usr/local/bin/terrarium-host").exists())
        self.assertFalse(self.h.file("/etc/terrarium-host/token").exists())

    def test_unknown_arch_shows_the_daemon_message(self):
        r = self.h.run("--token", TOKEN, env={"FAKE_ARCH": "arm64"})
        self.assertEqual(r.returncode, 2)
        self.assertIn("answered 404", r.stderr)
        self.assertIn("arch `aarch64`", r.stderr)

    def test_no_systemd_prints_the_run_command(self):
        shutil.rmtree(self.h.root / "run")
        r = self.h.run("--token", TOKEN)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("no systemd", r.stdout)
        self.assertIn("runuser -u wil -- /usr/local/bin/terrarium-host --config /etc/terrarium-host/config.toml run", r.stdout)
        self.assertFalse([c for c in self.h.calls() if c.startswith("systemctl")])
        self.assertFalse(self.h.file("/etc/systemd/system/terrarium-host.service").exists())

    def test_failed_probe_exits_1(self):
        r = self.h.run("--token", TOKEN, env={"FAKE_PROBE_FAIL": "1"})
        self.assertEqual(r.returncode, 1)
        self.assertIn("journalctl -u terrarium-host", r.stderr)

    def test_without_sudo_user_a_system_user(self):
        r = self.h.run("--token", TOKEN, env={"SUDO_USER": ""})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(
            "useradd --system --user-group --home-dir /var/lib/terrarium-host --create-home --shell /usr/sbin/nologin terrarium",
            self.h.calls(),
        )
        self.assertIn("\nUser=terrarium\n", self.h.file("/etc/systemd/system/terrarium-host.service").read_text())

    def test_docker_group_kept_when_member(self):
        self.h.tool("usr/bin/docker")
        self.h.groups_of.write_text("docker\n")
        r = self.h.run("--token", TOKEN)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("wil is in the group docker", r.stdout)
        self.assertFalse([c for c in self.h.calls() if c.startswith("usermod")])

    # -- upgrade and uninstall

    def test_upgrade_replaces_the_binary_only(self):
        self.assertEqual(self.h.run("--upgrade").returncode, 2)  # not installed
        self.assertEqual(self.h.run("--token", TOKEN).returncode, 0)
        newer = FAKE_HOST + "# v2\n"
        self.h.publish(newer.encode())
        cfg = self.h.file("/etc/terrarium-host/config.toml").read_text()
        self.h.log.write_text("")
        r = self.h.run("--upgrade")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.h.file("/usr/local/bin/terrarium-host").read_text(), newer)
        self.assertEqual(self.h.file("/etc/terrarium-host/config.toml").read_text(), cfg)
        self.assertEqual(self.h.file("/etc/terrarium-host/token").read_text(), TOKEN + "\n")
        self.assertEqual([c for c in self.h.calls() if c.startswith("systemctl")], ["systemctl restart terrarium-host"])

    def test_uninstall_keeps_config_unless_purge(self):
        self.assertEqual(self.h.run("--token", TOKEN).returncode, 0)
        r = self.h.run("--uninstall", "--dry-run")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(self.h.file("/usr/local/bin/terrarium-host").exists())
        r = self.h.run("--uninstall")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(self.h.file("/usr/local/bin/terrarium-host").exists())
        self.assertFalse(self.h.file("/etc/systemd/system/terrarium-host.service").exists())
        self.assertTrue(self.h.file("/etc/terrarium-host/config.toml").exists())
        self.assertIn("terrarium admin host rm lab1", r.stdout)
        self.assertIn("systemctl disable terrarium-host", self.h.calls())
        r = self.h.run("--uninstall", "--purge")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(self.h.file("/etc/terrarium-host").exists())
        # Again: nothing to do, still fine.
        self.assertEqual(self.h.run("--uninstall", "--purge").returncode, 0)


class Lint(unittest.TestCase):
    def test_posix_syntax(self):
        r = subprocess.run(["sh", "-n", str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)

    @unittest.skipUnless(shutil.which("shellcheck"), "shellcheck is not on PATH")
    def test_shellcheck(self):
        r = subprocess.run(["shellcheck", "-s", "sh", str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout)

    def test_placeholders(self):
        text = SCRIPT.read_text()
        for key in ("@TERRARIUM_URL@", "@TERRARIUM_HOST_URL@", "@TERRARIUM_HOST_PIN@", "@TERRARIUM_VERSION@", "@TERRARIUM_HOST_UNIT@"):
            self.assertIn(key, text)
        self.assertNotIn("@TERRARIUM_", render())


if __name__ == "__main__":
    unittest.main()

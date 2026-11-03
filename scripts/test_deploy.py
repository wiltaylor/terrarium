"""Tests for scripts/deploy.py: the ref and dirty rules, .env, the preflight,
the order of the steps, rollback, the dry run, and the scripts that run on
the target (in a temp dir, with a stub `docker`). Nothing here dials a host,
runs docker, or builds an image. The last class checks the CI workflow.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import re
import shlex
import subprocess
import sys
import tarfile
import tempfile
import unittest
import unittest.mock
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("deploy", HERE / "deploy.py")
assert _spec and _spec.loader
dp = importlib.util.module_from_spec(_spec)
sys.modules["deploy"] = dp  # dataclasses look the module up here
_spec.loader.exec_module(dp)

NOW = datetime(2026, 10, 28, 12, 0, 0, tzinfo=timezone.utc)
COMMIT = "0123456789abcdef0123456789abcdef01234567"
TOKEN = "f00dfeedcafebeef" * 4
EXAMPLE = """# example
TERRARIUM_TOKEN=change-me
TERRARIUM_IMAGE=terrarium:dev
#TERRARIUM_UID=1000
#TERRARIUM_GID=1000
TERRARIUM_PORT=8080
TERRARIUM_ARTIFACT_PORT=8081
TERRARIUM_HOST_PORT=8082
TERRARIUM_INGEST_PORT=8083
TERRARIUM_PUBLIC_URL=http://localhost:8080
RUST_LOG=info
"""


def fake_git(dirty: bool = False, commits: dict[str, str] | None = None, files: str = "a\0b\0gone\0"):
    commits = commits if commits is not None else {"HEAD": COMMIT, "v0.1.0": "f" * 40}

    def run(*args: str) -> str:
        if args[0] == "rev-parse":
            ref = args[-1].removesuffix("^{commit}")
            return commits.get(ref, "") + ("\n" if ref in commits else "")
        if args[0] == "status":
            return " M crates/x.rs\n" if dirty else ""
        if args[0] == "ls-files":
            return files
        if args[0] == "show":
            return EXAMPLE
        raise AssertionError(f"unexpected git {args}")

    return run


def probe_out(**kw) -> str:
    base = {
        "dir": "/home/wil/terrarium", "user": "wil", "uid": "1000", "gid": "1000",
        "groups": "wil docker", "arch": "x86_64", "docker": "/usr/bin/docker",
        "docker_access": "yes", "compose": "2.29.1", "git": "/usr/bin/git",
        "curl": "/usr/bin/curl", "free_kb": str(50 * 1024 * 1024),
    }
    extra = kw.pop("extra", [])
    base.update(kw)
    return "\n".join([f"{k}={v}" for k, v in base.items()] + extra) + "\n"


EXISTING = [
    "has=src", "has=src.prev", "has=.env",
    "env_keys=TERRARIUM_TOKEN TERRARIUM_PORT TERRARIUM_PUBLIC_URL",
    "env_port=9090", "env_public_url=http://t.lan:9090",
    "src.ref=" + "e" * 40, "src.image=terrarium-deploy:eeeeeeeeeeee",
    "src.prev.ref=" + "d" * 40,
]


def script_of(argv: list[str]) -> tuple[str, list[str]]:
    """The script and its $1... of a target command, local or over ssh, with
    or without `--sudo-user`."""
    cmd = shlex.split(argv[-1]) if argv[0] == "ssh" else argv
    if cmd[0] == "sudo":
        cmd = sudo_inner(cmd)
    assert cmd[0] == "sh" and cmd[1] in ("-c", "-ec"), cmd
    assert cmd[3] == "deploy"
    return cmd[2], cmd[4:]


def sudo_inner(words: list[str]) -> list[str]:
    """The command that `sudo -n -H -u <user> -- sh -c SUDO_CD sh ...` runs."""
    assert words[:3] == ["sudo", "-n", "-H"] and words[3] == "-u" and words[5:10] == \
        ["--", "sh", "-c", dp.SUDO_CD, "sh"], words
    return words[10:]


NAMES = {
    dp.PROBE: "probe", dp.LAYOUT: "layout", dp.RECEIVE: "receive", dp.ENV_WRITE: "env",
    dp.BUILD: "build", dp.SWAP: "swap", dp.ROLLBACK_SWAP: "rollback-swap", dp.UP: "up",
    dp.HEALTH: "health", dp.PRUNE: "prune", dp.STATUS: "status",
}


class FakeRunner(dp.Runner):
    """Records the steps that run. `fail` maps a step name to an exit code."""

    def __init__(self, dry_run: bool, probe: str, fail: dict[str, int] | None = None):
        super().__init__(dry_run)
        self.probe = probe
        self.fail = fail or {}
        self.ran: list[str] = []
        self.stdin: dict[str, str] = {}
        self.argv: dict[str, list[str]] = {}

    def step(self, argv: list[str]) -> str:
        script, _ = script_of(argv)
        return NAMES[script]

    def execute(self, argv, stdin, capture):
        name = self.step(argv)
        self.ran.append(name)
        self.argv[name] = argv
        if stdin is not None:
            self.stdin[name] = stdin
        out = self.probe if name == "probe" else ""
        return subprocess.CompletedProcess(argv, self.fail.get(name, 0), out, "")

    def execute_pipe(self, producer, producer_stdin, consumer):
        name = self.step(consumer)
        self.ran.append(name)
        self.argv[name] = consumer
        self.argv["producer"] = producer
        return self.fail.get(name, 0)


def run_main(argv: list[str], runner: FakeRunner, git=None, isatty=False) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    stdin = unittest.mock.MagicMock()
    stdin.isatty.return_value = isatty
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err), \
            unittest.mock.patch.object(sys, "stdin", stdin):
        kw = {}
        if argv[0] in ("server", "update"):
            kw = dict(run_git=git or fake_git(), token_fn=lambda: TOKEN, now=NOW)
        code = dp.main(argv, runner=runner, **kw)
    return code, out.getvalue(), err.getvalue()


class Source(unittest.TestCase):
    def test_clean_head(self):
        s = dp.resolve_source(None, False, fake_git(), NOW)
        self.assertEqual((s.ref, s.commit, s.dirty, s.files), ("HEAD", COMMIT, False, None))
        self.assertEqual(s.tag, COMMIT[:12])
        self.assertEqual(s.producer(Path("/r")), (["git", "-C", "/r", "archive", "--format=tar", COMMIT], None))

    def test_dirty_head_is_refused(self):
        with self.assertRaisesRegex(dp.DeployError, "--allow-dirty"):
            dp.resolve_source(None, False, fake_git(dirty=True), NOW)

    def test_allow_dirty_packs_the_tracked_files(self):
        s = dp.resolve_source(None, True, fake_git(dirty=True), NOW, exists=lambda f: f != "gone")
        self.assertTrue(s.dirty)
        self.assertEqual(s.files, ["a", "b"])  # a deleted file does not go
        self.assertEqual(s.tag, f"{COMMIT[:12]}-dirty-20261028120000")
        argv, names = s.producer(Path("/r"))
        self.assertEqual(argv, ["tar", "-C", "/r", "--null", "--no-recursion", "-T", "-", "-cf", "-"])
        self.assertEqual(names, b"a\0b\0")
        self.assertEqual(s.ref_lines("p:t", NOW)[0], f"ref={COMMIT}-dirty")

    def test_allow_dirty_on_a_clean_tree_is_a_normal_deploy(self):
        s = dp.resolve_source(None, True, fake_git(), NOW)
        self.assertFalse(s.dirty)
        self.assertIsNone(s.files)

    def test_ref_ignores_the_working_tree(self):
        s = dp.resolve_source("v0.1.0", False, fake_git(dirty=True), NOW)
        self.assertEqual((s.commit, s.dirty), ("f" * 40, False))

    def test_allow_dirty_with_a_ref_is_refused(self):
        with self.assertRaisesRegex(dp.DeployError, "does not go with --ref"):
            dp.resolve_source("v0.1.0", True, fake_git(), NOW)

    def test_unknown_ref(self):
        with self.assertRaisesRegex(dp.DeployError, "not a commit"):
            dp.resolve_source("nope", False, fake_git(), NOW)

    def test_ref_lines(self):
        s = dp.resolve_source(None, False, fake_git(), NOW)
        self.assertEqual(
            s.ref_lines("terrarium-deploy:0123456789ab", NOW),
            [f"ref={COMMIT}", "source=HEAD", "image=terrarium-deploy:0123456789ab", "time=2026-10-28T12:00:00Z"],
        )


class Env(unittest.TestCase):
    def test_render(self):
        text = dp.render_env(EXAMPLE, dp.env_values(TOKEN, "1001", "1002", "http://t.lan:9000", 9000), NOW)
        lines = text.splitlines()
        self.assertTrue(lines[0].startswith("# Made by `just deploy`"))
        for want in (f"TERRARIUM_TOKEN={TOKEN}", "TERRARIUM_UID=1001", "TERRARIUM_GID=1002",
                     "TERRARIUM_PORT=9000", "TERRARIUM_ARTIFACT_PORT=9001", "TERRARIUM_HOST_PORT=127.0.0.1:9002",
                     "TERRARIUM_INGEST_PORT=9003", "TERRARIUM_PUBLIC_URL=http://t.lan:9000", "RUST_LOG=info"):
            self.assertIn(want, lines)
        self.assertNotIn("change-me", text)
        self.assertNotIn("#TERRARIUM_UID=1000", lines)
        # Each deploy sets the image: no active TERRARIUM_IMAGE line.
        self.assertFalse(any(l.startswith("TERRARIUM_IMAGE=") for l in lines))

    def test_new_env_opens_the_host_dial_in_on_loopback(self):
        # No host TLS in a new .env: the listener on all addresses in the
        # container, the port on loopback only, and daemon_url at the
        # published port (#3).
        v = dp.env_values(TOKEN, "1000", "1000", "http://t.lan:8090", 8090)
        self.assertEqual(v["TERRARIUM_HOST_PORT"], "127.0.0.1:8092")
        self.assertEqual(v["TERRARIUM_HOST_LISTEN"], "0.0.0.0:8082")
        self.assertEqual(v["TERRARIUM_HOST_DAEMON_URL"], "ws://localhost:8092")
        self.assertEqual(dp.published_port(v["TERRARIUM_HOST_PORT"]), 8092)
        v = dp.env_values(TOKEN, "1000", "1000", "http://t.lan:8080", 8080)
        self.assertEqual(v["TERRARIUM_HOST_DAEMON_URL"], "ws://localhost:8082")

    def test_new_env_sets_the_commented_host_lines_of_the_real_example(self):
        example = (dp.REPO / ".env.example").read_text()
        lines = dp.render_env(example, dp.env_values(TOKEN, "1000", "1000", "http://x:8090", 8090), NOW).splitlines()
        for want in ("TERRARIUM_HOST_PORT=127.0.0.1:8092", "TERRARIUM_HOST_LISTEN=0.0.0.0:8082",
                     "TERRARIUM_HOST_DAEMON_URL=ws://localhost:8092"):
            self.assertEqual(lines.count(want), 1, want)
        self.assertNotIn("#TERRARIUM_HOST_LISTEN=auto", lines)
        # No TLS in a new .env.
        self.assertFalse(any(l.startswith("TERRARIUM_HOST_TLS_") for l in lines))

    def test_host_link_note(self):
        new = dp.host_link_note(None, 8090)
        self.assertIn("127.0.0.1:8092", new)
        self.assertIn("only a host agent on this machine", new)
        self.assertIn("Remote host agents need TLS", new)
        # An old .env with no listener: no host agent can connect at all.
        old = dp.host_link_note({"TERRARIUM_TOKEN"}, 8080)
        self.assertIn("no host agent can connect", old)
        self.assertIn("TERRARIUM_HOST_LISTEN=0.0.0.0:8082", old)
        plain = dp.host_link_note({"TERRARIUM_HOST_LISTEN"}, 8080)
        self.assertTrue(plain.startswith("no host TLS. Remote host agents need TLS"), plain)
        self.assertIsNone(dp.host_link_note(set(dp.HOST_TLS_KEYS), 8080))

    def test_render_appends_an_unknown_key(self):
        text = dp.render_env("A=1\n", {"B": "2"}, NOW)
        self.assertTrue(text.endswith("A=1\nB=2\n"))

    def test_render_against_the_real_example(self):
        example = (dp.REPO / ".env.example").read_text()
        text = dp.render_env(example, dp.env_values(TOKEN, "1000", "1000", "http://x:8080", 8080), NOW)
        self.assertEqual(sum(l == f"TERRARIUM_TOKEN={TOKEN}" for l in text.splitlines()), 1)
        self.assertIn("\nTERRARIUM_UID=1000\n", text)
        self.assertEqual(dp.missing_env_keys(example, set(dp.example_keys(text))), [])

    def test_missing_keys(self):
        self.assertEqual(
            dp.missing_env_keys(EXAMPLE, {"TERRARIUM_TOKEN", "TERRARIUM_PORT", "TERRARIUM_PUBLIC_URL"}),
            ["TERRARIUM_ARTIFACT_PORT", "TERRARIUM_HOST_PORT", "TERRARIUM_INGEST_PORT", "RUST_LOG"],
        )

    def test_published_port(self):
        self.assertEqual(dp.published_port("8080"), 8080)
        self.assertEqual(dp.published_port("127.0.0.1:9090"), 9090)
        self.assertIsNone(dp.published_port(""))


class Preflight(unittest.TestCase):
    def levels(self, **kw) -> dict[str, list[str]]:
        out: dict[str, list[str]] = {}
        for c in dp.preflight(dp.parse_probe(probe_out(**kw))):
            out.setdefault(c.level, []).append(c.text)
        return out

    def test_all_good(self):
        r = self.levels()
        self.assertNotIn("error", r)
        self.assertNotIn("warn", r)

    def test_no_docker(self):
        r = self.levels(docker="", docker_access="no", compose="")
        self.assertTrue(any("get.docker.com" in t for t in r["error"]), r)

    def test_not_in_the_docker_group(self):
        r = self.levels(docker_access="no", groups="wil")
        self.assertTrue(any("sudo usermod -aG docker wil" in t for t in r["error"]), r)

    def test_docker_daemon_down(self):
        r = self.levels(docker_access="no")
        self.assertTrue(any("systemctl enable --now docker" in t for t in r["error"]), r)

    def test_no_compose_plugin(self):
        r = self.levels(compose="")
        self.assertTrue(any("docker-compose-plugin" in t for t in r["error"]), r)

    def test_old_compose(self):
        r = self.levels(compose="2.12.0")
        self.assertTrue(any("2.20 or later" in t for t in r["error"]), r)

    def test_git_is_optional(self):
        r = self.levels(git="")
        self.assertNotIn("error", r)
        self.assertTrue(any("optional" in t for t in r["info"]))

    def test_low_disk(self):
        r = self.levels(free_kb=str(3 * 1024 * 1024))
        self.assertTrue(any("3.0 GB free" in t for t in r["warn"]), r)

    def test_arm64_warns(self):
        r = self.levels(arch="aarch64")
        self.assertNotIn("error", r)
        self.assertTrue(any("untested" in t or "nobody has tested" in t for t in r["warn"]))

    def test_other_arch_and_root_fail(self):
        self.assertTrue(any("not supported" in t for t in self.levels(arch="riscv64")["error"]))
        self.assertTrue(any("root" in t for t in self.levels(uid="0")["error"]))

    def test_parse_probe(self):
        p = dp.parse_probe(probe_out(extra=EXISTING + ["project=terrarium /home/wil/dev/terrarium"]))
        self.assertEqual(p.has, {"src", "src.prev", ".env"})
        self.assertEqual(p.env_port, "9090")
        self.assertEqual(p.refs["src.prev.ref"], "d" * 40)
        self.assertEqual(p.refs["src.image"], "terrarium-deploy:eeeeeeeeeeee")
        self.assertEqual(p.projects, [("terrarium", "/home/wil/dev/terrarium")])


class Names(unittest.TestCase):
    def test_project_name(self):
        self.assertEqual(dp.project_name("/home/wil/terrarium"), "terrarium-deploy")
        self.assertEqual(dp.project_name("/tmp/tt-deploy"), "terrarium-tt-deploy")
        self.assertEqual(dp.project_name("/srv/terrarium-prod"), "terrarium-prod")
        self.assertEqual(dp.project_name("/srv/My App"), "terrarium-my-app")

    def test_project_of_another_dir_is_refused(self):
        p = dp.parse_probe(probe_out(extra=["project=terrarium-deploy /other/src"]))
        with self.assertRaisesRegex(dp.DeployError, "belongs to /other/src"):
            dp.check_project(p, "terrarium-deploy", "/home/wil/terrarium")
        p = dp.parse_probe(probe_out(extra=["project=terrarium-deploy /home/wil/terrarium/src"]))
        dp.check_project(p, "terrarium-deploy", "/home/wil/terrarium")

    def test_refuse_the_dev_repo(self):
        for d in (str(dp.REPO), str(dp.REPO / "deploy"), str(dp.REPO) + "/"):
            with self.assertRaisesRegex(dp.DeployError, "dev stack"):
                dp.refuse_dev_dir(d, dp.REPO)
        dp.refuse_dev_dir("/tmp/tt-deploy", dp.REPO)
        dp.refuse_dev_dir(str(dp.REPO) + "-deploy", dp.REPO)  # a sibling, not the repo

    def test_ssh_argv(self):
        t = dp.Target("wil@t.lan", ["-p", "2222"])
        argv = t.argv(dp.SWAP, "/home/wil/my dir")
        self.assertEqual(argv[:4], ["ssh", "-p", "2222", "wil@t.lan"])
        self.assertEqual(shlex.split(argv[-1]), ["sh", "-ec", dp.SWAP, "deploy", "/home/wil/my dir"])
        self.assertEqual(dp.Target("local").argv(dp.SWAP, "/d"), ["sh", "-ec", dp.SWAP, "deploy", "/d"])
        self.assertEqual(t.host, "t.lan")
        self.assertEqual(dp.Target("local").host, "localhost")

    def test_wrapper(self):
        w = dp.wrapper_script("terrarium-tt")
        self.assertIn("--project-name terrarium-tt ", w)
        self.assertIn('--env-file "$d/.env"', w)
        self.assertIn('export TERRARIUM_DATA_DIR="$d"', w)
        self.assertIn('cd "$src"', w)  # compose reads the seccomp profile from the cwd


class Flows(unittest.TestCase):
    STEPS = ["probe", "layout", "receive", "env", "build", "swap", "up", "health", "prune"]

    def test_first_install(self):
        r = FakeRunner(False, probe_out(extra=["project=terrarium /home/wil/dev/terrarium"]))
        code, out, err = run_main(["server", "wil@t.lan", "--public-url", "http://t.lan:8080"], r)
        self.assertEqual(code, 0, err)
        # The build comes before the swap; the swap before `up`.
        self.assertEqual(r.ran, self.STEPS)
        env = r.stdin["env"]
        self.assertIn(f"TERRARIUM_TOKEN={TOKEN}\n", env)
        self.assertIn("TERRARIUM_PUBLIC_URL=http://t.lan:8080\n", env)
        # The token: once, in the summary; never in a command line.
        self.assertEqual(out.count(TOKEN), 1, out)
        self.assertNotIn(TOKEN, err)
        self.assertFalse(any(TOKEN in l for l in out.splitlines() if l.startswith("+ ")))
        self.assertIn("shown once", out)
        self.assertIn("terrariumd password set", out)
        self.assertIn("/install/host.sh", out)
        # The source lands in src.new with its .deployed-ref.
        _, args = script_of(r.argv["receive"])
        self.assertEqual(args[0], "/home/wil/terrarium")
        self.assertIn(f"ref={COMMIT}", args)
        self.assertIn(f"image=terrarium-deploy:{COMMIT[:12]}", args)
        self.assertEqual(r.argv["producer"][-3:], ["archive", "--format=tar", COMMIT])
        # The wrapper goes with the layout step.
        self.assertIn("--project-name terrarium-deploy ", r.stdin["layout"])
        _, args = script_of(r.argv["prune"])
        self.assertEqual(args, ["terrarium-deploy", COMMIT[:12]])

    def test_default_public_url_and_port(self):
        r = FakeRunner(False, probe_out())
        code, out, _ = run_main(["server", "wil@t.lan", "--port", "18090"], r)
        self.assertEqual(code, 0)
        self.assertIn("TERRARIUM_PUBLIC_URL=http://t.lan:18090\n", r.stdin["env"])
        self.assertIn("TERRARIUM_INGEST_PORT=18093\n", r.stdin["env"])
        self.assertIn("TERRARIUM_HOST_PORT=127.0.0.1:18092\n", r.stdin["env"])
        self.assertIn("TERRARIUM_HOST_DAEMON_URL=ws://localhost:18092\n", r.stdin["env"])
        self.assertIn("pass --public-url", out)
        # The no-TLS warning: as the step runs, and again in the summary.
        self.assertEqual(out.count("only a host agent on this machine can connect"), 2, out)
        self.assertEqual(script_of(r.argv["health"])[1], ["/home/wil/terrarium", "18090"])

    def test_reinstall_keeps_the_env(self):
        r = FakeRunner(False, probe_out(extra=EXISTING))
        code, out, err = run_main(["server", "wil@t.lan", "--yes", "--port", "8080"], r)
        self.assertEqual(code, 0, err)
        self.assertNotIn("env", r.ran)
        self.assertNotIn(TOKEN, out)
        self.assertIn("keep the existing /home/wil/terrarium/.env", out)
        self.assertIn(".env has no line for: TERRARIUM_ARTIFACT_PORT", out)
        self.assertIn("--port 8080 applies only to a new .env", out)
        self.assertIn("no host agent can connect", out)
        self.assertEqual(script_of(r.argv["health"])[1][1], "9090")
        # The image of the old src stays, for a rollback.
        self.assertEqual(script_of(r.argv["prune"])[1], ["terrarium-deploy", COMMIT[:12], "eeeeeeeeeeee"])

    def test_reinstall_asks_first(self):
        r = FakeRunner(False, probe_out(extra=EXISTING))
        code, _, err = run_main(["server", "wil@t.lan"], r)
        self.assertEqual(code, 2)
        self.assertIn("Pass --yes", err)
        self.assertEqual(r.ran, ["probe"])
        r = FakeRunner(False, probe_out(extra=EXISTING))
        with unittest.mock.patch("builtins.input", return_value="n"):
            code, _, _ = run_main(["server", "wil@t.lan"], r, isatty=True)
        self.assertEqual((code, r.ran), (1, ["probe"]))

    def test_build_failure_keeps_src_and_the_stack(self):
        r = FakeRunner(False, probe_out(extra=EXISTING), fail={"build": 1})
        code, _, err = run_main(["update", "wil@t.lan"], r)
        self.assertEqual(code, 2)
        self.assertEqual(r.ran, ["probe", "layout", "receive", "build"])
        self.assertIn("did not change", err)

    def test_update(self):
        r = FakeRunner(False, probe_out(extra=EXISTING))
        code, _, err = run_main(["update", "wil@t.lan", "--ref", "v0.1.0"], r)
        self.assertEqual(code, 0, err)
        self.assertEqual(r.ran, ["probe", "layout", "receive", "build", "swap", "up", "health", "prune"])
        self.assertEqual(r.argv["producer"][-1], "f" * 40)

    def test_update_needs_an_install(self):
        r = FakeRunner(False, probe_out())
        code, _, err = run_main(["update", "wil@t.lan"], r)
        self.assertEqual((code, r.ran), (2, ["probe"]))
        self.assertIn("just deploy::server wil@t.lan", err)

    def test_dirty_tree_stops_before_the_target(self):
        r = FakeRunner(False, probe_out())
        code, _, err = run_main(["server", "wil@t.lan"], r, git=fake_git(dirty=True))
        self.assertEqual((code, r.ran), (2, []))
        self.assertIn("--allow-dirty", err)

    def test_preflight_error_changes_nothing(self):
        r = FakeRunner(False, probe_out(docker_access="no", groups="wil"))
        code, out, err = run_main(["server", "wil@t.lan"], r)
        self.assertEqual((code, r.ran), (2, ["probe"]))
        self.assertIn("sudo usermod -aG docker wil", out)
        self.assertIn("Nothing changed", err)

    def test_dry_run_changes_nothing(self):
        r = FakeRunner(True, probe_out(extra=EXISTING))
        code, out, err = run_main(["server", "wil@t.lan", "--dry-run", "--dir", "/srv/t"], r)
        self.assertEqual(code, 0, err)
        self.assertEqual(r.ran, ["probe"])  # read-only
        self.assertIn("# would ask:", out)
        self.assertIn("nothing changed", out)
        printed = [l for l in out.splitlines() if l.startswith("+ ")]
        self.assertEqual(len(printed), 8)  # probe, layout, receive, build, swap, up, health, prune
        self.assertIn(" | ssh wil@t.lan ", printed[2])

    def test_dry_run_new_install_hides_the_token(self):
        r = FakeRunner(True, probe_out())
        token_calls = []
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = dp.main(["server", "wil@t.lan", "--dry-run"], runner=r, run_git=fake_git(),
                           token_fn=lambda: token_calls.append(1) or TOKEN, now=NOW)
        self.assertEqual(code, 0, err.getvalue())
        self.assertEqual(token_calls, [])
        self.assertNotIn("TERRARIUM_TOKEN (shown once", out.getvalue())
        self.assertIn("<.env from .env.example, with a new TERRARIUM_TOKEN>", out.getvalue())

    def test_local_refuses_the_dev_repo(self):
        r = FakeRunner(False, probe_out(dir=str(dp.REPO)))
        code, _, err = run_main(["server", "local", "--dir", str(dp.REPO)], r)
        self.assertEqual((code, r.ran), (2, ["probe"]))
        self.assertIn("dev stack", err)
        self.assertEqual(r.argv["probe"][0], "sh")  # no ssh

    def test_local_install(self):
        r = FakeRunner(False, probe_out(dir="/tmp/tt-deploy"))
        code, out, err = run_main(["server", "local", "--dir", "/tmp/tt-deploy", "--port", "18090"], r)
        self.assertEqual(code, 0, err)
        self.assertEqual(r.ran, self.STEPS)
        self.assertIn("TERRARIUM_PUBLIC_URL=http://localhost:18090\n", r.stdin["env"])
        self.assertIn("just deploy::update local --dir=/tmp/tt-deploy", out)
        self.assertIn("compose project terrarium-tt-deploy", out)

    def test_project_clash_stops(self):
        r = FakeRunner(False, probe_out(extra=["project=terrarium-deploy /somewhere/else"]))
        code, _, err = run_main(["server", "wil@t.lan"], r)
        self.assertEqual((code, r.ran), (2, ["probe"]))
        self.assertIn("belongs to /somewhere/else", err)

    def test_rollback(self):
        r = FakeRunner(False, probe_out(extra=EXISTING))
        code, out, err = run_main(["rollback", "wil@t.lan"], r)
        self.assertEqual(code, 0, err)
        self.assertEqual(r.ran, ["probe", "rollback-swap", "up", "health"])
        self.assertIn(f"now at {'d' * 40}", out)

    def test_rollback_needs_a_previous_deploy(self):
        r = FakeRunner(False, probe_out(extra=["has=src", "has=.env"]))
        code, _, err = run_main(["rollback", "wil@t.lan"], r)
        self.assertEqual((code, r.ran), (2, ["probe"]))
        self.assertIn("no deploy to go back to", err)

    def test_rollback_dry_run(self):
        r = FakeRunner(True, probe_out(extra=EXISTING))
        code, out, _ = run_main(["rollback", "wil@t.lan", "--dry-run"], r)
        self.assertEqual((code, r.ran), (0, ["probe"]))
        self.assertIn("nothing changed", out)

    def test_status_is_read_only(self):
        r = FakeRunner(False, probe_out(extra=EXISTING))
        code, _, _ = run_main(["status", "wil@t.lan"], r)
        self.assertEqual((code, r.ran), (0, ["probe", "status"]))
        self.assertEqual(script_of(r.argv["status"])[1], ["/home/wil/terrarium", "9090"])

    def test_ssh_options(self):
        r = FakeRunner(False, probe_out())
        run_main(["server", "wil@t.lan", "--ssh-option=-p 2222", "--ssh-option=-i", "--ssh-option=key"], r)
        self.assertEqual(r.argv["probe"][:6], ["ssh", "-p", "2222", "-i", "key", "wil@t.lan"])
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            dp.parse_args(["server", "local", "--ssh-option=-p"])

    def test_sudo_user_argv(self):
        t = dp.Target("meta@t.lan", ["-p", "2222"], "wil")
        argv = t.argv(dp.SWAP, "~/my dir")
        self.assertEqual(argv[:4], ["ssh", "-p", "2222", "meta@t.lan"])
        words = shlex.split(argv[-1])
        self.assertEqual(words[:5], ["sudo", "-n", "-H", "-u", "wil"])
        self.assertEqual(sudo_inner(words), ["sh", "-ec", dp.SWAP, "deploy", "~/my dir"])

    def test_sudo_user_runs_in_home_and_keeps_stdin(self):
        # The remote string, run by sh with a stub `sudo` that sets HOME, as
        # `sudo -H` does: the command starts in that home and gets stdin.
        with tempfile.TemporaryDirectory() as tmp:
            home, bin_ = Path(tmp, "home"), Path(tmp, "bin")
            home.mkdir()
            bin_.mkdir()
            stub = bin_ / "sudo"
            stub.write_text(f'#!/bin/sh\nshift 5\nHOME={shlex.quote(str(home))} exec "$@"\n')
            stub.chmod(0o755)
            argv = dp.Target("meta@t.lan", [], "wil").argv('pwd; cat; echo " $1"', "x")
            env = {**os.environ, "PATH": f"{bin_}:{os.environ['PATH']}"}
            r = subprocess.run(["sh", "-c", argv[-1]], input="in", capture_output=True, text=True,
                               cwd="/", env=env, check=False)
        self.assertEqual((r.returncode, r.stdout), (0, f"{home}\nin x\n"), r.stderr)

    def test_sudo_user_shell_hint(self):
        t = dp.Target("meta@t.lan", [], "wil")
        hint = t.shell_hint("/home/wil/t/terrarium-compose logs")
        self.assertEqual(hint, """ssh -t meta@t.lan "sudo -H -u wil sh -c 'cd ~ && /home/wil/t/terrarium-compose logs'\"""")
        words = shlex.split(hint)
        self.assertEqual(shlex.split(words[-1]), ["sudo", "-H", "-u", "wil", "sh", "-c",
                                                  "cd ~ && /home/wil/t/terrarium-compose logs"])
        odd = t.shell_hint('echo "$HOME"')  # no plain double quotes here: a second shlex quote
        self.assertEqual(shlex.split(shlex.split(odd)[-1])[-1], 'cd ~ && echo "$HOME"')
        self.assertEqual(dp.Target("meta@t.lan").shell_hint("ls"), "ssh -t meta@t.lan ls")

    def test_sudo_user_flag(self):
        r = FakeRunner(False, probe_out(extra=EXISTING))
        code, out, err = run_main(["rollback", "meta@t.lan", "--sudo-user", "wil", "--dir", "~/T"], r)
        self.assertEqual(code, 0, err)
        self.assertEqual(shlex.split(r.argv["probe"][-1])[:5], ["sudo", "-n", "-H", "-u", "wil"])
        self.assertIn("just deploy::rollback meta@t.lan '--dir=~/T' --sudo-user=wil again", out)
        r = FakeRunner(False, probe_out())
        code, out, err = run_main(["server", "meta@t.lan", "--sudo-user=wil"], r)
        self.assertEqual(code, 0, err)
        self.assertIn("just deploy::update meta@t.lan --sudo-user=wil", out)
        for bad in (["status", "local", "--sudo-user=wil"], ["status", "meta@t.lan", "--sudo-user=wil;id"],
                    ["status", "meta@t.lan", "--sudo-user=-u"], ["status", "meta@t.lan", "--sudo-user=Wil"]):
            with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                dp.parse_args(bad)


class Scripts(unittest.TestCase):
    """The target-side scripts, run for real with sh in a temp dir."""

    def sh(self, script: str, *args: str, stdin: bytes | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(["sh", "-ec", script, "deploy", *args], input=stdin, capture_output=True,
                              env=env or os.environ.copy(), check=False)

    def tar_of(self, files: dict[str, str]) -> bytes:
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as tf:
            for name, text in files.items():
                data = text.encode()
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tf.addfile(info, io.BytesIO(data))
        return buf.getvalue()

    def test_layout_receive_env_swap_rollback(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = os.path.join(tmp, "inst")
            r = self.sh(dp.LAYOUT, d, dp.WRAPPER, stdin=dp.wrapper_script("terrarium-inst").encode())
            self.assertEqual(r.returncode, 0, r.stderr)
            for sub in dp.DATA_DIRS:
                self.assertTrue(os.path.isdir(os.path.join(d, sub)), sub)
            self.assertTrue(os.access(os.path.join(d, dp.WRAPPER), os.X_OK))

            # Data in the data dirs must live through every step below.
            Path(d, "state", "db").write_text("keep")

            def receive(version: str):
                r = self.sh(dp.RECEIVE, d, f"ref={version}", "image=x:y",
                            stdin=self.tar_of({"compose.yaml": version, "docker/x": "1"}))
                self.assertEqual(r.returncode, 0, r.stderr)

            receive("v1")
            self.assertEqual(Path(d, "src.new", ".deployed-ref").read_text(), "ref=v1\nimage=x:y\n")
            self.assertEqual(self.sh(dp.SWAP, d).returncode, 0)
            self.assertFalse(os.path.exists(os.path.join(d, "src.new")))
            receive("v2")
            self.assertEqual(self.sh(dp.SWAP, d).returncode, 0)
            self.assertEqual(Path(d, "src", "compose.yaml").read_text(), "v2")
            self.assertEqual(Path(d, "src.prev", "compose.yaml").read_text(), "v1")

            # .env: made once, mode 0600, never replaced.
            r = self.sh(dp.ENV_WRITE, d, stdin=b"TERRARIUM_TOKEN=x\n")
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(os.stat(os.path.join(d, ".env")).st_mode & 0o777, 0o600)
            r = self.sh(dp.ENV_WRITE, d, stdin=b"TERRARIUM_TOKEN=other\n")
            self.assertEqual(r.returncode, 3)
            self.assertEqual(Path(d, ".env").read_text(), "TERRARIUM_TOKEN=x\n")

            # Rollback swaps; a second one goes forward again.
            self.assertEqual(self.sh(dp.ROLLBACK_SWAP, d).returncode, 0)
            self.assertEqual(Path(d, "src", "compose.yaml").read_text(), "v1")
            self.assertEqual(self.sh(dp.ROLLBACK_SWAP, d).returncode, 0)
            self.assertEqual(Path(d, "src", "compose.yaml").read_text(), "v2")

            # A re-layout keeps the data.
            self.sh(dp.LAYOUT, d, dp.WRAPPER, stdin=b"#!/bin/sh\n")
            self.assertEqual(Path(d, "state", "db").read_text(), "keep")

            # A swap with no src.new fails and changes nothing.
            self.assertNotEqual(self.sh(dp.SWAP, d).returncode, 0)
            self.assertEqual(Path(d, "src", "compose.yaml").read_text(), "v2")

    def stub_docker(self, bindir: str, log: str) -> None:
        stub = Path(bindir, "docker")
        stub.write_text(
            "#!/bin/sh\n"
            f'echo "$PWD|TERRARIUM_DATA_DIR=$TERRARIUM_DATA_DIR|TERRARIUM_IMAGE=$TERRARIUM_IMAGE|'
            f'TERRARIUM_PORT=${{TERRARIUM_PORT:-unset}}|$*" >> {shlex.quote(log)}\n'
            'case "$1 $2" in\n'
            '  "info "*) exit 0 ;;\n'
            '  "compose version") echo 2.29.1 ;;\n'
            '  "ps "*) echo "project=terrarium /home/x/dev/terrarium" ;;\n'
            "esac\n"
        )
        stub.chmod(0o755)

    def test_wrapper_runs_compose_for_the_install(self):
        with tempfile.TemporaryDirectory() as tmp:
            bindir, log = os.path.join(tmp, "bin"), os.path.join(tmp, "log")
            os.mkdir(bindir)
            self.stub_docker(bindir, log)
            d = os.path.join(tmp, "inst")
            self.sh(dp.LAYOUT, d, dp.WRAPPER, stdin=dp.wrapper_script("terrarium-inst").encode())
            for sub in ("src", "src.new"):
                os.makedirs(os.path.join(d, sub))
            Path(d, "src", ".deployed-ref").write_text("ref=a\nimage=terrarium-inst:aaa\n")
            Path(d, "src.new", ".deployed-ref").write_text("ref=b\nimage=terrarium-inst:bbb\n")
            env = dict(os.environ, PATH=f"{bindir}:{os.environ['PATH']}",
                       TERRARIUM_PORT="8080", TERRARIUM_IMAGE="terrarium:dev")
            # No .deployed-ref: the wrapper still runs, with a fallback image.
            os.makedirs(os.path.join(d, "src.prev"))
            r = subprocess.run([os.path.join(d, dp.WRAPPER), "ps"], env=dict(env, TERRARIUM_SRC=os.path.join(d, "src.prev")),
                               capture_output=True, text=True, check=False)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("TERRARIUM_IMAGE=terrarium-inst:latest|", Path(log).read_text())
            os.remove(log)
            self.assertEqual(self.sh(dp.BUILD, d, env=env).returncode, 0)
            self.assertEqual(self.sh(dp.UP, d, env=env).returncode, 0)
            build, up = Path(log).read_text().splitlines()
            real = os.path.realpath(d)
            # The build reads src.new and its image; `up` reads src.
            self.assertTrue(build.startswith(f"{real}/src.new|TERRARIUM_DATA_DIR={real}|TERRARIUM_IMAGE=terrarium-inst:bbb|"), build)
            self.assertIn(f"--project-directory {real}/src.new --file {real}/src.new/compose.yaml --env-file {real}/.env build", build)
            self.assertTrue(up.startswith(f"{real}/src|TERRARIUM_DATA_DIR={real}|TERRARIUM_IMAGE=terrarium-inst:aaa|"), up)
            self.assertIn("--project-name terrarium-inst ", up)
            self.assertIn(f"up -d --wait --wait-timeout {dp.UP_TIMEOUT_S}", up)
            # The caller's TERRARIUM_* (just exports them) do not reach compose.
            self.assertIn("TERRARIUM_PORT=unset", up)

    def test_probe(self):
        with tempfile.TemporaryDirectory() as tmp:
            bindir, log = os.path.join(tmp, "bin"), os.path.join(tmp, "log")
            os.mkdir(bindir)
            self.stub_docker(bindir, log)
            d = os.path.join(tmp, "inst")
            os.makedirs(os.path.join(d, "src"))
            Path(d, ".env").write_text("TERRARIUM_TOKEN=secret-value\nTERRARIUM_PORT=9090\n")
            Path(d, "src", ".deployed-ref").write_text("ref=abc\nimage=p:abc\n")
            env = dict(os.environ, PATH=f"{bindir}:{os.environ['PATH']}", HOME=tmp)
            r = subprocess.run(["sh", "-c", dp.PROBE, "deploy", "~/inst"], capture_output=True, text=True, env=env, check=False)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertNotIn("secret-value", r.stdout)  # key names only
            p = dp.parse_probe(r.stdout)
            self.assertEqual(p.dir, d)
            self.assertEqual(p.has, {"src", ".env"})
            self.assertEqual(p.env_keys, {"TERRARIUM_TOKEN", "TERRARIUM_PORT"})
            self.assertEqual(p.env_port, "9090")
            self.assertEqual(p.refs, {"src.ref": "abc", "src.image": "p:abc"})
            self.assertTrue(p.docker_access)
            self.assertEqual(p.compose, "2.29.1")
            self.assertEqual(p.projects, [("terrarium", "/home/x/dev/terrarium")])
            self.assertIsNotNone(p.free_kb)
            # A dir that does not exist yet: the free space of its parent.
            r = subprocess.run(["sh", "-c", dp.PROBE, "deploy", "new/deep"], capture_output=True, text=True, env=env, check=False)
            p = dp.parse_probe(r.stdout)
            self.assertEqual((p.dir, p.has), (os.path.join(tmp, "new/deep"), set()))
            self.assertIsNotNone(p.free_kb)


class Layout(unittest.TestCase):
    """compose.yaml, `docker::volumes-init`, and DATA_DIRS name the same dirs."""

    def test_compose_binds_only_data_dirs(self):
        text = (dp.REPO / "compose.yaml").read_text()
        binds = re.findall(r"^\s*- (\S+?):/", text, re.MULTILINE)
        sources = [b for b in binds if not b.startswith(("seccomp", "apparmor", "systempaths", '"'))]
        self.assertTrue(sources)
        for s in sources:
            self.assertTrue(s.startswith("${TERRARIUM_DATA_DIR:-.}/"), s)
        self.assertEqual({s.split("}/", 1)[1] for s in sources}, set(dp.DATA_DIRS))

    def test_volumes_init_makes_the_same_dirs(self):
        text = (dp.REPO / ".just/docker/mod.just").read_text()
        m = re.search(r"^volumes-init:\n\s*mkdir -p (.*)$", text, re.MULTILINE)
        assert m
        self.assertEqual(set(m.group(1).split()), set(dp.DATA_DIRS))


class Workflow(unittest.TestCase):
    """The CI workflow after the registry image job went away."""

    text = (dp.REPO / ".github/workflows/ci.yml").read_text()

    def test_no_registry(self):
        self.assertNotIn("ghcr.io", self.text)
        self.assertNotIn("packages: write", self.text)
        self.assertNotIn("docker/login-action", self.text)
        self.assertNotRegex(self.text, r"(?m)^  image:")

    def test_release_needs(self):
        self.assertRegex(self.text, r"(?m)^    needs: \[version, host\]$")
        self.assertIn("files: dist/**/*", self.text)
        self.assertIn("generate_release_notes: true", self.text)

    def test_yaml_parses(self):
        try:
            import yaml  # type: ignore
        except ImportError:
            self.skipTest("PyYAML is not installed (uv run --with pyyaml ...)")
        doc = yaml.safe_load(self.text)
        jobs = doc["jobs"]
        self.assertEqual(set(jobs), {"check", "version", "host", "release"})
        for name, job in jobs.items():
            for need in job.get("needs", []):
                self.assertIn(need, jobs, f"{name} needs {need}")
        self.assertEqual(jobs["release"]["needs"], ["version", "host"])
        self.assertEqual(jobs["release"]["permissions"], {"contents": "write"})


if __name__ == "__main__":
    unittest.main()

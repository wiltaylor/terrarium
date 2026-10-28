#!/usr/bin/env python3
"""Install the Terrarium host agent on a machine over ssh.

`just host::install <user@host> [name]` runs this after `host::build`.
The steps are in docs/host-protocol.md ("Install"). In short:

1. Copy target/host/terrarium-host and a generated systemd unit to the host.
2. Make the service user and /etc/terrarium-host (owned by that user,
   mode 0700, so that the agent can swap its enrol token in place).
3. Write /etc/terrarium-host/config.toml when it does not exist.
4. Ask the daemon for a one-time enrol token and write it to the token
   file (mode 0600). An existing token file is replaced only after a
   confirmation.
5. Enable and restart the unit, then run `terrarium-host check --probe`.

`--dry-run` prints every command and the generated files, and runs
nothing: no ssh, no scp, no daemon call. Standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parent.parent

CONFIG_DIR = "/etc/terrarium-host"
CONFIG_PATH = f"{CONFIG_DIR}/config.toml"
TOKEN_PATH = f"{CONFIG_DIR}/token"
BIN_PATH = "/usr/local/bin/terrarium-host"
UNIT_PATH = "/etc/systemd/system/terrarium-host.service"
UNIT_NAME = "terrarium-host"
HOST_PORT = 8082

# Upload names in the remote user's home directory. `install` moves them.
STAGE_BIN = ".terrarium-host.bin.new"
STAGE_UNIT = ".terrarium-host.service.new"
STAGE_CONFIG = ".terrarium-host.config.toml.new"

NAME_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,62}$")


class InstallError(Exception):
    """A problem that stops the install, with a message for the operator."""


@dataclass
class Settings:
    target: str
    name: str
    daemon_url: str
    fingerprint: str | None
    service_user: str
    binary: Path
    unit_template: Path
    terrarium: list[str]
    ssh_opts: list[str] = field(default_factory=list)
    yes: bool = False
    enable_display: bool = False

    @property
    def sudo(self) -> list[str]:
        """`sudo -n` unless the ssh user is root. `-n` fails at once in place
        of a password prompt that ssh cannot show."""
        user = self.target.split("@", 1)[0] if "@" in self.target else ""
        return [] if user == "root" else ["sudo", "-n"]


@dataclass
class RemoteState:
    """What already exists on the host."""

    config: bool = False
    token: bool = False
    unit: bool = False
    user: bool = False


# ---------------------------------------------------------------------------
# Inputs


def default_name(target: str) -> str:
    """`wil@lab1.lan` -> `lab1`: the first label of the host, lower case."""
    host = target.rsplit("@", 1)[-1]
    host = host.strip("[]").split(".", 1)[0].lower()
    return re.sub(r"[^a-z0-9_-]", "-", host)


def check_name(name: str) -> str:
    if not NAME_RE.match(name):
        raise InstallError(
            f"bad host name `{name}`: use 1-63 of a-z, 0-9, `-`, `_`, "
            "starting with a letter or digit"
        )
    return name


def resolve_daemon_url(arg: str | None, env: dict[str, str]) -> str:
    """The `daemon_url` for the host agent.

    The order is `--daemon-url`, then TERRARIUM_HOST_DAEMON_URL, then the
    host of TERRARIUM_PUBLIC_URL. A bare host name becomes
    `wss://<name>:8082`.
    """
    raw = arg or env.get("TERRARIUM_HOST_DAEMON_URL") or ""
    if not raw:
        public = env.get("TERRARIUM_PUBLIC_URL", "")
        host = urlsplit(public).hostname if public else None
        if not host or host in ("localhost", "127.0.0.1", "::1"):
            raise InstallError(
                "no daemon URL: pass --daemon-url or set TERRARIUM_HOST_DAEMON_URL "
                "(for example terrarium.lan or wss://terrarium.lan:8082); "
                f"TERRARIUM_PUBLIC_URL ({public or 'unset'}) is not a host that "
                "a remote machine can dial"
            )
        raw = host
    if "://" not in raw:
        raw = f"wss://{raw}"
    u = urlsplit(raw)
    if u.scheme not in ("ws", "wss", "http", "https") or not u.hostname:
        raise InstallError(f"bad daemon URL `{raw}`: use wss://host:port")
    if u.port is None:
        host = f"[{u.hostname}]" if ":" in u.hostname else u.hostname
        raw = f"{u.scheme}://{host}:{HOST_PORT}{u.path}"
    return raw


def is_tls(url: str) -> bool:
    return urlsplit(url).scheme in ("wss", "https")


def resolve_fingerprint(
    arg: str | None,
    cert: Path,
    no_pin: bool,
    daemon_url: str,
    dry_run: bool,
) -> str | None:
    """The `server_fingerprint_sha256` pin, or None (system roots).

    The order is `--fingerprint`, then `<cert>.fingerprint` (for example
    config/tls/host-cert.fingerprint), then `terrarium-host fingerprint
    <cert>`. A `ws://` URL never gets a pin: the host agent refuses that.
    """
    if no_pin or not is_tls(daemon_url):
        return None
    if arg:
        return arg.strip()
    side = cert.with_suffix(".fingerprint")
    if side.is_file():
        text = side.read_text().strip()
        # Accept the bare value, or the line that `host::cert` prints.
        m = re.search(r'"([^"]+)"', text)
        return m.group(1) if m else text
    if not cert.is_file():
        return None
    cmd = ["cargo", "run", "-q", "-p", "terrarium-host", "--", "fingerprint", str(cert)]
    if dry_run:
        print(f"# local: {shlex.join(cmd)}")
        return f"<SHA-256 of {cert}>"
    out = subprocess.run(cmd, cwd=REPO, check=True, capture_output=True, text=True)
    return out.stdout.strip()


# ---------------------------------------------------------------------------
# Generated files


def toml_str(s: str) -> str:
    return json.dumps(s)  # A JSON string is a valid TOML basic string.


DISPLAY_RISK = (
    "an agent with an exclusive lease on a desktop target can see this machine's "
    "screen and use its mouse and keyboard as the service user"
)
DISPLAY_BY_HAND = (
    f"# --enable-display: the existing {CONFIG_PATH} does not change; add "
    "`[display]` with `enabled = true` to it by hand (the daemon cannot turn it on)"
)


def render_config(s: Settings) -> str:
    lines = [
        "# Written by the Terrarium host installer (docs/host-protocol.md).",
        f"daemon_url = {toml_str(s.daemon_url)}",
        f"token_file = {toml_str(TOKEN_PATH)}",
        f"name = {toml_str(s.name)}",
    ]
    if s.fingerprint:
        lines.append(f"server_fingerprint_sha256 = {toml_str(s.fingerprint)}")
    if s.enable_display:
        lines += ["", "[display]", "enabled = true"]
    return "\n".join(lines) + "\n"


def render_unit(template: str, user: str, home: str | None = None) -> str:
    """The unit from crates/terrarium-host/terrarium-host.service, with
    `User=` and `Group=` set to the service user. With `home`, `%h` in the
    `Environment=PATH=` line becomes it: in a system unit `%h` is root's
    home. The ssh install does the same on the host (`setup_cmd`), where it
    knows the home."""
    out = re.sub(r"(?m)^User=.*$", f"User={user}", template)
    out = re.sub(r"(?m)^Group=.*$", f"Group={user}", out)
    if home is not None:
        out = re.sub(
            r"(?m)^(Environment=PATH=.*)$",
            lambda m: m.group(1).replace("%h", home),
            out,
        )
    return out


# ---------------------------------------------------------------------------
# Commands. Each one is an argv list; the runner prints or runs it.


def ssh(s: Settings, remote: list[str]) -> list[str]:
    """`ssh <target> <command>`. ssh joins its arguments with spaces and the
    remote shell splits them again, so the command goes as one quoted string."""
    return ["ssh", *s.ssh_opts, s.target, shlex.join(remote)]


def probe_cmd(s: Settings) -> list[str]:
    script = "; ".join(
        [
            f"test -e {CONFIG_PATH} && echo config",
            f"test -e {TOKEN_PATH} && echo token",
            f"test -e {UNIT_PATH} && echo unit",
            f"id -u {shlex.quote(s.service_user)} >/dev/null 2>&1 && echo user",
            "true",
        ]
    )
    return ssh(s, [*s.sudo, "sh", "-c", script])


def parse_probe(out: str) -> RemoteState:
    words = set(out.split())
    return RemoteState(
        config="config" in words,
        token="token" in words,
        unit="unit" in words,
        user="user" in words,
    )


def scp_opts(ssh_opts: list[str]) -> list[str]:
    """ssh `-p PORT` is `-P PORT` for scp; other `-o`/`-i` options are the same."""
    out: list[str] = []
    it = iter(ssh_opts)
    for o in it:
        if o == "-p":
            out += ["-P", next(it, "")]
        else:
            out.append(o)
    return out


def upload_cmds(s: Settings, unit_file: Path, config_file: Path | None) -> list[list[str]]:
    files = [(s.binary, STAGE_BIN), (unit_file, STAGE_UNIT)]
    if config_file is not None:
        files.append((config_file, STAGE_CONFIG))
    return [
        ["scp", *scp_opts(s.ssh_opts), "-q", str(src), f"{s.target}:{dst}"]
        for src, dst in files
    ]


def setup_cmd(s: Settings, state: RemoteState, write_config: bool) -> list[str]:
    """The root part on the host: the user, the files, daemon-reload."""
    u = shlex.quote(s.service_user)
    steps = []
    if not state.user:
        steps.append(
            f"useradd --system --user-group --home-dir /var/lib/{UNIT_NAME} "
            f"--create-home --shell /usr/sbin/nologin {u}"
        )
    steps += [
        f"install -m 0755 -o root -g root {STAGE_BIN} {BIN_PATH}",
        f"install -m 0644 -o root -g root {STAGE_UNIT} {UNIT_PATH}",
        # `%h` in a system unit is root's home: write the service user's.
        f'h=$(getent passwd {u} | cut -d: -f6) && sed -i "/^Environment=PATH=/s|%h|$h|g" {UNIT_PATH}',
        # The agent writes the token through a temporary file and a rename,
        # so it must own the directory.
        f"install -d -m 0700 -o {u} -g {u} {CONFIG_DIR}",
    ]
    if write_config:
        steps.append(f"install -m 0640 -o root -g {u} {STAGE_CONFIG} {CONFIG_PATH}")
    steps += [
        f"rm -f {STAGE_BIN} {STAGE_UNIT} {STAGE_CONFIG}",
        "systemctl daemon-reload",
    ]
    return ssh(s, [*s.sudo, "sh", "-ec", " && ".join(steps)])


def host_ls_cmd(s: Settings) -> list[str]:
    return [*s.terrarium, "--json", "admin", "host", "ls"]


def token_cmd(s: Settings, registered: bool) -> list[str]:
    """A new host gets `new --enrol`; a known one gets `rotate --enrol`."""
    verb = "rotate" if registered else "new"
    return [*s.terrarium, "--json", "admin", "host", verb, s.name, "--enrol"]


def as_service_user(s: Settings) -> list[str]:
    if s.sudo:
        return [*s.sudo, "-u", s.service_user]
    return ["runuser", "-u", s.service_user, "--"]


def write_token_cmd(s: Settings) -> list[str]:
    """The token goes on stdin, never in an argument (ps would show it).
    The service user writes it, so the file is the agent's own, mode 0600."""
    return ssh(s, [*as_service_user(s), BIN_PATH, "token-file", TOKEN_PATH])


def start_cmd(s: Settings) -> list[str]:
    # `restart`, not `enable --now`: a running agent must pick up the new
    # binary and the new token.
    return ssh(
        s,
        [*s.sudo, "sh", "-ec", f"systemctl enable {UNIT_NAME} && systemctl restart {UNIT_NAME}"],
    )


def check_cmd(s: Settings) -> list[str]:
    return ssh(s, [*as_service_user(s), BIN_PATH, "--config", CONFIG_PATH, "check", "--probe"])


# ---------------------------------------------------------------------------
# Running


class Runner:
    def __init__(self, dry_run: bool):
        self.dry_run = dry_run

    def show(self, argv: list[str], stdin: str | None = None) -> None:
        line = shlex.join(argv)
        if stdin is not None:
            line = f"{line}  < {stdin}"
        print(f"+ {line}", flush=True)

    def run(
        self,
        argv: list[str],
        *,
        stdin: str | None = None,
        stdin_label: str | None = None,
        capture: bool = False,
        check: bool = True,
        dry_out: str = "",
    ) -> subprocess.CompletedProcess[str]:
        self.show(argv, stdin_label if stdin is not None else None)
        if self.dry_run:
            return subprocess.CompletedProcess(argv, 0, dry_out, "")
        r = subprocess.run(
            argv,
            cwd=REPO,
            input=stdin,
            text=True,
            capture_output=capture,
        )
        if check and r.returncode != 0:
            err = (r.stderr or "").strip() if capture else ""
            raise InstallError(f"`{argv[0]}` failed (exit {r.returncode}){': ' + err if err else ''}")
        return r


def confirm(question: str, yes: bool, dry_run: bool) -> bool:
    if yes:
        return True
    if dry_run:
        print(f"# would ask: {question} [y/N]")
        return True
    if not sys.stdin.isatty():
        raise InstallError(f"{question} Pass --yes to confirm without a terminal.")
    return input(f"{question} [y/N] ").strip().lower() in ("y", "yes")


def install(s: Settings, runner: Runner) -> int:
    dry = runner.dry_run
    if not dry and not s.binary.is_file():
        raise InstallError(f"{s.binary} is missing: run `just host::build`")

    print(f"# install the host agent `{s.name}` on {s.target}")
    print(f"# daemon_url = {s.daemon_url}")
    if s.fingerprint:
        print(f"# pin        = {s.fingerprint}")
    elif is_tls(s.daemon_url):
        print("# pin        = none (the host checks the daemon against the system roots)")

    if s.enable_display:
        print(f"# warning: --enable-display: {DISPLAY_RISK}")

    # 1. What is on the host already. A dry run assumes a new host.
    out = runner.run(probe_cmd(s), capture=True).stdout
    state = parse_probe(out)
    if dry:
        print("# dry run: assume a new host (no config, no token, no unit, no user)")

    # 2. The files.
    write_config = not state.config
    if state.config:
        print(f"# keep the existing {CONFIG_PATH}")
        if s.enable_display:
            print(DISPLAY_BY_HAND)
    unit = render_unit(s.unit_template.read_text(), s.service_user)
    config = render_config(s)
    if dry:
        print(f"# generated {UNIT_PATH}:")
        print("\n".join(f"#   {l}" for l in unit.splitlines()))
        if write_config:
            print(f"# generated {CONFIG_PATH}:")
            print("\n".join(f"#   {l}" for l in config.splitlines()))
    with tempfile.TemporaryDirectory(prefix="terrarium-host-install.") as tmp:
        unit_file = Path(tmp) / "terrarium-host.service"
        unit_file.write_text(unit)
        config_file = None
        if write_config:
            config_file = Path(tmp) / "config.toml"
            config_file.write_text(config)
        for cmd in upload_cmds(s, unit_file, config_file):
            runner.run(cmd)
    runner.run(setup_cmd(s, state, write_config))

    # 3. The enrol token. An existing token file is the host's identity:
    # replace it only on a yes.
    new_token = True
    if state.token:
        new_token = confirm(
            f"{TOKEN_PATH} exists on {s.target}. Replace it with a new enrol token "
            f"(the old token of `{s.name}` stops working)?",
            s.yes,
            dry,
        )
    registered = False
    if new_token:
        hosts = runner.run(host_ls_cmd(s), capture=True, dry_out="[]").stdout
        registered = any(h.get("name") == s.name for h in json.loads(hosts or "[]"))
        if registered and not state.token:
            new_token = confirm(
                f"The daemon knows a host agent `{s.name}` already. Rotate its token "
                "(an agent that uses the old token loses its next connect)?",
                s.yes,
                dry,
            )
    if new_token:
        answer = runner.run(
            token_cmd(s, registered),
            capture=True,
            dry_out=json.dumps({"token": "the_<enrol token>", "token_expires_at": "<now + 10 min>"}),
        ).stdout
        issued = json.loads(answer)
        token = issued["token"]
        print(f"# enrol token: one use, expires {issued.get('token_expires_at', '?')}")
        runner.run(write_token_cmd(s), stdin=token + "\n", stdin_label="<enrol token>")
    else:
        print(f"# keep the existing {TOKEN_PATH}")

    # 4. Start, and check from the host.
    runner.run(start_cmd(s))
    r = runner.run(check_cmd(s), check=False)
    if r.returncode != 0:
        print(
            f"check --probe failed on {s.target}: see `journalctl -u {UNIT_NAME}` there",
            file=sys.stderr,
        )
        return 1
    if new_token and not dry:
        wait_online(s, runner)
    print(f"# done: `terrarium admin host show {s.name}` shows the link")
    return 0


def wait_online(s: Settings, runner: Runner, secs: int = 20) -> None:
    """Wait a short time for the enrol swap; tell the operator either way."""
    cmd = [*s.terrarium, "--json", "admin", "host", "show", s.name]
    deadline = time.monotonic() + secs
    while time.monotonic() < deadline:
        r = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
        if r.returncode == 0 and json.loads(r.stdout or "{}").get("online"):
            print(f"# host agent `{s.name}` is online")
            return
        time.sleep(2)
    print(
        f"# host agent `{s.name}` is not online after {secs}s: "
        f"see `journalctl -u {UNIT_NAME}` on {s.target}"
    )


# ---------------------------------------------------------------------------


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="host-install.py",
        description="Install the Terrarium host agent on a machine over ssh.",
    )
    p.add_argument("target", help="the machine: host or user@host (as ssh takes it)")
    p.add_argument("name", nargs="?", default="", help="the host agent name (default: the first label of the host)")
    p.add_argument("--daemon-url", help="the daemon's host listener; default: TERRARIUM_HOST_DAEMON_URL, then the host of TERRARIUM_PUBLIC_URL, port 8082")
    p.add_argument("--fingerprint", help="the SHA-256 to pin; default: <cert>.fingerprint, then the SHA-256 of --cert")
    p.add_argument("--cert", default="config/tls/host-cert.pem", help="the daemon's host listener certificate (default: %(default)s)")
    p.add_argument("--no-pin", action="store_true", help="write no pin: the daemon has a certificate that the system roots trust")
    p.add_argument("--user", default="terrarium", dest="service_user", help="the service user on the host (default: %(default)s)")
    p.add_argument("--binary", default="target/host/terrarium-host", help="the static binary (default: %(default)s)")
    p.add_argument("--ssh-opt", action="append", default=[], help="an extra ssh option, for example --ssh-opt=-p --ssh-opt=2222 (repeat)")
    p.add_argument("--yes", "-y", action="store_true", help="replace an existing token file without a question")
    p.add_argument("--enable-display", action="store_true", help="a new config gets [display] enabled = true: desktop targets may use this machine's screen (default: off)")
    p.add_argument("--dry-run", action="store_true", help="print the commands and the generated files; run nothing")
    return p.parse_args(argv)


def settings_from(args: argparse.Namespace, env: dict[str, str]) -> Settings:
    name = check_name(args.name or default_name(args.target))
    url = resolve_daemon_url(args.daemon_url, env)
    cert = Path(args.cert)
    if not cert.is_absolute():
        cert = REPO / cert
    fp = resolve_fingerprint(args.fingerprint, cert, args.no_pin, url, args.dry_run)
    binary = Path(args.binary)
    terrarium = shlex.split(env.get("TERRARIUM_CLI", "")) or [
        "cargo", "run", "-q", "-p", "terrarium", "--",
    ]
    return Settings(
        target=args.target,
        name=name,
        daemon_url=url,
        fingerprint=fp,
        service_user=args.service_user,
        binary=binary if binary.is_absolute() else REPO / binary,
        unit_template=REPO / "crates/terrarium-host/terrarium-host.service",
        terrarium=terrarium,
        ssh_opts=args.ssh_opt,
        yes=args.yes,
        enable_display=args.enable_display,
    )


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        s = settings_from(args, dict(os.environ))
        return install(s, Runner(args.dry_run))
    except InstallError as e:
        print(f"host install: {e}", file=sys.stderr)
        return 2
    except subprocess.CalledProcessError as e:
        print(f"host install: {shlex.join(e.cmd)} failed: {(e.stderr or '').strip()}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

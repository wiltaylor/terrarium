#!/usr/bin/env python3
"""Deploy Terrarium to a machine and build the image there, from the source.

The repo is private, so there is no image in a registry. The deploy sends
the source over ssh (`git archive`), and the target builds the image with
`docker compose build`. The target needs Docker with the compose plugin,
and no access to the repo. `just deploy::...` runs this script:

    server <target>    first install, or a re-install
    update <target>    send a new source, build, swap, start
    rollback <target>  go back to the previous source and image
    status <target>    the deployed ref, compose ps, the health check

The target is `user@host` (as ssh takes it), or `local` for this machine
with no ssh (`just deploy::local`).

The layout on the target (`--dir`, default `~/terrarium`):

    <dir>/src/                the code of the running deploy, and .deployed-ref
    <dir>/src.prev/           the code of the deploy before it (for rollback)
    <dir>/src.new/            the code that a deploy builds (only during a deploy)
    <dir>/.env                made once, never changed again by a deploy
    <dir>/terrarium-compose   docker compose for this install (written at each deploy)
    <dir>/config/ state/ tools/ work/ repos/ projects/ agents/ targets/
    <dir>/subscriptions/ fleets/
                              the data: compose binds them through
                              TERRARIUM_DATA_DIR, so a deploy never touches them

A deploy builds the image from `src.new/` first. Only after a good build
does it swap `src.new/` to `src/` (and `src/` to `src.prev/`) and run
`up -d`. A failed build leaves `src/` and the running stack as they were.

`--dry-run` runs only the read-only checks on the target (the preflight)
and prints every other command. It changes nothing. Standard library only.
"""

from __future__ import annotations

import argparse
import os
import re
import secrets
import shlex
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

REPO = Path(__file__).resolve().parent.parent

LOCAL = "local"
DEFAULT_DIR = "~/terrarium"
DEFAULT_PORT = 8080
WRAPPER = "terrarium-compose"
REF_FILE = ".deployed-ref"
MIN_FREE_GB = 10
UP_TIMEOUT_S = 900  # compose.yaml: start_period 600 s for the first harness install
MIN_COMPOSE = (2, 20)  # healthcheck start_interval, `up --wait --wait-timeout`

# The bind-mount dirs of compose.yaml, under TERRARIUM_DATA_DIR
# (`just docker::volumes-init` makes the same dirs in a dev checkout).
DATA_DIRS = [
    "config/claude", "config/codex", "config/copilot", "config/pi",
    "config/terrarium", "config/tls", "state", "tools", "work", "repos",
    "projects", "agents", "targets", "subscriptions", "fleets",
]

# The .env keys that the deploy sets in a new .env. The image comes from
# src/.deployed-ref at each deploy, so the .env does not set it.
PORT_KEYS = [
    ("TERRARIUM_PORT", 0), ("TERRARIUM_ARTIFACT_PORT", 1),
    ("TERRARIUM_HOST_PORT", 2), ("TERRARIUM_INGEST_PORT", 3),
]
NOT_IN_ENV = {"TERRARIUM_IMAGE"}

# The host agent dial-in in the container (compose.yaml: `<published>:8082`).
HOST_CONTAINER_PORT = 8082
# The .env keys of host TLS. A new .env has none of them.
HOST_TLS_KEYS = ("TERRARIUM_HOST_TLS_CERT", "TERRARIUM_HOST_TLS_KEY")


class DeployError(Exception):
    """A problem that stops the deploy, with a message for the operator."""


# ---------------------------------------------------------------------------
# The target


# A POSIX user name, as `--sudo-user` takes it.
USER_NAME = re.compile(r"[a-z_][a-z0-9_-]{0,31}")

# With `--sudo-user`, sudo runs this before the command: the command starts
# in the home of that user, so "~" and a relative --dir resolve there.
SUDO_CD = 'cd ~ && exec "$@"'


@dataclass
class Target:
    """`user@host` over ssh, or `local` (this machine, no ssh). With a
    `sudo_user`, each command runs on the target as that user, through
    `sudo -n -H -u <user>`: the ssh user only logs in."""

    name: str
    ssh_opts: list[str] = field(default_factory=list)
    sudo_user: str | None = None

    @property
    def local(self) -> bool:
        return self.name == LOCAL

    @property
    def host(self) -> str:
        if self.local:
            return "localhost"
        return self.name.rsplit("@", 1)[-1]

    def argv(self, script: str, *args: str, errexit: bool = True) -> list[str]:
        """Run `script` with `sh`, with `args` as $1, $2, ... A value never goes
        into the script text, so it needs no quoting there. ssh joins its
        arguments and the remote shell splits them again, so the command goes
        to ssh as one quoted string. sudo gets the command as its arguments
        and passes stdin through; -n makes a password prompt fail."""
        cmd = ["sh", "-ec" if errexit else "-c", script, "deploy", *args]
        if self.local:
            return cmd
        if self.sudo_user:
            cmd = ["sudo", "-n", "-H", "-u", self.sudo_user, "--", "sh", "-c", SUDO_CD, "sh", *cmd]
        return ["ssh", *self.ssh_opts, self.name, shlex.join(cmd)]

    def shell_hint(self, cmd: str) -> str:
        """A command for the operator to type, run on the target."""
        if self.local:
            return cmd
        ssh = ["ssh", "-t", *self.ssh_opts, self.name]
        if not self.sudo_user:
            return shlex.join([*ssh, cmd])
        remote = shlex.join(["sudo", "-H", "-u", self.sudo_user, "sh", "-c", f"cd ~ && {cmd}"])
        if not set(remote) & set('"$`\\!'):
            return f'{shlex.join(ssh)} "{remote}"'  # easier to read than a second shlex quote
        return shlex.join([*ssh, remote])


# ---------------------------------------------------------------------------
# The scripts that run on the target. Each one takes its values as $1, ...

PROBE = r"""raw=$1
case $raw in
	"~") d=$HOME ;;
	"~/"*) d=$HOME/${raw#"~/"} ;;
	/*) d=$raw ;;
	*) d=$HOME/$raw ;;
esac
[ "$d" = / ] || d=${d%/}
echo "dir=$d"
echo "user=$(id -un)"
echo "uid=$(id -u)"
echo "gid=$(id -g)"
echo "groups=$(id -nG)"
echo "arch=$(uname -m)"
echo "docker=$(command -v docker)"
if command -v docker >/dev/null 2>&1; then
	if docker info >/dev/null 2>&1; then echo docker_access=yes; else echo docker_access=no; fi
	echo "compose=$(docker compose version --short 2>/dev/null)"
	docker ps -a --filter label=com.docker.compose.project --format 'project={{.Label "com.docker.compose.project"}} {{.Label "com.docker.compose.project.working_dir"}}' 2>/dev/null | sort -u
fi
echo "git=$(command -v git)"
echo "curl=$(command -v curl)"
p=$d
while [ ! -d "$p" ]; do p=$(dirname "$p"); done
echo "free_kb=$(df -Pk "$p" | awk 'NR == 2 { print $4 }')"
for f in src src.new src.prev .env; do
	if [ -e "$d/$f" ]; then echo "has=$f"; fi
done
if [ -f "$d/.env" ]; then
	echo "env_keys=$(sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' "$d/.env" | tr '\n' ' ')"
	echo "env_port=$(sed -n 's/^TERRARIUM_PORT=//p' "$d/.env" | tail -n 1)"
	echo "env_public_url=$(sed -n 's/^TERRARIUM_PUBLIC_URL=//p' "$d/.env" | tail -n 1)"
fi
for f in src src.prev; do
	if [ -f "$d/$f/.deployed-ref" ]; then sed "s/^/$f./" "$d/$f/.deployed-ref"; fi
done
true"""

# $1 the dir, $2 the wrapper name; stdin: the wrapper.
LAYOUT = (
    'd=$1\nmkdir -p "$d"\ncd "$d"\nmkdir -p '
    + " ".join(DATA_DIRS)
    + '\ncat > "$2.tmp"\nchmod 755 "$2.tmp"\nmv -f "$2.tmp" "$2"'
)

# $1 the dir, $2... the lines of .deployed-ref; stdin: the tar of the source.
RECEIVE = """d=$1
shift
rm -rf "$d/src.new"
mkdir -p "$d/src.new"
tar -x -C "$d/src.new"
printf '%s\\n' "$@" > "$d/src.new/.deployed-ref\""""

# $1 the dir; stdin: the new .env. Never replaces an existing .env.
ENV_WRITE = """cd "$1"
if [ -e .env ]; then echo ".env exists: the deploy keeps it" >&2; exit 3; fi
umask 077
cat > .env.new
mv .env.new .env"""

# $1 the dir. Builds the image of src.new: src/ and the stack do not change.
BUILD = 'TERRARIUM_SRC="$1/src.new" "$1/terrarium-compose" build'

# $1 the dir. Only after a good build.
SWAP = """cd "$1"
test -d src.new
rm -rf src.prev
if [ -d src ]; then mv src src.prev; fi
mv src.new src"""

# $1 the dir. src and src.prev change places: a second rollback goes forward.
ROLLBACK_SWAP = """cd "$1"
test -d src.prev
test -d src
rm -rf src.swap
mv src src.swap
mv src.prev src
mv src.swap src.prev"""

# $1 the dir.
UP = f'"$1/terrarium-compose" up -d --wait --wait-timeout {UP_TIMEOUT_S}'

# $1 the dir, $2 the published dashboard port.
HEALTH = """"$1/terrarium-compose" exec -T terrarium terrarium daemon status
if command -v curl >/dev/null 2>&1; then
	curl -fsS -o /dev/null --max-time 10 "http://127.0.0.1:$2/"
	echo "dashboard: HTTP ok on port $2"
else
	echo "dashboard: no curl on the target, so no HTTP check of port $2"
fi"""

# $1 the dir, $2 the published dashboard port. Read-only.
STATUS = """cd "$1"
for f in src src.prev; do
	echo "== $f/.deployed-ref"
	cat "$f/.deployed-ref" 2>/dev/null || echo "(none)"
done
echo "== compose ps"
./terrarium-compose ps
echo "== health"
./terrarium-compose exec -T terrarium terrarium daemon status || echo "health: the daemon does not answer"
if command -v curl >/dev/null 2>&1; then
	if curl -fsS -o /dev/null --max-time 10 "http://127.0.0.1:$2/"; then echo "dashboard: HTTP ok on port $2"; else echo "dashboard: no answer on port $2"; fi
fi"""

# $1 the image repository, $2... the tags to keep. Removes the older deploy images.
PRUNE = """repo=$1
shift
docker image ls --format '{{.Tag}}' "$repo" | while read -r t; do
	keep=no
	for k in "$@"; do if [ "$t" = "$k" ]; then keep=yes; fi; done
	if [ "$keep" = no ]; then docker image rm "$repo:$t" >/dev/null 2>&1 || true; fi
done"""


def wrapper_script(project: str) -> str:
    """`<dir>/terrarium-compose`: docker compose for this install. It reads the
    image and the git ref from `$TERRARIUM_SRC/.deployed-ref` (default
    `<dir>/src`), sets TERRARIUM_DATA_DIR to its own dir, and drops every
    other TERRARIUM_* variable of the caller, so only `<dir>/.env` counts.
    The ref goes to the image build as TERRARIUM_GIT_REF (compose.yaml): the
    `git archive` source has no .git, so the binaries get their version
    (`terrarium version`) from it."""
    return f"""#!/bin/sh
# Written by `just deploy` (scripts/deploy.py) at each deploy: do not edit.
# docker compose for the Terrarium install in this dir: the code in src/
# (or $TERRARIUM_SRC), the data and .env here, and the image that
# src/.deployed-ref names. For example: ./terrarium-compose ps
set -e
d=$(cd "$(dirname "$0")" && pwd)
src=${{TERRARIUM_SRC:-$d/src}}
image=$(sed -n 's/^image=//p' "$src/.deployed-ref" 2>/dev/null || true)
ref=$(sed -n 's/^ref=//p' "$src/.deployed-ref" 2>/dev/null || true)
for v in $(env | sed -n 's/^\\(TERRARIUM_[A-Za-z0-9_]*\\)=.*/\\1/p'); do unset "$v"; done
export TERRARIUM_DATA_DIR="$d"
export TERRARIUM_IMAGE="${{image:-{project}:latest}}"
export TERRARIUM_GIT_REF="$ref"
cd "$src"
exec docker compose --project-name {project} --project-directory "$src" --file "$src/compose.yaml" --env-file "$d/.env" "$@"
"""


# ---------------------------------------------------------------------------
# The probe and the preflight


@dataclass
class Probe:
    dir: str = ""
    user: str = ""
    uid: str = ""
    gid: str = ""
    groups: list[str] = field(default_factory=list)
    arch: str = ""
    docker: str = ""
    docker_access: bool = False
    compose: str = ""
    git: str = ""
    curl: str = ""
    free_kb: int | None = None
    has: set[str] = field(default_factory=set)
    env_keys: set[str] = field(default_factory=set)
    env_port: str = ""
    env_public_url: str = ""
    refs: dict[str, str] = field(default_factory=dict)
    projects: list[tuple[str, str]] = field(default_factory=list)


def parse_probe(out: str) -> Probe:
    p = Probe()
    for line in out.splitlines():
        key, sep, value = line.partition("=")
        if not sep:
            continue
        if key == "has":
            p.has.add(value)
        elif key == "project":
            name, _, wd = value.partition(" ")
            p.projects.append((name, wd))
        elif key == "groups":
            p.groups = value.split()
        elif key == "env_keys":
            p.env_keys = set(value.split())
        elif key == "docker_access":
            p.docker_access = value == "yes"
        elif key == "free_kb":
            p.free_kb = int(value) if value.isdigit() else None
        elif key.startswith(("src.", "src.prev.")):
            p.refs[key] = value
        elif key in ("env_port", "env_public_url"):
            setattr(p, key, value.strip().strip("\"'"))
        elif hasattr(p, key):
            setattr(p, key, value)
    return p


@dataclass
class Check:
    level: str  # ok, info, warn, error
    text: str


def version_tuple(v: str) -> tuple[int, ...]:
    return tuple(int(x) for x in re.findall(r"\d+", v)[:3])


def preflight(p: Probe) -> list[Check]:
    """What the target needs. Each problem has the exact command to fix it.
    The preflight changes nothing."""
    out: list[Check] = []
    if p.uid == "0":
        out.append(Check("error", "do not deploy as root: the image makes its `terrarium` user "
                         "with the UID of the deploy user. Use a user in the group `docker`."))
    if not p.docker:
        out.append(Check("error", "docker: not found. Install Docker Engine: "
                         "https://docs.docker.com/engine/install/ "
                         "(for example `curl -fsSL https://get.docker.com | sudo sh`)"))
    else:
        if not p.docker_access:
            if "docker" not in p.groups:
                out.append(Check("error", f"docker: the user `{p.user}` is not in the group `docker`. "
                                 f"Run `sudo usermod -aG docker {p.user}`, then open a new login (a new ssh session)"))
            else:
                out.append(Check("error", "docker: `docker info` failed. Start the Docker daemon: "
                                 "`sudo systemctl enable --now docker`"))
        else:
            out.append(Check("ok", f"docker: {p.docker}"))
        if not p.compose:
            out.append(Check("error", "docker compose: the compose plugin is not installed. Install it: "
                             "`sudo apt-get install docker-compose-plugin` (Debian, Ubuntu) or "
                             "`sudo dnf install docker-compose-plugin` (Fedora); see "
                             "https://docs.docker.com/compose/install/linux/"))
        elif version_tuple(p.compose) < MIN_COMPOSE:
            out.append(Check("error", f"docker compose {p.compose}: use {'.'.join(map(str, MIN_COMPOSE))} or later "
                             "(`up --wait --wait-timeout`, the healthcheck start_interval). Update docker-compose-plugin"))
        else:
            out.append(Check("ok", f"docker compose: {p.compose}"))
    if p.git:
        out.append(Check("ok", f"git: {p.git}"))
    else:
        out.append(Check("info", "git: not found. It is optional: the deploy sends the source as a tar. "
                         "To install it: `sudo apt-get install git`"))
    if p.free_kb is None:
        out.append(Check("warn", "disk: the free space is not known"))
    else:
        gb = p.free_kb / (1024 * 1024)
        if gb < MIN_FREE_GB:
            out.append(Check("warn", f"disk: {gb:.1f} GB free at {p.dir}. The image build, the harnesses, "
                             f"and the agent workspaces need more: {MIN_FREE_GB} GB or more is better"))
        else:
            out.append(Check("ok", f"disk: {gb:.0f} GB free"))
    arch = p.arch
    if arch in ("x86_64", "amd64"):
        out.append(Check("ok", f"arch: {arch}"))
    elif arch in ("aarch64", "arm64"):
        out.append(Check("warn", f"arch: {arch}. The image build can work, but nobody has tested it. "
                         "The supported arch is x86_64"))
    else:
        out.append(Check("error", f"arch: {arch or 'unknown'} is not supported: use x86_64"))
    if not p.curl:
        out.append(Check("info", "curl: not found, so no HTTP check of the dashboard port "
                         "(the compose health check still runs)"))
    return out


def project_name(d: str) -> str:
    """The compose project of an install: from the name of its dir, never the
    name of a dev checkout (`terrarium`), so the two stacks stay apart."""
    base = re.sub(r"[^a-z0-9_-]", "-", PurePosixPath(d).name.lower()).strip("-_") or "root"
    if base == "terrarium":
        return "terrarium-deploy"
    return base if base.startswith("terrarium-") else f"terrarium-{base}"


def check_project(p: Probe, project: str, d: str) -> None:
    """Refuse a compose project of the same name in another dir."""
    ours = {f"{d}/src", f"{d}/src.new", f"{d}/src.prev"}
    for name, wd in p.projects:
        if name == project and wd not in ours:
            raise DeployError(
                f"the compose project `{project}` belongs to {wd}, not to {d}. "
                "Pick a --dir with another name"
            )


def refuse_dev_dir(d: str, repo: Path) -> None:
    """`deploy::local` must not touch the dev stack of this checkout."""
    real = os.path.realpath(d)
    root = os.path.realpath(repo)
    if real == root or real.startswith(root + os.sep):
        raise DeployError(
            f"--dir {d} is in this repo ({root}): the dev stack of `just up` lives here. "
            "Pick a dir outside the repo, for example --dir /tmp/tt-deploy"
        )


# ---------------------------------------------------------------------------
# The source


GitFn = Callable[..., str]


def git(*args: str) -> str:
    r = subprocess.run(["git", "-C", str(REPO), *args], capture_output=True, text=True, check=False)
    if r.returncode != 0:
        raise DeployError(f"`git {' '.join(args)}` failed: {r.stderr.strip()}")
    return r.stdout


@dataclass
class Source:
    ref: str  # as the operator gave it
    commit: str
    dirty: bool = False
    files: list[str] | None = None  # dirty: the tracked files of the working tree
    stamp: str = ""  # dirty: the time, so each dirty deploy has its own image

    @property
    def tag(self) -> str:
        return self.commit[:12] + (f"-dirty-{self.stamp}" if self.dirty else "")

    def producer(self, repo: Path) -> tuple[list[str], bytes | None]:
        """The command that writes the source as a tar to stdout, and its stdin."""
        if self.files is None:
            return ["git", "-C", str(repo), "archive", "--format=tar", self.commit], None
        names = b"".join(f.encode() + b"\0" for f in self.files)
        return ["tar", "-C", str(repo), "--null", "--no-recursion", "-T", "-", "-cf", "-"], names

    def ref_lines(self, image: str, now: datetime) -> list[str]:
        """The lines of `src/.deployed-ref`."""
        return [
            f"ref={self.commit}" + ("-dirty" if self.dirty else ""),
            f"source={self.ref}",
            f"image={image}",
            f"time={now.strftime('%Y-%m-%dT%H:%M:%SZ')}",
        ]


def resolve_source(ref: str | None, allow_dirty: bool, run_git: GitFn = git,
                   now: datetime | None = None, exists: Callable[[str], bool] | None = None) -> Source:
    """The ref and dirty rules.

    - The default ref is HEAD. A tree with changes to tracked files stops a
      HEAD deploy, unless `--allow-dirty`: then the deploy packs the tracked
      files of the working tree (untracked files never go).
    - `--ref <x>` packs that commit; the working tree does not count.
    - `--allow-dirty` does not go with a `--ref` other than HEAD.
    """
    given = ref or "HEAD"
    if allow_dirty and given != "HEAD":
        raise DeployError("--allow-dirty packs the working tree: it does not go with --ref")
    out = run_git("rev-parse", "--verify", "--quiet", f"{given}^{{commit}}").strip()
    if not out:
        raise DeployError(f"`{given}` is not a commit in this repo")
    commit = out
    if given != "HEAD":
        return Source(given, commit)
    changes = run_git("status", "--porcelain", "--untracked-files=no").strip()
    if not changes:
        return Source(given, commit)
    if not allow_dirty:
        raise DeployError(
            "the working tree has changes to tracked files. Commit them, "
            "pass --ref <commit> to deploy a commit, or pass --allow-dirty to "
            "send the working tree (tracked files only)"
        )
    exists = exists or (lambda f: os.path.lexists(REPO / f))
    files = [f for f in run_git("ls-files", "-z", "--cached").split("\0") if f and exists(f)]
    stamp = (now or datetime.now(timezone.utc)).strftime("%Y%m%d%H%M%S")
    return Source(given, commit, True, files, stamp)


def env_example(src: Source, run_git: GitFn = git) -> str:
    if src.dirty:
        return (REPO / ".env.example").read_text()
    return run_git("show", f"{src.commit}:.env.example")


# ---------------------------------------------------------------------------
# .env


KEY_RE = re.compile(r"^(#?)\s*([A-Za-z_][A-Za-z0-9_]*)=")


def render_env(example: str, values: dict[str, str], now: datetime) -> str:
    """A new .env: .env.example with `values` set. An active line wins over a
    commented one; a key that is in neither goes at the end. The image line
    becomes a comment: each deploy sets the image."""
    lines = example.splitlines()
    for key, value in values.items():
        active = [i for i, l in enumerate(lines) if (m := KEY_RE.match(l)) and not m[1] and m[2] == key]
        commented = [i for i, l in enumerate(lines) if (m := KEY_RE.match(l)) and m[1] and m[2] == key]
        idx = (active or commented or [None])[0]
        if idx is None:
            lines.append(f"{key}={value}")
        else:
            lines[idx] = f"{key}={value}"
    for i, l in enumerate(lines):
        m = KEY_RE.match(l)
        if m and not m[1] and m[2] in NOT_IN_ENV:
            lines[i] = f"#{l}  (the deploy sets the image: src/.deployed-ref)"
    head = [
        f"# Made by `just deploy` (scripts/deploy.py) at {now.strftime('%Y-%m-%dT%H:%M:%SZ')}.",
        "# A deploy never changes this file again: edit it by hand.",
    ]
    return "\n".join(head + lines) + "\n"


def env_values(token: str, uid: str, gid: str, public_url: str, port: int) -> dict[str, str]:
    v = {"TERRARIUM_TOKEN": token, "TERRARIUM_UID": uid, "TERRARIUM_GID": gid,
         "TERRARIUM_PUBLIC_URL": public_url}
    for key, offset in PORT_KEYS:
        v[key] = str(port + offset)
    v.update(host_link_values(port))
    return v


def host_link_values(port: int) -> dict[str, str]:
    """The host agent dial-in of a new .env, which has no host TLS.

    Without TLS, `TERRARIUM_HOST_LISTEN=auto` binds 127.0.0.1:8082 in the
    container, and the published port cannot reach a loopback bind. So the
    listener goes on all addresses in the container, and compose publishes
    it on loopback only: a host agent on the Docker host can connect, and
    the plaintext link stays off the network. The only URL that reaches it
    is `localhost` with the published port, so the installer gets it as
    `daemon_url` (the default is the host of the public URL, port 8082)."""
    host_port = port + dict(PORT_KEYS)["TERRARIUM_HOST_PORT"]
    return {
        "TERRARIUM_HOST_PORT": f"127.0.0.1:{host_port}",
        "TERRARIUM_HOST_LISTEN": f"0.0.0.0:{HOST_CONTAINER_PORT}",
        "TERRARIUM_HOST_DAEMON_URL": f"ws://localhost:{host_port}",
    }


def host_link_note(env_keys: set[str] | None, port: int) -> str | None:
    """A warning about the host agent dial-in. `env_keys`: the active keys of
    an existing .env, or None for a new .env (made by `host_link_values`).
    None: the .env has host TLS."""
    if env_keys is not None and all(k in env_keys for k in HOST_TLS_KEYS):
        return None
    tls = ("Remote host agents need TLS: run `just host::cert <name>`, then set "
           f"{' and '.join(HOST_TLS_KEYS)}, TERRARIUM_HOST_PORT, and "
           "TERRARIUM_HOST_DAEMON_URL in .env (docs/getting-started.md, section 2.7)")
    if env_keys is None:
        host_port = port + dict(PORT_KEYS)["TERRARIUM_HOST_PORT"]
        return (f"no host TLS: the host agent dial-in is plaintext on 127.0.0.1:{host_port}, "
                f"so only a host agent on this machine can connect. {tls}")
    if "TERRARIUM_HOST_LISTEN" not in env_keys:
        return ("no host TLS and no TERRARIUM_HOST_LISTEN in .env: the dial-in binds loopback "
                "in the container, so no host agent can connect. For a host agent on this "
                "machine, set TERRARIUM_HOST_LISTEN=0.0.0.0:8082, "
                "TERRARIUM_HOST_PORT=127.0.0.1:<port>, and "
                "TERRARIUM_HOST_DAEMON_URL=ws://localhost:<port> in .env. " + tls)
    return f"no host TLS. {tls}"


def example_keys(example: str) -> list[str]:
    """The active keys of .env.example, in order."""
    out = []
    for l in example.splitlines():
        m = KEY_RE.match(l)
        if m and not m[1] and m[2] not in out:
            out.append(m[2])
    return out


def missing_env_keys(example: str, have: set[str]) -> list[str]:
    return [k for k in example_keys(example) if k not in have and k not in NOT_IN_ENV]


def new_token() -> str:
    return secrets.token_hex(32)


def published_port(value: str) -> int | None:
    """`8080` or `127.0.0.1:8080` -> 8080."""
    tail = value.rsplit(":", 1)[-1].strip()
    return int(tail) if tail.isdigit() else None


# ---------------------------------------------------------------------------
# Running


class Runner:
    """Prints each command, then runs it. A dry run runs only the read-only
    commands. Tests replace `execute` and `execute_pipe`."""

    def __init__(self, dry_run: bool):
        self.dry_run = dry_run

    def show(self, line: str) -> None:
        print(f"+ {line}", flush=True)

    def run(self, argv: list[str], *, stdin: str | None = None, stdin_label: str | None = None,
            capture: bool = False, check: bool = True, readonly: bool = False) -> subprocess.CompletedProcess[str]:
        line = shlex.join(argv)
        if stdin is not None:
            line += f"  < {stdin_label or '<stdin>'}"
        self.show(line)
        if self.dry_run and not readonly:
            return subprocess.CompletedProcess(argv, 0, "", "")
        r = self.execute(argv, stdin, capture)
        if check and r.returncode != 0:
            err = (r.stderr or "").strip() if capture else ""
            raise DeployError(f"`{argv[0]}` failed (exit {r.returncode}){': ' + err if err else ''}")
        return r

    def pipe(self, producer: list[str], producer_stdin: bytes | None, consumer: list[str],
             label: str = "") -> None:
        line = f"{shlex.join(producer)} | {shlex.join(consumer)}"
        if producer_stdin is not None:
            line = f"{line}  < {label or '<stdin>'}"
        self.show(line)
        if self.dry_run:
            return
        code = self.execute_pipe(producer, producer_stdin, consumer)
        if code != 0:
            raise DeployError(f"sending the source failed (exit {code})")

    def execute(self, argv: list[str], stdin: str | None, capture: bool) -> subprocess.CompletedProcess[str]:
        return subprocess.run(argv, cwd=REPO, input=stdin, text=True, capture_output=capture, check=False)

    def execute_pipe(self, producer: list[str], producer_stdin: bytes | None, consumer: list[str]) -> int:
        prod = subprocess.Popen(producer, cwd=REPO, stdout=subprocess.PIPE,
                                stdin=subprocess.PIPE if producer_stdin is not None else None)
        assert prod.stdout is not None
        cons = subprocess.Popen(consumer, cwd=REPO, stdin=prod.stdout)
        prod.stdout.close()
        if producer_stdin is not None:
            assert prod.stdin is not None
            prod.stdin.write(producer_stdin)
            prod.stdin.close()
        c = cons.wait()
        p = prod.wait()
        return p or c


def confirm(question: str, yes: bool, dry_run: bool) -> bool:
    if yes:
        return True
    if dry_run:
        print(f"# would ask: {question} [y/N]")
        return True
    if not sys.stdin.isatty():
        raise DeployError(f"{question} Pass --yes to confirm without a terminal.")
    return input(f"{question} [y/N] ").strip().lower() in ("y", "yes")


# ---------------------------------------------------------------------------
# The flows


@dataclass
class Options:
    mode: str  # server, update, rollback, status
    target: Target
    dir: str = DEFAULT_DIR
    ref: str | None = None
    public_url: str | None = None
    port: int | None = None
    dry_run: bool = False
    yes: bool = False
    allow_dirty: bool = False


def recipe_hint(o: Options, recipe: str) -> str:
    """`just deploy::<recipe> <target>` with the --dir and --sudo-user of this run."""
    parts = ["just", f"deploy::{recipe}", o.target.name]
    if o.dir != DEFAULT_DIR:
        parts.append(f"--dir={o.dir}")
    if o.target.sudo_user:
        parts.append(f"--sudo-user={o.target.sudo_user}")
    return shlex.join(parts)


def run_probe(o: Options, runner: Runner) -> Probe:
    r = runner.run(o.target.argv(PROBE, o.dir, errexit=False), capture=True, check=False, readonly=True)
    if r.returncode != 0:
        err = (r.stderr or "").strip()
        raise DeployError(f"cannot reach {o.target.name}{': ' + err if err else ''}")
    p = parse_probe(r.stdout)
    if not p.dir.startswith("/"):
        raise DeployError(f"the probe on {o.target.name} gave no dir: {r.stdout.strip()[:200]}")
    if o.target.local:
        refuse_dev_dir(p.dir, REPO)
    return p


def print_preflight(checks: list[Check]) -> None:
    for c in checks:
        print(f"# preflight: {c.level:5} {c.text}")
    if any(c.level == "error" for c in checks):
        raise DeployError("the preflight found errors (see above). Nothing changed on the target")


def deploy(o: Options, runner: Runner, run_git: GitFn = git,
           token_fn: Callable[[], str] = new_token, now: datetime | None = None) -> int:
    """`server` and `update`."""
    now = now or datetime.now(timezone.utc)
    t = o.target
    src = resolve_source(o.ref, o.allow_dirty, run_git, now)
    p = run_probe(o, runner)
    d = p.dir
    project = project_name(d)
    image = f"{project}:{src.tag}"
    print(f"# deploy {src.ref} ({src.commit[:12]}{', dirty' if src.dirty else ''}) to {t.name}:{d}")
    print(f"# compose project {project}, image {image}")
    print_preflight(preflight(p))
    check_project(p, project, d)

    has_src = "src" in p.has
    has_env = ".env" in p.has
    if o.mode == "update" and not (has_src and has_env):
        raise DeployError(f"no install in {d} on {t.name}: run {recipe_hint(o, 'server')} first")
    if o.mode == "server" and has_src:
        current = p.refs.get("src.ref", "unknown")
        if not confirm(f"{d}/src exists (deployed ref {current}). Replace it?", o.yes, o.dry_run):
            print("# stopped: nothing changed")
            return 1

    # 1. The data dirs and the compose wrapper.
    runner.run(t.argv(LAYOUT, d, WRAPPER), stdin=wrapper_script(project), stdin_label=f"<{WRAPPER}>")

    # 2. The source, into src.new.
    producer, pstdin = src.producer(REPO)
    runner.pipe(producer, pstdin, t.argv(RECEIVE, d, *src.ref_lines(image, now)),
                label="<the tracked files of the working tree>")

    # 3. .env: made once, never changed.
    example = env_example(src, run_git)
    token = None
    if not has_env:
        port = o.port or DEFAULT_PORT
        public_url = o.public_url or f"http://{t.host}:{port}"
        if not o.public_url:
            print(f"# TERRARIUM_PUBLIC_URL={public_url}: pass --public-url with the name "
                  "that your browser and your host agents use")
        token = "<new token>" if o.dry_run else token_fn()
        content = render_env(example, env_values(token, p.uid, p.gid, public_url, port), now)
        host_note = host_link_note(None, port)
        runner.run(t.argv(ENV_WRITE, d), stdin=content,
                   stdin_label="<.env from .env.example, with a new TERRARIUM_TOKEN>")
        if o.dry_run:
            token = None
    else:
        port = published_port(p.env_port) or DEFAULT_PORT
        public_url = p.env_public_url or f"http://{t.host}:{port}"
        print(f"# keep the existing {d}/.env")
        missing = missing_env_keys(example, p.env_keys)
        if missing:
            print(f"# .env has no line for: {', '.join(missing)} (see src/.env.example; the defaults apply)")
        if o.port and o.port != port:
            print(f"# warning: --port {o.port} applies only to a new .env; the .env has TERRARIUM_PORT={p.env_port}")
        if o.public_url and o.public_url != p.env_public_url:
            print("# warning: --public-url applies only to a new .env; edit TERRARIUM_PUBLIC_URL there")
        host_note = host_link_note(p.env_keys, port)
    if host_note:
        print(f"# warning: {host_note}")

    # 4. Build from src.new. A failure leaves src/ and the stack as they were.
    r = runner.run(t.argv(BUILD, d), check=False)
    if r.returncode != 0:
        raise DeployError(
            f"the image build failed (exit {r.returncode}). {d}/src and the running stack "
            f"did not change; {d}/src.new stays for a look"
        )

    # 5. Swap and start. `up -d` makes the container again with the new image.
    runner.run(t.argv(SWAP, d))
    r = runner.run(t.argv(UP, d), check=False)
    if r.returncode != 0:
        raise DeployError(
            f"the stack did not become healthy (exit {r.returncode}). See "
            f"{t.shell_hint(f'{d}/{WRAPPER} logs')}; to go back: {recipe_hint(o, 'rollback')}"
        )
    health = runner.run(t.argv(HEALTH, d, str(port)), check=False)

    # 6. Keep the images of src and src.prev; remove older deploy images.
    keep = [src.tag]
    if prev := p.refs.get("src.image", "").partition(":")[2]:
        keep.append(prev)
    runner.run(t.argv(PRUNE, project, *keep), check=False)

    summary(o, d, public_url, token, project, health.returncode == 0, host_note)
    return 0


def summary(o: Options, d: str, public_url: str, token: str | None, project: str, healthy: bool,
            host_note: str | None = None) -> None:
    t = o.target
    compose = f"{d}/{WRAPPER}"
    if o.dry_run:
        print("# dry run: nothing changed")
        return
    print()
    print(f"Terrarium runs on {t.name} in {d} (compose project {project}).")
    if not healthy:
        print("warning: the health check failed; see " + t.shell_hint(f"{compose} logs"))
    print(f"  Dashboard:   {public_url}")
    if token:
        print(f"  TERRARIUM_TOKEN (shown once; it is in {d}/.env): {token}")
    if o.mode == "server":
        print(f"  Password:    {t.shell_hint(f'{compose} exec -it terrarium terrariumd password set')}")
        print("               then log in with the password (or select \"Use the token instead\")")
        print(f"  Host agent:  {t.shell_hint(f'{compose} exec terrarium terrarium admin host new <name> --enrol')}")
        print(f"               prints `curl -fsSL {public_url}/install/host.sh | sudo sh -s -- --token ... --name <name>`")
    if host_note:
        print(f"warning: {host_note}")
    print(f"  Compose:     {t.shell_hint(f'{compose} ps')} (logs -f, exec, ...)")
    print(f"  Update:      {recipe_hint(o, 'update')}")
    print(f"  Roll back:   {recipe_hint(o, 'rollback')}")
    print(f"  Status:      {recipe_hint(o, 'status')}")


def rollback(o: Options, runner: Runner) -> int:
    t = o.target
    p = run_probe(o, runner)
    d = p.dir
    if "src.prev" not in p.has or "src" not in p.has:
        raise DeployError(f"no {d}/src.prev on {t.name}: there is no deploy to go back to")
    check_project(p, project_name(d), d)
    cur = p.refs.get("src.ref", "unknown")
    prev = p.refs.get("src.prev.ref", "unknown")
    print(f"# roll back {t.name}:{d} from {cur} to {prev}")
    runner.run(t.argv(ROLLBACK_SWAP, d))
    r = runner.run(t.argv(UP, d), check=False)
    if r.returncode != 0:
        raise DeployError(f"the stack did not become healthy (exit {r.returncode}); "
                          f"see {t.shell_hint(f'{d}/{WRAPPER} logs')}")
    port = published_port(p.env_port) or DEFAULT_PORT
    runner.run(t.argv(HEALTH, d, str(port)), check=False)
    if o.dry_run:
        print("# dry run: nothing changed")
    else:
        print(f"# now at {prev}; {recipe_hint(o, 'rollback')} again goes back to {cur}")
    return 0


def status(o: Options, runner: Runner) -> int:
    t = o.target
    p = run_probe(o, runner)
    d = p.dir
    if "src" not in p.has:
        print(f"# no install in {d} on {t.name}")
        return 1
    port = published_port(p.env_port) or DEFAULT_PORT
    r = runner.run(t.argv(STATUS, d, str(port), errexit=False), check=False, readonly=True)
    return r.returncode


# ---------------------------------------------------------------------------


def parse_args(argv: list[str]) -> Options:
    ap = argparse.ArgumentParser(prog="deploy.py", description="Deploy Terrarium and build the image on the target.")
    sub = ap.add_subparsers(dest="mode", required=True)

    def common(sp: argparse.ArgumentParser) -> None:
        sp.add_argument("target", help="user@host (as ssh takes it), or `local` for this machine with no ssh")
        sp.add_argument("--dir", default=DEFAULT_DIR, help="the install dir on the target (default: %(default)s)")
        sp.add_argument("--ssh-option", "--ssh-opt", action="append", default=[], dest="ssh_option",
                        help="an extra ssh option, for example --ssh-option=-p --ssh-option=2222 (repeat)")
        sp.add_argument("--sudo-user", metavar="USER",
                        help="run each command on the target as USER, with `sudo -n -H -u USER`: "
                        "the ssh user needs sudo without a password; ~ and a relative --dir are in the home of USER")

    def source(sp: argparse.ArgumentParser) -> None:
        sp.add_argument("--ref", help="the commit to deploy (default: HEAD)")
        sp.add_argument("--allow-dirty", action="store_true",
                        help="deploy HEAD with the changes of the working tree (tracked files only)")
        sp.add_argument("--dry-run", action="store_true",
                        help="run only the read-only checks; print every other command")

    s = sub.add_parser("server", help="first install, or a re-install")
    common(s)
    source(s)
    s.add_argument("--public-url", help="TERRARIUM_PUBLIC_URL of a new .env (default: http://<host>:<port>)")
    s.add_argument("--port", type=int, help="the dashboard port of a new .env; the next three ports go to "
                   "artifacts, host agents, and webhooks (default: 8080)")
    s.add_argument("--yes", "-y", action="store_true", help="replace an existing src/ without a question")

    u = sub.add_parser("update", help="send a new source, build, swap, start")
    common(u)
    source(u)

    r = sub.add_parser("rollback", help="go back to src.prev")
    common(r)
    r.add_argument("--dry-run", action="store_true", help="print the commands; change nothing")

    st = sub.add_parser("status", help="the deployed ref, compose ps, the health check")
    common(st)

    a = ap.parse_args(argv)
    ssh_opts = [x for v in a.ssh_option for x in shlex.split(v)]
    if a.target == LOCAL and ssh_opts:
        ap.error("--ssh-option does not go with the target `local`")
    if a.sudo_user is not None:
        if a.target == LOCAL:
            ap.error("--sudo-user does not go with the target `local`")
        if not USER_NAME.fullmatch(a.sudo_user):
            ap.error(f"--sudo-user {a.sudo_user!r} is not a user name (lower case letters, digits, _ and -)")
    return Options(
        mode=a.mode,
        target=Target(a.target, ssh_opts, a.sudo_user),
        dir=a.dir,
        ref=getattr(a, "ref", None),
        public_url=getattr(a, "public_url", None),
        port=getattr(a, "port", None),
        dry_run=getattr(a, "dry_run", False),
        yes=getattr(a, "yes", False),
        allow_dirty=getattr(a, "allow_dirty", False),
    )


def main(argv: list[str] | None = None, runner: Runner | None = None, **kw) -> int:
    o = parse_args(sys.argv[1:] if argv is None else argv)
    runner = runner or Runner(o.dry_run)
    try:
        if o.mode in ("server", "update"):
            return deploy(o, runner, **kw)
        if o.mode == "rollback":
            return rollback(o, runner)
        return status(o, runner)
    except DeployError as e:
        print(f"deploy: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

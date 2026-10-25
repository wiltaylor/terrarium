#!/usr/bin/env python3
"""Check, update, and roll out the harness versions in harnesses.toml.

Stdlib only (Python 3.11+). The `harness` just module calls it:

    harness.py check [--json]
        For each harness (and extension): the pin, the tested range, the
        version in the running container, on the host, and the latest on npm
        with its publish date, and whether the latest is in the tested range.
        It changes nothing.

    harness.py update <name> [version] [--allow-major] [--dry-run]
        Bump the pin (default: the latest on npm). A patch or minor bump
        widens `tested` when the new version is outside it. A major bump
        needs --allow-major. Then: sync the Dockerfile ARG defaults, add a row
        to docs/compat.md "Version history", build the image
        (`just docker::image-build`), check the version in the image, and run
        the dry compatibility suite (`cargo test -p terrarium-compat`).
        --dry-run prints the plan and changes nothing.

    harness.py rollout [--dry-run]
        `just up`: compose recreates the container on the new image. The
        entrypoint stops the daemon and then each runner gracefully (each
        runner records `stopped_by: container`), and the new daemon launches
        each of those agents again with resume (docker/README.md, PLAN §18).

"Major" follows the semver rule for compatibility, as Cargo does: the first
non-zero part. So 2.1.x -> 2.2.0 is a minor bump, 2.x -> 3.0.0 is major, and
for a 0.x version 0.160.0 -> 0.161.0 is major (semver: anything may change in
0.x), 0.160.0 -> 0.160.1 is a patch.
"""

from __future__ import annotations

import argparse
import datetime as dt
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMPAT_MD = ROOT / "docs" / "compat.md"
SERVICE = "terrarium"


def _load_pins_module():
    spec = importlib.util.spec_from_file_location("harness_versions", Path(__file__).with_name("harness-versions.py"))
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


hv = _load_pins_module()


# ---------------------------------------------------------------- semver


@dataclass(frozen=True, order=True)
class Version:
    major: int
    minor: int
    patch: int
    pre: str = ""

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}" + (f"-{self.pre}" if self.pre else "")

    @property
    def core(self) -> tuple[int, int, int]:
        return (self.major, self.minor, self.patch)

    def compat_key(self) -> tuple[int, ...]:
        """The part that must stay the same for a compatible version."""
        if self.major:
            return (self.major,)
        if self.minor:
            return (0, self.minor)
        return (0, 0, self.patch)


VERSION_RE = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$")
FIND_RE = re.compile(r"(?<![\d.])(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)")


def parse_version(text: str) -> Version:
    m = VERSION_RE.match(text.strip())
    if not m:
        raise ValueError(f"`{text}` is not a semver version")
    return Version(int(m.group(1)), int(m.group(2)), int(m.group(3)), m.group(4) or "")


def find_version(output: str) -> str | None:
    """The first x.y.z in the output of `<binary> --version`."""
    m = FIND_RE.search(output)
    return m.group(1) if m else None


COMP_RE = re.compile(r"^(>=|<=|>|<|=|\^|~)?\s*v?(\d+)(?:\.(\d+))?(?:\.(\d+))?$")


def _comparators(req: str) -> list[tuple[str, tuple[int, int, int]]]:
    """`>=1.2.3, <1.3.0` -> [(">=", (1,2,3)), ("<", (1,3,0))]. Caret and tilde
    become two comparators, as in Rust's `semver::VersionReq`."""
    out = []
    for part in req.split(","):
        part = part.strip()
        m = COMP_RE.match(part)
        if not m:
            raise ValueError(f"`{part}` in `{req}` is not a comparator this script knows")
        op = m.group(1) or "^"
        major = int(m.group(2))
        minor = int(m.group(3) or 0)
        patch = int(m.group(4) or 0)
        lo = (major, minor, patch)
        if op == "^":
            if major:
                hi = (major + 1, 0, 0)
            elif minor or m.group(4) is None:
                hi = (0, minor + 1, 0)
            else:
                hi = (0, 0, patch + 1)
            out += [(">=", lo), ("<", hi)]
        elif op == "~":
            out += [(">=", lo), ("<", (major, minor + 1, 0))]
        else:
            out.append((op, lo))
    return out


def matches(version: Version, req: str) -> bool:
    """Does `version` satisfy `req`? A pre-release never matches."""
    if version.pre:
        return False
    v = version.core
    for op, b in _comparators(req):
        ok = {
            ">=": v >= b,
            ">": v > b,
            "<": v < b,
            "<=": v <= b,
            "=": v == b,
        }[op]
        if not ok:
            return False
    return True


def bump_kind(old: Version, new: Version) -> str:
    """`same`, `downgrade`, `patch`, `minor`, or `major` (see the module doc)."""
    if new == old:
        return "same"
    if new < old:
        return "downgrade"
    if new.compat_key() != old.compat_key():
        return "major"
    if new.minor != old.minor:
        return "minor"
    return "patch"


def upper_for(new: Version) -> str:
    """The upper bound that keeps a range on the minor line of `new`."""
    return f"<{new.major}.{new.minor + 1}.0"


class PlanError(Exception):
    pass


def next_range(tested: str, old: Version, new: Version, kind: str, allow_major: bool) -> str:
    """The `tested` range after a bump from `old` to `new`."""
    if kind == "major":
        if not allow_major:
            raise PlanError(
                f"{old} -> {new} is a major bump: hooks and flags may have changed. "
                "Read the release notes, then run again with --allow-major"
            )
        return f">={new}, {upper_for(new)}"
    if matches(new, tested):
        return tested
    if kind == "downgrade":
        raise PlanError(
            f"{new} is below the tested range `{tested}`. To roll back, edit harnesses.toml "
            "by hand (docs/getting-started.md, \"Updating a harness\")"
        )
    parts = [p.strip() for p in tested.split(",")]
    kept = [p for p in parts if not p.startswith("<")]
    if len(kept) == len(parts):
        raise PlanError(f"{new} is outside `{tested}`, which has no upper bound to widen")
    widened = ", ".join(kept + [upper_for(new)])
    if not matches(new, widened):
        raise PlanError(f"{new} is outside `{tested}`, and widening the upper bound gives `{widened}`")
    return widened


# ---------------------------------------------------------------- commands


def run(cmd: list[str], timeout: float = 60, **kw) -> subprocess.CompletedProcess | None:
    """Run a command; None when it is missing or times out."""
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)
    except (OSError, subprocess.TimeoutExpired):
        return None


def parse_npm_view(text: str) -> tuple[str | None, str | None]:
    """The latest version and its publish date from
    `npm view <pkg> version time --json`. npm 10 prints an object; npm 11 and
    later wrap it in a one-item array."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return None, None
    if isinstance(data, list):
        data = data[-1] if data else {}
    if not isinstance(data, dict):
        return None, None
    version = data.get("version")
    if isinstance(version, list):
        version = version[-1] if version else None
    times = data.get("time") or {}
    date = times.get(version) if isinstance(times, dict) and version else None
    return version, date


def npm_latest(package: str) -> tuple[str | None, str | None, str | None]:
    """(version, publish time, error)."""
    if not shutil.which("npm"):
        return None, None, "npm is not installed"
    r = run(["npm", "view", package, "version", "time", "--json"], timeout=60)
    if r is None or r.returncode != 0:
        return None, None, (r.stderr.strip().splitlines() or ["npm view failed"])[-1] if r else "npm view timed out"
    version, date = parse_npm_view(r.stdout)
    return version, date, None if version else "no version in the npm answer"


def parse_npm_ls(text: str, package: str) -> str | None:
    """The version of `package` from `npm ls -g <pkg> --json --depth=0`."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return None
    dep = (data.get("dependencies") or {}).get(package) if isinstance(data, dict) else None
    return dep.get("version") if isinstance(dep, dict) else None


def container_running() -> bool:
    if not shutil.which("docker"):
        return False
    r = run(["docker", "compose", "ps", "--status", "running", "-q", SERVICE], cwd=ROOT, timeout=30)
    return bool(r and r.returncode == 0 and r.stdout.strip())


def installed(entry: dict, where: str) -> str | None:
    """The installed version in the container (`where="container"`) or on the
    host. A harness answers `--version`; an extension is read from `npm ls -g`."""
    prefix = ["docker", "compose", "exec", "-T", "-u", SERVICE, SERVICE] if where == "container" else []
    if entry["kind"] == "extension":
        if where == "host" and not shutil.which("npm"):
            return None
        r = run(prefix + ["npm", "ls", "-g", entry["package"], "--json", "--depth=0"], cwd=ROOT)
        return parse_npm_ls(r.stdout, entry["package"]) if r else None
    if where == "host" and not shutil.which(entry["binary"]):
        return None
    r = run(prefix + [entry["binary"], "--version"], cwd=ROOT)
    if r is None or r.returncode != 0:
        return None
    return find_version(r.stdout)


def entries(cfg: dict) -> list[dict]:
    out = []
    for name, h in cfg["harness"].items():
        out.append({"name": name, "kind": "harness", **h})
    for name, e in cfg.get("extension", {}).items():
        out.append({"name": name, "kind": "extension", "tested": None, **e})
    return out


@dataclass
class Row:
    name: str
    kind: str
    package: str
    pinned: str
    tested: str | None
    container: str | None
    host: str | None
    latest: str | None
    latest_published: str | None
    latest_in_range: bool | None
    status: str
    notes: list[str] = field(default_factory=list)


def judge(entry: dict, container: str | None, host: str | None, latest: str | None, up: bool) -> tuple[bool | None, str, list[str]]:
    """(latest in range, status word, notes) for one row."""
    notes = []
    pinned = entry["version"]
    tested = entry.get("tested")
    in_range = None
    if latest and tested:
        try:
            in_range = matches(parse_version(latest), tested)
        except ValueError:
            in_range = None
    if up and container is None:
        notes.append("not found in the container")
    elif container and container != pinned:
        notes.append(f"the container has {container}, not the pin: run `just up`")
    if host and tested:
        try:
            if not matches(parse_version(host), tested):
                notes.append(f"the host has {host}, outside the range: `just ci::compat` will fail")
        except ValueError:
            pass
    if not latest:
        status = "unknown"
    else:
        try:
            newer = parse_version(latest) > parse_version(pinned)
        except ValueError:
            newer = latest != pinned
        if not newer:
            status = "current"
        elif tested is None or in_range:
            status = "update"
        else:
            status = "update (outside range)"
    return in_range, status, notes


def collect(cfg: dict) -> tuple[list[Row], bool]:
    items = entries(cfg)
    up = container_running()
    with ThreadPoolExecutor(max_workers=12) as pool:
        latest = [pool.submit(npm_latest, e["package"]) for e in items]
        cont = [pool.submit(installed, e, "container") if up else None for e in items]
        host = [pool.submit(installed, e, "host") for e in items]
        rows = []
        for i, e in enumerate(items):
            lv, ld, lerr = latest[i].result()
            cv = cont[i].result() if cont[i] else None
            hv_ = host[i].result()
            in_range, status, notes = judge(e, cv, hv_, lv, up)
            if lerr:
                notes.append(f"npm: {lerr}")
            rows.append(
                Row(e["name"], e["kind"], e["package"], e["version"], e.get("tested"), cv, hv_, lv, ld, in_range, status, notes)
            )
    return rows, up


def render_table(rows: list[Row], up: bool) -> str:
    def cell(v) -> str:
        if v is None:
            return "-"
        if v is True:
            return "yes"
        if v is False:
            return "NO"
        return str(v)

    headers = ["HARNESS", "PINNED", "TESTED", "CONTAINER", "HOST", "LATEST", "PUBLISHED", "LATEST IN RANGE", "STATUS"]
    body = [
        [
            r.name,
            r.pinned,
            r.tested,
            r.container if up else "(stack down)",
            r.host,
            r.latest,
            (r.latest_published or "")[:10] or None,
            r.latest_in_range,
            r.status,
        ]
        for r in rows
    ]
    table = [headers] + [[cell(c) for c in row] for row in body]
    widths = [max(len(row[i]) for row in table) for i in range(len(headers))]
    lines = ["  ".join(c.ljust(widths[i]) for i, c in enumerate(row)).rstrip() for row in table]
    for r in rows:
        for n in r.notes:
            lines.append(f"{r.name}: {n}")
    updates = [r.name for r in rows if r.status.startswith("update") and r.kind == "harness"]
    if updates:
        lines.append("")
        lines.append("Update one with `just harness::update <name>` (add --dry-run to see the plan).")
    return "\n".join(lines)


def cmd_check(args) -> int:
    cfg = hv.load()
    rows, up = collect(cfg)
    if args.json:
        print(json.dumps({"container_running": up, "harnesses": [asdict(r) for r in rows]}, indent=2))
    else:
        print(render_table(rows, up))
    return 0


# ---------------------------------------------------------------- update


@dataclass
class Plan:
    name: str
    kind: str  # harness | extension
    package: str
    old: str
    new: str
    bump: str
    tested_old: str | None
    tested_new: str | None
    warnings: list[str] = field(default_factory=list)


def make_plan(cfg: dict, name: str, target: str, allow_major: bool) -> Plan:
    if name in cfg["harness"]:
        kind, table = "harness", cfg["harness"][name]
    elif name in cfg.get("extension", {}):
        kind, table = "extension", cfg["extension"][name]
    else:
        known = list(cfg["harness"]) + list(cfg.get("extension", {}))
        raise PlanError(f"unknown harness `{name}`: use one of {', '.join(known)}")
    old = parse_version(table["version"])
    try:
        new = parse_version(target)
    except ValueError as e:
        raise PlanError(str(e)) from e
    if new.pre:
        raise PlanError(f"{new} is a pre-release; pin a release")
    bump = bump_kind(old, new)
    tested_old = table.get("tested")
    tested_new = tested_old
    warnings = []
    if bump == "major":
        if not allow_major:
            raise PlanError(
                f"{old} -> {new} is a major bump: hooks and flags may have changed. "
                "Read the release notes, then run again with --allow-major"
            )
        warnings.append(
            f"MAJOR bump {old} -> {new}: hooks, flags, and config keys may have changed. "
            f"Read the release notes, compare with docs/compat.md, and run `just ci::compat {name}` before rollout."
        )
    if tested_old and bump != "same":
        tested_new = next_range(tested_old, old, new, bump, allow_major)
    return Plan(name, kind, table["package"], str(old), str(new), bump, tested_old, tested_new, warnings)


def set_table_values(text: str, section: str, values: dict[str, str]) -> str:
    """Set `key = "value"` lines inside the `[section]` table, keeping the rest."""
    lines = text.splitlines(keepends=True)
    header = f"[{section}]"
    try:
        start = next(i for i, l in enumerate(lines) if l.strip() == header)
    except StopIteration:
        raise PlanError(f"harnesses.toml has no {header}") from None
    end = next((i for i in range(start + 1, len(lines)) if lines[i].lstrip().startswith("[")), len(lines))
    todo = dict(values)
    for i in range(start + 1, end):
        m = re.match(r'^(\s*)([A-Za-z_]+)(\s*=\s*)"[^"]*"(.*)$', lines[i].rstrip("\n"))
        if m and m.group(2) in todo:
            lines[i] = f'{m.group(1)}{m.group(2)}{m.group(3)}"{todo.pop(m.group(2))}"{m.group(4)}\n'
    if todo:
        raise PlanError(f"{header} has no {', '.join(todo)} line")
    return "".join(lines)


HISTORY_HEADING = "## Version history"


def compat_row(plan: Plan, date: str) -> str:
    rng = "unchanged" if plan.tested_new == plan.tested_old else f"`{plan.tested_old}` → `{plan.tested_new}`"
    if plan.tested_old is None:
        rng = "-"
    return f"| {date} | {plan.name} | {plan.old} → {plan.new} | {plan.bump} | {rng} | not run yet |"


def add_history_row(text: str, row: str) -> str:
    """Append `row` to the table under "## Version history"."""
    lines = text.splitlines(keepends=True)
    try:
        h = next(i for i, l in enumerate(lines) if l.strip() == HISTORY_HEADING)
    except StopIteration:
        raise PlanError(f'docs/compat.md has no "{HISTORY_HEADING}" section') from None
    i = h + 1
    while i < len(lines) and not lines[i].startswith("|"):
        if lines[i].startswith("#"):
            raise PlanError(f'docs/compat.md: "{HISTORY_HEADING}" has no table')
        i += 1
    while i < len(lines) and lines[i].startswith("|"):
        i += 1
    lines.insert(i, row + "\n")
    return "".join(lines)


def print_plan(plan: Plan, image: str) -> None:
    print(f"{plan.name} ({plan.package}): {plan.old} -> {plan.new} ({plan.bump})")
    if plan.tested_old is not None:
        if plan.tested_new == plan.tested_old:
            print(f"  tested range: `{plan.tested_old}` (unchanged; {plan.new} is inside it)")
        else:
            print(f"  tested range: `{plan.tested_old}` -> `{plan.tested_new}`")
    for w in plan.warnings:
        print(f"  WARNING: {w}")
    print("  steps:")
    print(f"    1. harnesses.toml: [{plan.kind}.{plan.name}] version = \"{plan.new}\"" + (f', tested = "{plan.tested_new}"' if plan.tested_new != plan.tested_old else ""))
    print("    2. docker/Dockerfile: the ARG defaults (scripts/harness-versions.py sync)")
    print(f'    3. docs/compat.md "Version history": {compat_row(plan, "<today>")}')
    print(f"    4. just docker::image-build  (image {image})")
    print("    5. check the version in the image")
    print("    6. cargo test -p terrarium-compat  (the dry compatibility suite)")


def next_steps(plan: Plan) -> str:
    harness = plan.name if plan.kind == "harness" else "copilot"
    return "\n".join(
        [
            "Next:",
            f"  1. Install {plan.package}@{plan.new} on the host: `just ci::compat` runs the host binary,",
            f"     on your real logins (npm install -g {plan.package}@{plan.new}).",
            f"  2. just ci::compat {harness}    the live proof (up to 3 model turns); then edit the",
            '     "Compat" cell of the new docs/compat.md "Version history" row.',
            "  3. just harness::rollout      (or `just up`): the container restarts on the new image,",
            "     and the daemon resumes the agents that were running.",
            "Roll back: set the old version in harnesses.toml (and `tested` if it changed), then `just up`.",
        ]
    )


def step(title: str, cmd: list[str]) -> bool:
    print(f"\n==> {title}: {' '.join(cmd)}", flush=True)
    try:
        r = subprocess.run(cmd, cwd=ROOT)
    except OSError as e:
        print(f"cannot run {cmd[0]}: {e}", file=sys.stderr)
        return False
    return r.returncode == 0


def image_version(image: str, plan: Plan, cfg: dict) -> str | None:
    if plan.kind == "extension":
        cmd = ["docker", "run", "--rm", "--entrypoint", "npm", image, "ls", "-g", plan.package, "--json", "--depth=0"]
        r = run(cmd, timeout=120)
        return parse_npm_ls(r.stdout, plan.package) if r else None
    binary = cfg["harness"][plan.name]["binary"]
    r = run(["docker", "run", "--rm", "--entrypoint", binary, image, "--version"], timeout=120)
    return find_version(r.stdout) if r and r.returncode == 0 else None


def cmd_update(args) -> int:
    cfg = hv.load()
    image = os.environ.get("TERRARIUM_IMAGE", "terrarium:dev")
    target = args.version
    table = cfg["harness"].get(args.name) or cfg.get("extension", {}).get(args.name)
    if table is None:
        known = list(cfg["harness"]) + list(cfg.get("extension", {}))
        print(f"harness: unknown harness `{args.name}`: use one of {', '.join(known)}", file=sys.stderr)
        return 2
    if target is None:
        latest, date, err = npm_latest(table["package"])
        if not latest:
            print(f"harness: cannot read the latest {table['package']} from npm: {err}", file=sys.stderr)
            return 1
        print(f"latest {table['package']} on npm: {latest}" + (f" (published {date[:10]})" if date else ""))
        target = latest
    try:
        plan = make_plan(cfg, args.name, target, args.allow_major)
    except PlanError as e:
        print(f"harness: {e}", file=sys.stderr)
        return 1
    if plan.bump == "same":
        print(f"{plan.name} is already pinned to {plan.new}; nothing to do.")
        return 0
    print_plan(plan, image)
    if args.dry_run:
        print("\ndry run: nothing changed.")
        return 0

    toml_path = hv.TOML
    values = {"version": plan.new}
    if plan.tested_new != plan.tested_old:
        values["tested"] = plan.tested_new
    toml_path.write_text(set_table_values(toml_path.read_text(), f"{plan.kind}.{plan.name}", values))
    cfg = hv.load()
    df = hv.DOCKERFILE
    df.write_text(hv.sync_dockerfile(cfg, df.read_text()))
    COMPAT_MD.write_text(add_history_row(COMPAT_MD.read_text(), compat_row(plan, dt.date.today().isoformat())))
    print("\nupdated harnesses.toml, docker/Dockerfile, docs/compat.md")

    rollback = "To undo: `git checkout harnesses.toml docker/Dockerfile docs/compat.md`."
    if args.no_build:
        print("--no-build: skipped the image build and the dry compatibility suite.")
    else:
        if not step("build the image", ["just", "docker::image-build"]):
            print(f"\nthe image build failed. {rollback}", file=sys.stderr)
            return 1
        got = image_version(image, plan, cfg)
        if got != plan.new:
            print(f"\nthe image {image} has {plan.name} {got}, not {plan.new}. {rollback}", file=sys.stderr)
            return 1
        print(f"the image {image} has {plan.name} {got}")
        if not step("dry compatibility suite", ["cargo", "test", "-p", "terrarium-compat"]):
            print(f"\nthe dry compatibility suite failed (target/compat-report-dry.md). {rollback}", file=sys.stderr)
            return 1
    print()
    print(next_steps(plan))
    return 0


# ---------------------------------------------------------------- rollout

LIVE = {"starting", "idle", "busy", "blocked", "restarting"}


def live_agents() -> list[dict] | None:
    r = run(["docker", "compose", "exec", "-T", "-u", SERVICE, SERVICE, "terrarium", "--json", "agent", "ls"], cwd=ROOT)
    if not r or r.returncode != 0:
        return None
    try:
        return [a for a in json.loads(r.stdout) if a.get("status") in LIVE]
    except (json.JSONDecodeError, AttributeError):
        return None


def cmd_rollout(args) -> int:
    up = container_running()
    agents = live_agents() if up else []
    names = ", ".join(f"{a.get('project')}/{a.get('name')} ({a.get('status')})" for a in agents or []) or "none"
    print(f"live agents: {names}")
    print(
        "`just up` builds the image and recreates the container when the image changed. The entrypoint\n"
        "stops terrariumd, then each runner (the harness gets its exit keys, then SIGHUP), and the\n"
        "runners record `stopped_by: container`. The new daemon launches those agents again with\n"
        "resume (docker/README.md, \"The shutdown sequence\")."
    )
    if args.dry_run:
        print("dry run: nothing changed.")
        return 0
    if not step("roll out", ["just", "up"]):
        return 1
    after = live_agents()
    if after is None:
        print("cannot list the agents after the rollout; check `just docker::logs`", file=sys.stderr)
        return 1
    print("live agents now: " + (", ".join(f"{a.get('project')}/{a.get('name')} ({a.get('status')})" for a in after) or "none"))
    gone = {a.get("id") for a in agents or []} - {a.get("id") for a in after}
    if gone:
        print(f"{len(gone)} agent(s) did not come back yet; see `terrarium agent ls` and the events", file=sys.stderr)
    return 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="command", required=True)
    c = sub.add_parser("check", help="compare the pins with the container, the host, and npm")
    c.add_argument("--json", action="store_true")
    u = sub.add_parser("update", help="bump one pin, build, and run the dry compat")
    u.add_argument("name")
    u.add_argument("version", nargs="?", help="default: the latest on npm")
    u.add_argument("--allow-major", action="store_true", help="allow a major bump (resets the tested range)")
    u.add_argument("--dry-run", action="store_true", help="print the plan; change nothing")
    u.add_argument("--no-build", action="store_true", help="change the files only; skip the image build and the dry compat")
    r = sub.add_parser("rollout", help="just up: restart on the new image; the daemon resumes the agents")
    r.add_argument("--dry-run", action="store_true")
    a = p.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    try:
        return {"check": cmd_check, "update": cmd_update, "rollout": cmd_rollout}[a.command](a)
    except (hv.PinError, PlanError) as e:
        print(f"harness: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

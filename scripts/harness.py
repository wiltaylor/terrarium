#!/usr/bin/env python3
"""Check and update the harnesses in the tools volume of the running stack.

Stdlib only (Python 3.11+). The `harness` just module calls it. The image
installs no harness: the container installs the pins of harnesses.toml into
/opt/terrarium/tools at the first start, and the daemon updates them
(`terrarium admin harness update`): each version goes into its own version
dir, and the daemon switches to it. No command here builds an image.

    harness.py check [--json]
        For each harness (and extension): the pin, the
        version in the container (the tools manifest), on the host, and the
        latest on npm with its publish date. STATUS compares the container
        version (the pin when the stack is down) with the latest: `latest`,
        `update available`, `update available (major)`, or `unknown`. It
        changes nothing.

    harness.py update <name> [version] [--allow-major] [--force] [--pin] [--dry-run]
        Update one harness in the container to `version` (default: the latest
        on npm), through the daemon (`npm install -g` into a version dir of
        the tools volume, then the switch). Any version is allowed: a major
        bump and a downgrade too. --allow-major is accepted for
        compatibility and has no effect. Running agents keep their version
        until their next restart; the daemon names them. --force installs
        the version again when its version dir is on disk.
        --pin also writes the version into harnesses.toml and a row into
        docs/compat.md "Version history", so a fresh install gets the same
        version. --dry-run prints the plan.

    harness.py latest [--dry-run] [--pin] [--force]
        Update every harness (and extension) with a newer npm release to that
        release, in one pass. A major bump is allowed.
        It ends with a table of the changes and the `just ci::compat <name>`
        runs that give the live proof.

"Major" (only a label in the output) follows the semver rule for
compatibility, as Cargo does: the first non-zero part. So 2.1.x -> 2.2.0 is a minor bump, 2.x -> 3.0.0 is major, and
for a 0.x version 0.160.0 -> 0.161.0 is major (semver: anything may change in
0.x), 0.160.0 -> 0.160.1 is a patch.
"""

from __future__ import annotations

import argparse
import datetime as dt
import importlib.util
import json
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
MANIFEST = "/opt/terrarium/tools/terrarium-manifest.json"


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


class PlanError(Exception):
    pass


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


EXEC = ["docker", "compose", "exec", "-T", "-u", SERVICE, SERVICE]
# The daemon caps one npm run at 10 minutes; `--all` runs one after another.
CLI_TIMEOUT = 60 * 60


def container_cli(args: list[str]) -> tuple[int, str, str]:
    """Run `terrarium --json <args>` in the container: (exit code, stdout,
    stderr). The code is 127 when docker is missing or the call timed out.
    Tests replace this function."""
    r = run(EXEC + ["terrarium", "--json"] + args, timeout=CLI_TIMEOUT, cwd=ROOT)
    if r is None:
        return 127, "", "cannot run `docker compose exec` (is docker installed, and the stack up?)"
    return r.returncode, r.stdout, r.stderr


def parse_manifest(text: str) -> dict[str, str]:
    """`{name: version}` from the tools manifest
    (`{"claude": {"package", "version", "installed_at"}, ...}`)."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(data, dict):
        return {}
    return {k: v["version"] for k, v in data.items() if isinstance(v, dict) and isinstance(v.get("version"), str)}


def container_manifest() -> dict[str, str]:
    """The versions in the container's tools manifest; empty when it cannot
    be read."""
    r = run(EXEC + ["cat", MANIFEST], cwd=ROOT)
    return parse_manifest(r.stdout) if r and r.returncode == 0 else {}


def installed(entry: dict, where: str) -> str | None:
    """The installed version in the container (`where="container"`) or on the
    host. A harness answers `--version`; an extension is read from `npm ls -g`.
    For the container, `check` reads the manifest first and uses this as the
    fallback."""
    prefix = EXEC if where == "container" else []
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


def container_versions(cfg: dict) -> dict[str, str]:
    """The version of each harness and extension in the container: the
    manifest, else `--version` (or `npm ls -g`)."""
    found = container_manifest()
    for e in entries(cfg):
        if e["name"] not in found:
            v = installed(e, "container")
            if v:
                found[e["name"]] = v
    return found


def entries(cfg: dict) -> list[dict]:
    out = []
    for name, h in cfg["harness"].items():
        out.append({"name": name, "kind": "harness", **h})
    for name, e in cfg.get("extension", {}).items():
        out.append({"name": name, "kind": "extension", **e})
    return out


@dataclass
class Row:
    name: str
    kind: str
    package: str
    pinned: str
    container: str | None
    host: str | None
    latest: str | None
    latest_published: str | None
    status: str
    notes: list[str] = field(default_factory=list)


def judge(entry: dict, container: str | None, latest: str | None, up: bool) -> tuple[str, list[str]]:
    """(status word, notes) for one row. The status compares the latest with
    the container version, else with the pin."""
    notes = []
    pinned = entry["version"]
    if up and container is None:
        notes.append("not found in the container: the entrypoint installs it at the next start (`just docker::restart`)")
    elif container and container != pinned:
        notes.append(
            f"the container has {container}, the pin is {pinned} "
            f"(`just harness::update {entry['name']} {pinned}`, or update with `--pin`)"
        )
    base = container or pinned
    if not latest:
        status = "unknown"
    else:
        try:
            newer = parse_version(latest) > parse_version(base)
        except ValueError:
            newer = latest != base
        if not newer:
            status = "latest"
        else:
            try:
                major = bump_kind(parse_version(base), parse_version(latest)) == "major"
            except ValueError:
                major = False
            status = "update available (major)" if major else "update available"
    return status, notes


def collect(cfg: dict) -> tuple[list[Row], bool]:
    items = entries(cfg)
    up = container_running()
    cont = container_versions(cfg) if up else {}
    with ThreadPoolExecutor(max_workers=12) as pool:
        latest = [pool.submit(npm_latest, e["package"]) for e in items]
        host = [pool.submit(installed, e, "host") for e in items]
        rows = []
        for i, e in enumerate(items):
            lv, ld, lerr = latest[i].result()
            cv = cont.get(e["name"])
            hv_ = host[i].result()
            status, notes = judge(e, cv, lv, up)
            if lerr:
                notes.append(f"npm: {lerr}")
            rows.append(
                Row(e["name"], e["kind"], e["package"], e["version"], cv, hv_, lv, ld, status, notes)
            )
    return rows, up


def render_table(rows: list[Row], up: bool) -> str:
    def cell(v) -> str:
        return "-" if v is None else str(v)

    headers = ["HARNESS", "PINNED", "CONTAINER", "HOST", "LATEST", "PUBLISHED", "STATUS"]
    body = [
        [
            r.name,
            r.pinned,
            r.container if up else "(stack down)",
            r.host,
            r.latest,
            (r.latest_published or "")[:10] or None,
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
    updates = [r.name for r in rows if r.status.startswith("update")]
    if updates:
        lines.append("")
        lines.append("`just harness::latest` updates every harness in place; `just harness::update <name>` updates one.")
        lines.append("Neither rebuilds the image. Add --dry-run to see the plan, --pin to also write harnesses.toml.")
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
    old: str  # the installed version in the container (else the pin)
    new: str
    bump: str  # from `old` to `new`
    pinned: str = ""  # the pin in harnesses.toml


def make_plan(cfg: dict, name: str, target: str, current: str | None = None) -> Plan:
    """The plan to move `name` from `current` (the installed version; default:
    the pin) to `target`. Any release is allowed: a major bump or a downgrade
    too."""
    if name in cfg["harness"]:
        kind, table = "harness", cfg["harness"][name]
    elif name in cfg.get("extension", {}):
        kind, table = "extension", cfg["extension"][name]
    else:
        known = list(cfg["harness"]) + list(cfg.get("extension", {}))
        raise PlanError(f"unknown harness `{name}`: use one of {', '.join(known)}")
    pinned = parse_version(table["version"])
    try:
        new = parse_version(target)
        old = parse_version(current) if current else pinned
    except ValueError as e:
        raise PlanError(str(e)) from e
    if new.pre:
        raise PlanError(f"{new} is a pre-release; pin a release")
    return Plan(name, kind, table["package"], str(old), str(new), bump_kind(old, new), str(pinned))


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
    """A "Version history" row: from the pin to the new version."""
    pinned = plan.pinned or plan.old
    try:
        kind = bump_kind(parse_version(pinned), parse_version(plan.new))
    except ValueError:
        kind = plan.bump
    return f"| {date} | {plan.name} | {pinned} → {plan.new} | {kind} | not run yet |"


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


def update_args(plan: Plan, force: bool) -> list[str]:
    """The CLI arguments that update one harness in the container."""
    return ["admin", "harness", "update", plan.name, "--version", plan.new] + (["--force"] if force else [])


def print_plan(plan: Plan, pin: bool, force: bool) -> None:
    print(f"{plan.name} ({plan.package}): {plan.old} -> {plan.new} ({plan.bump})")
    print("  steps:")
    print(f"    1. in the container: terrarium admin harness update {' '.join(update_args(plan, force)[3:])}")
    if pin:
        print(f"    2. harnesses.toml: [{plan.kind}.{plan.name}] version = \"{plan.new}\"")
        print(f'    3. docs/compat.md "Version history": {compat_row(plan, "<today>")}')


class UpdateFailed(Exception):
    pass


def apply(plan: Plan, force: bool) -> dict:
    """Update one harness in the container through the daemon. Prints npm's
    output. Raises UpdateFailed with the reason."""
    code, out, err = container_cli(update_args(plan, force))
    # The CLI prints the answer on stdout, and with --json an error body on
    # stderr.
    body = {}
    for text in (out, err):
        try:
            body = json.loads(text) if text.strip() else {}
        except json.JSONDecodeError:
            continue
        if body:
            break
    if code == 0 and isinstance(body, dict) and "to" in body:
        if body.get("output"):
            print(body["output"].rstrip())
        print(f"{plan.name}: {body.get('from') or '-'} -> {body['to']}")
        if body.get("note"):
            print(f"  {body['note']}")
        return body
    e = body.get("error") if isinstance(body, dict) else None
    if not isinstance(e, dict):
        raise UpdateFailed((err or out).strip() or f"the CLI exited with {code}")
    details = e.get("details") or {}
    if isinstance(details, dict) and details.get("output"):
        print(str(details["output"]).rstrip())
    msg = e.get("message") or e.get("code") or "the update failed"
    raise UpdateFailed(msg)


def write_files(plans: list[Plan]) -> None:
    """harnesses.toml and one docs/compat.md "Version history" row for each
    plan."""
    toml_path = hv.TOML
    text = toml_path.read_text()
    for plan in plans:
        text = set_table_values(text, f"{plan.kind}.{plan.name}", {"version": plan.new})
    toml_path.write_text(text)
    md = COMPAT_MD.read_text()
    today = dt.date.today().isoformat()
    for plan in plans:
        md = add_history_row(md, compat_row(plan, today))
    COMPAT_MD.write_text(md)
    print("\nupdated harnesses.toml and docs/compat.md. A fresh install gets the new version.")


def next_steps(plan: Plan) -> str:
    harness = plan.name if plan.kind == "harness" else "copilot"
    return "\n".join(
        [
            "Next (the live proof; not a gate):",
            f"  1. npm install -g {plan.package}@{plan.new} on the host: `just ci::compat` runs the host binary,",
            "     on your real logins.",
            f"  2. just ci::compat {harness}    (up to 3 model turns)",
            f"Roll back: just harness::update {plan.name} {plan.old}",
        ]
    )


def cmd_update(args) -> int:
    cfg = hv.load()
    table = cfg["harness"].get(args.name) or cfg.get("extension", {}).get(args.name)
    if table is None:
        known = list(cfg["harness"]) + list(cfg.get("extension", {}))
        print(f"harness: unknown harness `{args.name}`: use one of {', '.join(known)}", file=sys.stderr)
        return 2
    up = container_running()
    if not up and not args.dry_run:
        print("harness: the stack is not running: run `just up` (the container installs the pins at start)", file=sys.stderr)
        return 1
    current = container_versions(cfg).get(args.name) if up else None
    target = args.version
    if target is None:
        latest, date, err = npm_latest(table["package"])
        if not latest:
            print(f"harness: cannot read the latest {table['package']} from npm: {err}", file=sys.stderr)
            return 1
        print(f"latest {table['package']} on npm: {latest}" + (f" (published {date[:10]})" if date else ""))
        target = latest
    try:
        plan = make_plan(cfg, args.name, target, current)
    except PlanError as e:
        print(f"harness: {e}", file=sys.stderr)
        return 1
    if plan.bump == "same" and (not args.pin or plan.pinned == plan.new):
        print(f"{plan.name} is at {plan.new} in the container; nothing to do.")
        return 0
    print_plan(plan, args.pin, args.force)
    if args.dry_run:
        print("\ndry run: nothing changed.")
        return 0
    if plan.bump != "same":
        try:
            apply(plan, args.force)
        except UpdateFailed as e:
            print(f"harness: {plan.name}: {e}", file=sys.stderr)
            return 1
    if args.pin:
        write_files([plan])
    print()
    print(next_steps(plan))
    return 0


# ---------------------------------------------------------------- latest


@dataclass
class LatestItem:
    name: str
    kind: str
    package: str
    pinned: str  # the installed version in the container, else the pin
    latest: str | None
    plan: Plan | None
    status: str  # latest | patch | minor | major | unknown | skipped | failed
    note: str = ""


def plan_latest(cfg: dict, latest: dict[str, str | None], current: dict[str, str] | None = None) -> list[LatestItem]:
    """One item for each harness and extension: a plan to the latest npm
    release, or the reason for no change. `latest` maps a name to the npm
    version (None when npm did not answer); `current` maps a name to the
    version in the container."""
    current = current or {}
    out = []
    for e in entries(cfg):
        name, lv = e["name"], latest.get(e["name"])
        base = current.get(name) or e["version"]
        item = LatestItem(name, e["kind"], e["package"], base, lv, None, "unknown")
        if lv is None:
            item.note = "npm did not answer"
            out.append(item)
            continue
        try:
            if parse_version(lv) <= parse_version(base):
                item.status = "latest"
                if parse_version(lv) < parse_version(base):
                    item.note = f"the installed version is newer than the npm latest {lv}"
                out.append(item)
                continue
            plan = make_plan(cfg, name, lv, current=current.get(name))
        except (PlanError, ValueError) as err:
            item.status, item.note = "skipped", str(err)
            out.append(item)
            continue
        item.plan, item.status = plan, plan.bump
        out.append(item)
    return out


def render_latest(items: list[LatestItem]) -> str:
    headers = ["HARNESS", "FROM", "TO", "CHANGE"]
    table = [headers]
    for i in items:
        p = i.plan
        table.append(
            [
                i.name,
                i.pinned,
                p.new if p else (i.latest or "-"),
                f"{i.status} bump" if p and i.status != "failed" else i.status,
            ]
        )
    widths = [max(len(row[c]) for row in table) for c in range(len(headers))]
    lines = ["  ".join(c.ljust(widths[n]) for n, c in enumerate(row)).rstrip() for row in table]
    lines += [f"{i.name}: {i.note}" for i in items if i.note]
    return "\n".join(lines)


def compat_suggestions(items: list[LatestItem]) -> str:
    plans = [i.plan for i in items if i.plan and i.status != "failed"]
    lines = ["The live proof (not a gate; it runs the host binary on your real logins):"]
    names = []
    for p in plans:
        harness = p.name if p.kind == "harness" else "copilot"
        lines.append(f"  npm install -g {p.package}@{p.new}")
        if harness not in names:
            names.append(harness)
    lines += [f"  just ci::compat {n}" for n in names]
    return "\n".join(lines)


def run_latest(
    cfg: dict,
    latest: dict[str, str | None],
    dry_run: bool,
    pin: bool = False,
    force: bool = False,
    current: dict[str, str] | None = None,
) -> int:
    items = plan_latest(cfg, latest, current)
    for i in items:
        if i.plan:
            print(f"{i.name} ({i.package}): {i.pinned} -> {i.plan.new} ({i.status})")
        else:
            print(f"{i.name}: {i.status}" + (f" ({i.pinned})" if i.status == "latest" else f": {i.note}"))
    failed = any(i.status in ("unknown", "skipped") for i in items)
    if not any(i.plan for i in items):
        print("\nnothing to update: every harness is at the latest on npm.")
        return 1 if failed else 0
    if dry_run:
        print("\nplan (dry run: nothing changed):")
        print(render_latest(items))
        print()
        print(compat_suggestions(items))
        return 1 if failed else 0

    done = []
    for i in items:
        if not i.plan:
            continue
        print(f"\n==> {i.name} {i.plan.old} -> {i.plan.new}", flush=True)
        try:
            apply(i.plan, force)
            done.append(i.plan)
        except UpdateFailed as e:
            print(f"harness: {i.name}: {e}", file=sys.stderr)
            i.status, i.note, failed = "failed", str(e).splitlines()[0], True
    if pin and done:
        write_files(done)
    print("\nchanged:")
    print(render_latest(items))
    print()
    print(compat_suggestions(items))
    return 1 if failed else 0


def cmd_latest(args) -> int:
    cfg = hv.load()
    up = container_running()
    if not up and not args.dry_run:
        print("harness: the stack is not running: run `just up` (the container installs the pins at start)", file=sys.stderr)
        return 1
    current = container_versions(cfg) if up else {}
    items = entries(cfg)
    with ThreadPoolExecutor(max_workers=8) as pool:
        answers = list(pool.map(lambda e: npm_latest(e["package"]), items))
    latest = {}
    for e, (v, _date, err) in zip(items, answers):
        latest[e["name"]] = v
        if err:
            print(f"harness: {e['name']}: npm: {err}", file=sys.stderr)
    return run_latest(cfg, latest, args.dry_run, args.pin, args.force, current)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="command", required=True)
    c = sub.add_parser("check", help="compare the pins with the container, the host, and npm")
    c.add_argument("--json", action="store_true")
    u = sub.add_parser("update", help="update one harness in the container, in place")
    u.add_argument("name")
    u.add_argument("version", nargs="?", help="default: the latest on npm")
    u.add_argument("--allow-major", action="store_true", help="accepted for compatibility; has no effect")
    u.add_argument("--force", action="store_true", help="install the version again when its version dir is on disk")
    u.add_argument("--pin", action="store_true", help="also write harnesses.toml and docs/compat.md")
    u.add_argument("--dry-run", action="store_true", help="print the plan; change nothing")
    lt = sub.add_parser("latest", help="update every harness in the container to the latest")
    lt.add_argument("--dry-run", action="store_true", help="print the plan; change nothing")
    lt.add_argument("--pin", action="store_true", help="also write harnesses.toml and docs/compat.md")
    lt.add_argument("--force", action="store_true", help="install each version again when its version dir is on disk")
    a = p.parse_args(argv)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(line_buffering=True)
    try:
        return {"check": cmd_check, "update": cmd_update, "latest": cmd_latest}[a.command](a)
    except (hv.PinError, PlanError) as e:
        print(f"harness: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

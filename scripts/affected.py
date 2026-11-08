#!/usr/bin/env python3
"""The affected tests of a change (TER-100): the plan of `just ci::validate-affected`.

A validate job of the queue gives its recipe the changed paths
(`TERRARIUM_CHANGED_FILES`, docs/api.md 35.2). This script maps them to the
checks that can reach them, and prints the plan or runs it. A merge job runs
the full suite (`just ci::validate`), not this plan.

Before running any plan, the commit gate rejects case-insensitive lines
starting with `Co-authored-by:` anywhere in commit messages since the
merge-base of HEAD and origin/next. Ordinary body mentions and Terrarium-*
provenance trailers are allowed. Full validation runs the same gate via
`--check-commit-trailers`. A missing base fails closed. Plan-only mode
does not run the gate.

A file that a crate source embeds with `include_str!` or `include_bytes!`
from outside that crate (for example `scripts/install-host.sh` in
terrariumd's `src/api/install.rs`) also counts as a change to each source
that embeds it, before the rules below apply to the file itself.

The rules, first match per path:

1. Full suite: no list of changed files, or any path that changes the build
   or the gate: `Cargo.toml` (any), `Cargo.lock`, `build.rs`,
   `rust-toolchain*`, `.cargo/`, `clippy.toml`, `rustfmt.toml`, `deny.toml`,
   `justfile`, `.just/`, `.github/`, this script. A path outside the known
   areas below is also the full suite.
2. No tests: `docs/`, `LICENSE`.
3. `dashboard/`: `ci::dashboard-check` only. `scripts/`: `ci::scripts-test`
   only (plus the embedding sources above).
4. A file under `crates/<crate>/`: that crate and its reverse dependencies
   in the workspace (path dependencies of any kind: normal, dev, build), as
   whole crates. Except terrariumd, rule 5. A reverse dependency is always
   tested whole.
5. terrariumd, at the test-target level:
   - `src/<module>` (`src/api/<x>` and `src/store/<x>` count as module
     `<x>`): the lib and bin unit tests, plus the test targets
     `tests/<module>.rs` (also `<module>s.rs`), plus each `tests/*.rs` that
     names `terrariumd::<module>`, `terrariumd::api::<module>`, or
     `terrariumd::store::<module>`.
   - All targets when the module is shared by every test (`lib.rs`,
     `main.rs`, `db`, `error`, `policy`, `api/mod.rs`, `store/mod.rs`, and
     each module that `tests/common` names), or when no target matches.
   - Explicit #[path] modules of a consolidated target are indexed by
     filename and daemon references. Edits select the containing target.
   - `tests/<t>.rs`: only its containing target. `tests/common/`, `tests/fixtures/`,
     or another file of the crate: all targets.
   - Guard (TER-179): any changed `crates/<crate>/src/**/*.rs`, in any crate,
     also runs terrariumd's `no_token_endpoint` target. That test scans the
     `src/` of every crate, so it reaches a change that no target names
     (rule 4 for crates other than terrariumd: terrariumd is then in the plan
     with that target alone).
6. Files outside `crates/` that the crates read: `defaults/` is terrariumd,
   `examples/` is terrarium-core and terrariumd, `harnesses.toml` is
   terrarium-core and the scripts tests (each with its reverse
   dependencies, whole). Markdown files count here too: terrariumd tests
   render the prompts under `defaults/`.
7. No tests: a Markdown file outside the areas above (top level,
   `spikes/`, `docker/`, `config/`, `fleets/`). A Markdown file under
   `crates/`, `defaults/`, `examples/`, `dashboard/`, or `scripts/` follows
   the rule of its area.

Every plan runs `ci::fmt-check`, and clippy (`--all-targets -D warnings`) on
the affected crates. The known gap: a change that breaks a test target that
the rules do not pick shows up in the full suite of the merge batch.

Usage: affected.py [--files FILE] [--run] [--json]. Without `--files`, the
file named by `TERRARIUM_CHANGED_FILES`. Run it in the repo root.
"""

from __future__ import annotations

import argparse
import json
import os
import posixpath
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

DAEMON = "terrariumd"

# Rule 1: a path that changes the build or the gate.
FULL_NAMES = {"Cargo.toml", "Cargo.lock", "build.rs", "clippy.toml", ".clippy.toml",
              "rustfmt.toml", ".rustfmt.toml", "deny.toml", "justfile"}
FULL_PREFIXES = (".cargo/", ".just/", ".github/", "rust-toolchain", "scripts/affected.py")
# Rule 2.
NO_TEST_PREFIXES = ("docs/",)
NO_TEST_NAMES = {"LICENSE"}
# Rule 6: paths outside crates/ and the crates that read them.
OUTSIDE = {
    "defaults/": ["terrariumd"],
    "examples/": ["terrarium-core", "terrariumd"],
    "harnesses.toml": ["terrarium-core"],
}
# A literal `include_str!("...")` or `include_bytes!("...")`.
INCLUDE = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)')
# Rule 5: terrariumd modules that every test target reaches.
DAEMON_SHARED = {"lib", "main", "db", "error", "policy", "api", "store"}
# The terrariumd target that scans the sources of every crate (TER-179).
GUARD_TARGET = "no_token_endpoint"


@dataclass
class Crate:
    name: str
    # The crate dir, relative to the repo root, with a trailing slash.
    dir: str
    # The names of the workspace crates it depends on (any kind).
    deps: set[str] = field(default_factory=set)
    # The integration test targets: name -> path relative to the repo root.
    tests: dict[str, str] = field(default_factory=dict)


@dataclass
class Plan:
    full: bool = False
    reasons: list[str] = field(default_factory=list)
    # Crates tested whole.
    whole: set[str] = field(default_factory=set)
    # terrariumd test targets, when terrariumd is not tested whole. None:
    # terrariumd is not in the plan.
    daemon_targets: set[str] | None = None
    scripts: bool = False
    dashboard: bool = False

    def clippy(self) -> list[str]:
        crates = set(self.whole)
        if self.daemon_targets is not None:
            crates.add(DAEMON)
        return sorted(crates)

    def to_dict(self) -> dict:
        return {
            "full": self.full,
            "reasons": self.reasons,
            "whole": sorted(self.whole),
            "daemon_targets": None if self.daemon_targets is None else sorted(self.daemon_targets),
            "clippy": self.clippy(),
            "scripts": self.scripts,
            "dashboard": self.dashboard,
        }


def workspace(metadata: dict, root: Path) -> dict[str, Crate]:
    """The workspace crates from `cargo metadata --no-deps` output."""
    root = root.resolve()
    members = set(metadata.get("workspace_members", []))
    crates: dict[str, Crate] = {}
    for p in metadata["packages"]:
        if members and p["id"] not in members:
            continue
        cdir = Path(p["manifest_path"]).resolve().parent
        rel = cdir.relative_to(root).as_posix() + "/"
        c = Crate(name=p["name"], dir=rel)
        for t in p.get("targets", []):
            if "test" in t.get("kind", []):
                c.tests[t["name"]] = Path(t["src_path"]).resolve().relative_to(root).as_posix()
        crates[c.name] = c
        c.deps = {d["name"] for d in p.get("dependencies", []) if d.get("path")}
    for c in crates.values():
        c.deps = {d for d in c.deps if d in crates and d != c.name}
    return crates


def embeds(crates: dict[str, Crate], files: dict[str, str]) -> dict[str, set[str]]:
    """The files that a crate source embeds from outside its crate:
    embedded path -> the sources that embed it. `files` maps each crate
    source (relative to the repo root) to its text."""
    out: dict[str, set[str]] = {}
    for src, text in files.items():
        c = crate_of(crates, src)
        if c is None:
            continue
        base = posixpath.dirname(src)
        for m in INCLUDE.finditer(text):
            target = posixpath.normpath(posixpath.join(base, m.group(1)))
            if target.startswith("../") or target.startswith(c.dir):
                continue
            out.setdefault(target, set()).add(src)
    return out


def crate_sources(crates: dict[str, Crate]) -> dict[str, str]:
    """The `.rs` files of the workspace crates on disk, with their text."""
    out = {}
    for c in crates.values():
        for p in sorted(Path(c.dir).glob("**/*.rs")):
            if "target" in p.parts:
                continue
            try:
                out[p.as_posix()] = p.read_text(encoding="utf-8", errors="replace")
            except OSError:
                pass
    return out


def reverse_deps(crates: dict[str, Crate], names: set[str]) -> set[str]:
    """The crates in `names` and every workspace crate that depends on one."""
    out = set(names)
    grew = True
    while grew:
        grew = False
        for c in crates.values():
            if c.name not in out and c.deps & out:
                out.add(c.name)
                grew = True
    return out


def crate_of(crates: dict[str, Crate], path: str) -> Crate | None:
    best = None
    for c in crates.values():
        if path.startswith(c.dir) and (best is None or len(c.dir) > len(best.dir)):
            best = c
    return best


def module_of(rel: str) -> str | None:
    """The module of a path under terrariumd's `src/` (rule 5)."""
    parts = rel.split("/")
    if parts[0] in ("api", "store") and len(parts) >= 2:
        leaf = parts[1]
        if leaf == "mod.rs" and len(parts) == 2:
            return parts[0]
        return leaf.removesuffix(".rs")
    return parts[0].removesuffix(".rs")


def daemon_refs(text: str) -> set[str]:
    """The modules that a test file names: `terrariumd::<m>`, and
    `terrariumd::api::<m>` or `terrariumd::store::<m>` as `<m>`."""
    out = set()
    for m in re.finditer(r"terrariumd::([a-z_][a-z0-9_]*)(?:::([a-z_][a-z0-9_]*))?", text):
        top, sub = m.group(1), m.group(2)
        out.add(top)
        if top in ("api", "store") and sub:
            out.add(sub)
    return out


def test_sources(path: str, read) -> dict[str, str]:
    """A test entry point and its explicit #[path] modules, including groups.

    The consolidated daemon target keeps its modules beside the standalone
    targets. Follow those declarations so both source references and edits to
    a module select the containing binary, not a nonexistent old target.
    """
    sources = {}
    pending = [path]
    while pending:
        p = pending.pop()
        if p in sources:
            continue
        try:
            text = read(p)
        except OSError:
            text = ""
        sources[p] = text
        for child in re.findall(r'#\[path\s*=\s*"([^"]+)"\]\s*mod\s+\w+\s*;', text):
            pending.append(posixpath.normpath(posixpath.join(posixpath.dirname(p), child)))
    return sources


def daemon_targets_for(module: str, tests: dict[str, str], refs: dict[str, set[str]],
                       shared: set[str]) -> set[str] | None:
    """The terrariumd test targets of a module, or None for all."""
    if module in shared:
        return None
    hit = {t for t in tests if t in (module, module + "s")}
    hit |= {t for t, r in refs.items() if module in r or module + "s" in r}
    return hit or None


def plan(changed: list[str] | None, crates: dict[str, Crate],
         read=lambda p: Path(p).read_text(encoding="utf-8", errors="replace"),
         common: list[str] | None = None,
         embedded: dict[str, set[str]] | None = None) -> Plan:
    """The plan for `changed` (paths relative to the repo root; None: no list).
    `read` reads a file of the repo, `common` lists the files of
    terrariumd's tests/common (default: found on disk), `embedded` maps an
    embedded file to the sources that embed it (default: `embeds` over the
    crate sources on disk)."""
    pl = Plan()
    if changed is None:
        pl.full = True
        pl.reasons.append("no list of changed files")
        return pl
    whole: set[str] = set()
    daemon: set[str] | None = None
    daemon_all = False
    daemon_seen = False
    guard = False
    refs: dict[str, set[str]] | None = None
    sources: dict[str, dict[str, str]] = {}
    shared: set[str] = set()
    dc = crates.get(DAEMON)
    if embedded is None:
        embedded = embeds(crates, crate_sources(crates))
    # An embedded file also changes each source that embeds it.
    paths = [p.strip() for p in changed]
    for p in list(paths):
        for src in sorted(embedded.get(p, ())):
            if src not in paths:
                paths.append(src)

    def daemon_index():
        # The modules each terrariumd test target names, and the modules
        # that every target reaches through tests/common.
        nonlocal refs, shared
        if refs is None:
            refs = {}
            for t, p in dc.tests.items():
                sources[t] = test_sources(p, read)
                refs[t] = daemon_refs("\n".join(sources[t].values()))
                # Match modules by filename too, just as standalone targets.
                refs[t] |= {Path(s).stem for s in sources[t]}
            names = set()
            files = common if common is not None else [
                p.as_posix() for p in sorted(Path(dc.dir, "tests", "common").glob("**/*.rs"))]
            for p in files:
                names |= daemon_refs(read(p))
            # `terrariumd::start` is in lib.rs.
            shared = DAEMON_SHARED | (names - {"api", "store", "start"})
        return refs

    for path in paths:
        if not path:
            continue
        name = path.rsplit("/", 1)[-1]
        if name in FULL_NAMES or path.startswith(FULL_PREFIXES):
            pl.full = True
            pl.reasons.append(f"{path}: changes the build or the gate")
            continue
        if path.startswith(NO_TEST_PREFIXES) or name in NO_TEST_NAMES:
            continue
        if path.startswith("dashboard/"):
            pl.dashboard = True
            continue
        if path.startswith("scripts/"):
            pl.scripts = True
            continue
        outside = next((v for k, v in OUTSIDE.items() if path == k or path.startswith(k)), None)
        if outside is not None:
            if path == "harnesses.toml":
                pl.scripts = True
            for n in outside:
                if n == DAEMON and dc is not None:
                    daemon_seen = True
                    daemon_all = True
                else:
                    whole.add(n)
            continue
        c = crate_of(crates, path) if path.startswith("crates/") else None
        if c is None and path.endswith(".md") and not path.startswith("crates/"):
            # Rule 7: a Markdown file that no crate reads.
            continue
        if c is None:
            pl.full = True
            pl.reasons.append(f"{path}: outside the known areas")
            continue
        if path.endswith(".rs") and path[len(c.dir):].startswith("src/"):
            guard = True
        if c.name != DAEMON or dc is None:
            whole.add(c.name)
            continue
        daemon_seen = True
        rel = path[len(c.dir):]
        if rel.startswith("src/"):
            daemon_index()
            m = module_of(rel[len("src/"):])
            got = daemon_targets_for(m, dc.tests, refs, shared) if m else None
            if got is None:
                daemon_all = True
            else:
                daemon = (daemon or set()) | got
        elif rel.startswith(("tests/common/", "tests/fixtures/")):
            daemon_all = True
        elif rel.startswith("tests/") and rel.endswith(".rs"):
            daemon_index()
            targets = {t for t, files in sources.items() if path in files}
            if targets:
                daemon = (daemon or set()) | targets
            else:
                daemon_all = True
        else:
            daemon_all = True

    if pl.full:
        return pl
    # The guard (TER-179): a crate source change runs the target that scans
    # every crate's sources. A missing target runs all of terrariumd's.
    if guard and dc is not None:
        daemon_seen = True
        if GUARD_TARGET in dc.tests:
            daemon = (daemon or set()) | {GUARD_TARGET}
        else:
            daemon_all = True
    # Changed crates and their reverse dependencies are tested whole. When
    # terrariumd is one of them, it is tested whole too (all targets).
    tested = reverse_deps(crates, whole & crates.keys())
    if dc is not None and DAEMON in tested:
        daemon_seen = daemon_all = True
    if dc is not None and daemon_seen:
        tested |= reverse_deps(crates, {DAEMON}) - {DAEMON}
        if daemon_all:
            tested.add(DAEMON)
        else:
            pl.daemon_targets = daemon or set()
    pl.whole = tested
    return pl


def commands(pl: Plan) -> list[list[str]]:
    """The commands of a plan, in the order of `ci::validate`."""
    if pl.full:
        return [["just", "ci::validate"]]
    cmds = [["just", "ci::fmt-check"]]
    clippy = pl.clippy()
    if clippy:
        cmd = ["cargo", "clippy"]
        for c in clippy:
            cmd += ["-p", c]
        cmds.append(cmd + ["--all-targets", "--", "-D", "warnings"])
    whole = sorted(pl.whole)
    if whole:
        cmd = ["cargo", "test"]
        for c in whole:
            cmd += ["-p", c]
        cmds.append(cmd + ["--no-fail-fast"])
    if pl.daemon_targets is not None:
        cmd = ["cargo", "test", "-p", DAEMON, "--lib", "--bins"]
        for t in sorted(pl.daemon_targets):
            cmd += ["--test", t]
        cmds.append(cmd + ["--no-fail-fast"])
    if pl.scripts:
        cmds.append(["just", "ci::scripts-test"])
    if pl.dashboard:
        cmds.append(["just", "ci::dashboard-check"])
    return cmds


def read_changed(path: str | None) -> list[str] | None:
    if not path:
        return None
    try:
        return [line for line in Path(path).read_text(encoding="utf-8").splitlines() if line.strip()]
    except OSError:
        return None


def run(cmds: list[list[str]]) -> int:
    for cmd in cmds:
        print("+ " + " ".join(cmd), flush=True)
        if cmd[:2] == ["cargo", "test"]:
            code = subprocess.call([sys.executable, "scripts/rust_tests.py", *cmd[2:]])
        else:
            code = subprocess.call(cmd)
        if code != 0:
            print(f"affected: `{' '.join(cmd)}` failed with exit code {code}", file=sys.stderr)
            return code
    return 0


def check_commit_trailers() -> int:
    """Reject co-author lines anywhere in commits since origin/next's merge-base."""
    try:
        base = subprocess.run(["git", "merge-base", "origin/next", "HEAD"],
                              check=True, capture_output=True, text=True).stdout.strip()
        commits = subprocess.run(["git", "rev-list", f"{base}..HEAD"],
                                 check=True, capture_output=True, text=True).stdout.splitlines()
        rejected = []
        for commit in commits:
            message = subprocess.run(
                ["git", "show", "-s", "--format=%B", commit],
                check=True, capture_output=True, text=True).stdout
            if re.search(r"^co-authored-by:", message, re.IGNORECASE | re.MULTILINE):
                rejected.append(commit)
    except subprocess.CalledProcessError as error:
        print(f"affected: cannot check commit trailers: {error.stderr.strip()}", file=sys.stderr)
        return 1
    if rejected:
        for commit in rejected:
            print(f"affected: {commit}: forbidden Co-authored-by trailer", file=sys.stderr)
        return 1
    return 0


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--files", help="the file of changed paths (default: $TERRARIUM_CHANGED_FILES)")
    ap.add_argument("--run", action="store_true", help="run the plan")
    ap.add_argument("--check-commit-trailers", action="store_true", help="run only the commit gate")
    ap.add_argument("--json", action="store_true", help="print the plan as JSON")
    a = ap.parse_args(argv)
    if a.check_commit_trailers:
        return check_commit_trailers()
    if a.run and check_commit_trailers():
        return 1
    changed = read_changed(a.files or os.environ.get("TERRARIUM_CHANGED_FILES"))
    if changed is None:
        crates: dict[str, Crate] = {}
    else:
        meta = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                              check=True, capture_output=True, text=True).stdout
        crates = workspace(json.loads(meta), Path.cwd())
    pl = plan(changed, crates)
    cmds = commands(pl)
    if a.json:
        print(json.dumps(dict(pl.to_dict(), commands=cmds), indent=2))
    else:
        n = "no list" if changed is None else f"{len(changed)} changed files"
        print(f"affected: {n}; " + ("full suite: " + "; ".join(pl.reasons[:5]) if pl.full else "affected set"))
        for c in cmds:
            print("  " + " ".join(c))
    if a.run:
        if os.environ.get("RESULTS_DIR"):
            if not pl.full:
                directory = Path(os.environ["RESULTS_DIR"])
                for p in [directory / "rust.json"]:
                    if p.exists():
                        p.unlink()
        return run(cmds)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

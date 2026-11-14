#!/usr/bin/env python3
"""Disk cleanup for the build target (TER-1282): per-card trees, shared caps, a free floor.

Card agents leave checkouts, cargo target dirs, logs and result dirs under
/src on terrarium-helios-build2. This script removes the ones of finished
cards, caps the shared cargo target dirs, and checks the free-space floor
before a worker makes a new per-card target dir. Every deleting command is a
dry run unless `--apply` is given. Run it on the target with the system
python3 (no dependencies).

`clean --live A,B,... --done AGENT:CARD ... [--apply]`

  `--live` is required and may not be empty: the names of every agent that
  is still alive (`terrarium agents ls`). `--done` names a finished card (accepted or done)
  and its agent (the `by:<agent>` label). Only direct entries of the root
  (`/src`) are looked at, and only these are selected for a pair:

  - `<agent>-<card>` and `<agent>-<card>-*` (the card ID in any case),
  - `<agent>-target*` and `<agent>-dev*`,
  - every `<agent>` and `<agent>-*` entry when the agent is a bound card
    agent, named `<word>-<number>` with the number of its card.

  Kept, whatever the pair: a name that starts with `terrarium` (the shared
  targets, the slot locks, the lead's tree), a symlink, an entry of a live
  agent (`<live>` or `<live>-*`), and an entry that is the working
  directory or the `CARGO_TARGET_DIR` of a running process, or holds one.
  When the cwd or environ of some process cannot be read (another user),
  nothing is removed and the reason names those pids.
  Each selected and kept path is printed with its size and the reason, and
  the total freed (or that a dry run would free) is printed at the end.
  An entry that fails to delete is reported as `error`, the run goes on,
  and the exit code is 1.

`cap --cap-gib N [--apply]`

  The shared targets `terrarium-cardcheck-target-<s>` (lock
  `terrarium-cardcheck-<s>.lock`, which must exist), `terrarium-queue-target-<s>`
  (lock `terrarium-queue-<s>.lock`) and `terrarium-dev-target` (lock
  `terrarium-dev-target.lock`) are never removed. One that is over N GiB is
  emptied, the same as `cargo clean`, only while this script holds its own
  lock and every cargo `.cargo-lock` in it with a non-blocking flock, and
  no running process uses it (the same check as `clean`). A held lock or a
  busy dir skips it. The `.cargo-lock` files stay, so a cargo that starts
  during the clean waits on them; a dir with a new `.cargo-lock` is left
  alone. An entry that fails to delete is reported, and the exit code is 1.

`floor [PATH] [--min-free-pct 20]`

  Exit 1 with a message when the filesystem of PATH (default the root) has
  less than the floor free (20 percent, the floor of the services payload
  store, TER-1264). Run it before making a per-card target dir.

Common: `--root` (default /src) and `--json`. Exit 0, 1 for a floor below
the limit or a failed delete, 2 for bad arguments.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import stat
import sys

DEFAULT_ROOT = "/src"
FLOOR_PCT = 20.0
GIB = 1024**3

AGENT_RE = re.compile(r"[a-z][a-z0-9]*(?:-[a-z0-9]+)*")
CARD_RE = re.compile(r"[A-Za-z]+-(\d+)")
BOUND_RE = re.compile(r"[a-z]+-(\d+)")
# The shared targets and their own lock: (pattern, lock name, the lock must
# already exist). The card check takes `terrarium-cardcheck-<s>.lock`
# (AGENTS.md, the card check loop). No code takes the other two names: the
# daemon's job slots lock `<work_dir>/<project>/slot-<n>.lock` and build in
# `slot-<n>/target`, outside /src (crates/terrariumd/src/jobs.rs:912, :932,
# docs/api.md "Each slot has one work tree"). For those two the script
# makes and takes its own lock, so two cleanups do not run at once, and
# relies on the cargo locks and the process check.
SHARED = [
    (re.compile(r"terrarium-cardcheck-target-(\d+)"), "terrarium-cardcheck-{}.lock", True),
    (re.compile(r"terrarium-queue-target-(\d+)"), "terrarium-queue-{}.lock", False),
    (re.compile(r"terrarium-dev-target"), "terrarium-dev-target.lock", False),
]


class UsageError(Exception):
    pass


def human(n: int) -> str:
    v = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if v < 1024 or unit == "TiB":
            return f"{v:.1f} {unit}" if unit != "B" else f"{int(v)} B"
        v /= 1024
    return f"{n} B"


def du(path: Path) -> int:
    """Bytes on disk of a path, without following symlinks."""
    try:
        st = path.lstat()
    except FileNotFoundError:
        return 0
    total = st.st_blocks * 512
    if not stat.S_ISDIR(st.st_mode):
        return total
    for top, dirs, files in os.walk(path, followlinks=False):
        for name in dirs + files:
            with contextlib.suppress(OSError):
                total += os.lstat(os.path.join(top, name)).st_blocks * 512
    return total


def check_root(root: str) -> Path:
    p = Path(root)
    if not p.is_absolute():
        raise UsageError(f"root must be absolute: {root}")
    real = p.resolve()
    if real == Path("/") or real == Path.home().resolve() or not real.is_dir():
        raise UsageError(f"refusing root {root}")
    return real


def parse_done(items: list[str]) -> list[tuple[str, str]]:
    out = []
    for item in items:
        agent, sep, card = item.partition(":")
        if not sep or not AGENT_RE.fullmatch(agent) or not CARD_RE.fullmatch(card):
            raise UsageError(f"--done wants AGENT:CARD, got {item!r}")
        if agent.startswith("terrarium"):
            raise UsageError(f"refusing agent name {agent!r}")
        out.append((agent, card))
    return out


def parse_live(value: str) -> list[str]:
    names = [n.strip() for n in value.split(",") if n.strip()]
    if not names:
        # The lead itself is always live: an empty list is a mistake.
        raise UsageError("--live is empty: give every live agent (terrarium agents ls)")
    for n in names:
        if not AGENT_RE.fullmatch(n):
            raise UsageError(f"bad live agent name {n!r}")
    return names


def is_bound(agent: str, card: str) -> bool:
    m = BOUND_RE.fullmatch(agent)
    c = CARD_RE.fullmatch(card)
    return bool(m and c and int(m.group(1)) == int(c.group(1)))


def matches(name: str, agent: str, card: str) -> bool:
    if is_bound(agent, card) and (name == agent or name.startswith(agent + "-")):
        return True
    if not name.startswith(agent + "-"):
        return False
    rest = name[len(agent) + 1 :]
    low, c = rest.lower(), card.lower()
    return low == c or low.startswith(c + "-") or rest.startswith(("target", "dev"))


def busy_paths(proc: Path) -> tuple[list[Path], list[str]]:
    """The working dirs and CARGO_TARGET_DIRs of the running processes, and
    the pids whose cwd or environ could not be read (not a process that
    ended while it was read)."""
    out: list[Path] = []
    unreadable: list[str] = []
    for d in proc.iterdir() if proc.is_dir() else []:
        if not d.name.isdigit():
            continue
        failed = False
        try:
            out.append(Path(os.readlink(d / "cwd")))
        except (FileNotFoundError, ProcessLookupError):
            pass  # the process ended, or is a zombie with no cwd
        except OSError:
            failed = True
        try:
            for var in (d / "environ").read_bytes().split(b"\0"):
                if var.startswith(b"CARGO_TARGET_DIR="):
                    out.append(Path(os.fsdecode(var.split(b"=", 1)[1])))
        except (FileNotFoundError, ProcessLookupError):
            pass
        except OSError:
            failed = True
        if failed:
            unreadable.append(d.name)
    return out, unreadable


def in_use(path: Path, busy: list[Path]) -> bool:
    for b in busy:
        if b == path or path in b.parents:
            return True
    return False


def busy_reason(path: Path, proc: Path) -> str | None:
    """Why a path must be kept for a running process, or None."""
    busy, unreadable = busy_paths(proc)
    if in_use(path, busy):
        return "used by a running process"
    if unreadable:
        return f"cannot read {len(unreadable)} processes in {proc} (pids {', '.join(unreadable[:5])}), so their trees are unknown"
    return None


def select(root: Path, done: list[tuple[str, str]], live: list[str], proc: Path):
    """Each entry of root that a done pair names, as (path, action, reason)."""
    rows = []
    for entry in sorted(root.iterdir(), key=lambda p: p.name):
        name = entry.name
        pairs = [(a, c) for a, c in done if matches(name, a, c)]
        if not pairs:
            continue
        agent, card = pairs[0]
        owner = next((l for l in live if name == l or name.startswith(l + "-")), None)
        if name.startswith("terrarium"):
            rows.append((entry, "keep", "shared terrarium entry"))
        elif entry.is_symlink():
            rows.append((entry, "keep", "symlink"))
        elif entry.parent != root:
            rows.append((entry, "keep", "outside the root"))
        elif owner:
            rows.append((entry, "keep", f"live agent {owner}"))
        elif why := busy_reason(entry, proc):
            rows.append((entry, "keep", why))
        else:
            rows.append((entry, "remove", f"{agent} {card}"))
    return rows


def remove(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()


def cmd_clean(args, out) -> int:
    root = check_root(args.root)
    done = parse_done(args.done)
    live = parse_live(args.live)
    proc = Path(args.proc)
    rows = select(root, done, live, proc)
    freed = 0
    errors = 0
    report = []
    for path, action, reason in rows:
        size = du(path)
        gone = size
        if action == "remove" and args.apply:
            # Check again just before the delete: a run may have started.
            why = busy_reason(path, proc)
            if why:
                action, reason = "keep", why
            else:
                try:
                    remove(path)
                except OSError as e:
                    errors += 1
                    gone = size - du(path)
                    action, reason = "error", f"{type(e).__name__}: {e}"
        if action in ("remove", "error"):
            freed += gone
        report.append({"path": str(path), "action": action, "reason": reason, "bytes": size})
    mode = "freed" if args.apply else "dry run, would free"
    if args.json:
        json.dump({"apply": args.apply, "freed_bytes": freed, "errors": errors, "entries": report}, out, indent=2)
        out.write("\n")
    else:
        for r in report:
            out.write(f"{r['action']:6} {human(r['bytes']):>10}  {r['path']}  ({r['reason']})\n")
        out.write(f"{mode}: {human(freed)} in {sum(r['action'] == 'remove' for r in report)} entries\n")
        if errors:
            out.write(f"errors: {errors} entries could not be removed\n")
    return 1 if errors else 0


@contextlib.contextmanager
def try_locks(paths: list[Path], create: bool = False):
    """Yield True when every lock is taken with a non-blocking flock. A
    missing lock file is made only when `create` is set, else skipped."""
    held = []
    try:
        for p in paths:
            if p.exists():
                fd = os.open(p, os.O_RDONLY)
            elif create:
                fd = os.open(p, os.O_RDWR | os.O_CREAT, 0o644)
            else:
                continue
            held.append(fd)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                yield False
                return
        yield True
    finally:
        for fd in held:
            os.close(fd)


def cargo_locks(target: Path) -> list[Path]:
    return sorted(set(target.glob(".cargo-lock")) | set(target.glob("*/.cargo-lock")) | set(target.glob("*/*/.cargo-lock")))


def empty_target(target: Path) -> list[str]:
    """Remove the build output of a target dir, keep its `.cargo-lock` files.

    A dir that holds a `.cargo-lock` that was not there at the start (a
    cargo that began a new profile during the clean) is left alone. Returns
    the errors; each failed entry is skipped and the clean goes on."""
    keep = set(cargo_locks(target))
    errors = []
    for top, dirs, files in os.walk(target, topdown=True):
        here = Path(top)
        for name in list(dirs):
            sub = here / name
            if any(sub in k.parents for k in keep) and not sub.is_symlink():
                continue  # walk into it, keep its lock
            dirs.remove(name)
            if not sub.is_symlink() and set(cargo_locks(sub)) - keep:
                errors.append(f"{sub}: a new .cargo-lock appeared, left alone")
                continue
            try:
                if sub.is_symlink():
                    sub.unlink()
                else:
                    shutil.rmtree(sub)
            except OSError as e:
                errors.append(f"{sub}: {type(e).__name__}: {e}")
        for name in files:
            f = here / name
            if f not in keep and name != "CACHEDIR.TAG":
                try:
                    f.unlink()
                except OSError as e:
                    errors.append(f"{f}: {type(e).__name__}: {e}")
    return errors


def cmd_cap(args, out) -> int:
    root = check_root(args.root)
    cap = int(args.cap_gib * GIB)
    proc = Path(args.proc)
    report = []
    freed = 0
    errors = 0
    for entry in sorted(root.iterdir(), key=lambda p: p.name):
        lock_name = None
        for rx, lock, required in SHARED:
            m = rx.fullmatch(entry.name)
            if m:
                lock_name = lock.format(*m.groups())
                break
        if lock_name is None or entry.is_symlink() or not entry.is_dir():
            continue
        size = du(entry)
        row = {"path": str(entry), "bytes": size}
        report.append(row)
        if size <= cap:
            row.update(action="keep", reason="under the cap")
            continue
        own = root / lock_name
        if required and not own.is_file():
            row.update(action="keep", reason=f"no slot lock {own}")
            continue
        with try_locks([own] + cargo_locks(entry), create=args.apply) as ok:
            why = busy_reason(entry, proc)
            if not ok:
                row.update(action="keep", reason="over the cap, a lock is held")
            elif why:
                row.update(action="keep", reason=f"over the cap, {why}")
            elif args.apply:
                errs = empty_target(entry)
                after = du(entry)
                freed += size - after
                row.update(action="error" if errs else "clean", reason="over the cap", after=after)
                if errs:
                    errors += 1
                    row["errors"] = errs
            else:
                freed += size
                row.update(action="clean", reason="over the cap")
    mode = "freed" if args.apply else "dry run, would free"
    if args.json:
        json.dump({"apply": args.apply, "cap_bytes": cap, "freed_bytes": freed, "errors": errors, "entries": report}, out, indent=2)
        out.write("\n")
    else:
        for r in report:
            out.write(f"{r['action']:6} {human(r['bytes']):>10}  {r['path']}  ({r['reason']})\n")
            for e in r.get("errors", []):
                out.write(f"       error: {e}\n")
        out.write(f"{mode}: {human(freed)}\n")
    return 1 if errors else 0


def free_pct(path: str) -> tuple[float, int, int]:
    st = os.statvfs(path)
    total = st.f_blocks * st.f_frsize
    free = st.f_bavail * st.f_frsize
    return (100.0 * free / total if total else 0.0), free, total


def cmd_floor(args, out) -> int:
    path = args.path or args.root
    pct, free, total = free_pct(path)
    ok = pct >= args.min_free_pct
    if args.json:
        json.dump({"path": path, "free_pct": round(pct, 2), "free_bytes": free, "total_bytes": total,
                   "min_free_pct": args.min_free_pct, "ok": ok}, out)
        out.write("\n")
    elif ok:
        out.write(f"ok: {path} has {pct:.1f}% free ({human(free)} of {human(total)}), floor {args.min_free_pct:g}%\n")
    else:
        out.write(
            f"refused: {path} has {pct:.1f}% free ({human(free)} of {human(total)}), below the "
            f"{args.min_free_pct:g}% floor. Do not make a per-card target dir: use the shared "
            "CARGO_TARGET_DIR=/src/terrarium-dev-target for dev runs and the slot's "
            "/src/terrarium-cardcheck-target-<s> for the card check (AGENTS.md), and tell the lead.\n"
        )
    return 0 if ok else 1


def main(argv=None, out=sys.stdout) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default=DEFAULT_ROOT)
    ap.add_argument("--json", action="store_true")
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("clean")
    c.add_argument("--live", required=True, help="comma-separated live agents; '' for none")
    c.add_argument("--done", action="append", default=[], metavar="AGENT:CARD")
    c.add_argument("--apply", action="store_true")
    c.add_argument("--proc", default="/proc", help=argparse.SUPPRESS)
    k = sub.add_parser("cap")
    k.add_argument("--cap-gib", type=float, required=True)
    k.add_argument("--apply", action="store_true")
    k.add_argument("--proc", default="/proc", help=argparse.SUPPRESS)
    f = sub.add_parser("floor")
    f.add_argument("path", nargs="?")
    f.add_argument("--min-free-pct", type=float, default=FLOOR_PCT)
    args = ap.parse_args(argv)
    try:
        return {"clean": cmd_clean, "cap": cmd_cap, "floor": cmd_floor}[args.cmd](args, out)
    except UsageError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

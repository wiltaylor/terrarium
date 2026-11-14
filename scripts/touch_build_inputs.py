"""Set the mtime of each build script and its watched files to now (TER-1315).

The card check slots share a cargo target dir across checkouts. Cargo
decides that a build script output is fresh by mtime: when a build of an
older tree in the same slot finishes after a new checkout is made, its
dep-info is newer than the new `build.rs`, and cargo runs the old build
script. `ci::validate` and `ci::validate-affected` run this script first,
so each build script of the checkout is rebuilt and run again.

It touches every tracked `build.rs` and each path that a build script names
with a literal `rerun-if-changed=<path>` (relative to the crate dir), when
the path exists. A touched `build.rs` alone already makes cargo rebuild and
run that script. Only mtimes change: `git status` stays empty.

Usage: touch_build_inputs.py [root]   (default: the current directory)
"""

import os
from pathlib import Path
import re
import subprocess
import sys

RERUN = re.compile(r'rerun-if-changed=([^"{}]+)"')
SKIP_DIRS = {".git", "target", "node_modules"}


def build_scripts(root: Path) -> list[Path]:
    """The tracked build.rs files under root, else every build.rs outside target/."""
    try:
        out = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z", "--", ":(glob)**/build.rs"],
            check=True, capture_output=True, text=True,
        ).stdout
        return sorted(root / p for p in out.split("\0") if p)
    except (OSError, subprocess.CalledProcessError):
        found = []
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
            if "build.rs" in filenames:
                found.append(Path(dirpath) / "build.rs")
        return sorted(found)


def watched(script: Path) -> list[Path]:
    """The existing literal rerun-if-changed paths of a build script."""
    paths = []
    for rel in RERUN.findall(script.read_text(encoding="utf-8")):
        path = script.parent / rel
        if path.exists():
            paths.append(path)
    return paths


def touch_all(root: Path) -> list[Path]:
    """Touch each build script and its watched paths. Returns the touched paths."""
    touched: list[Path] = []
    for script in build_scripts(root):
        for path in [script, *watched(script)]:
            if path not in touched:
                os.utime(path)
                touched.append(path)
    return touched


def main(argv: list[str]) -> int:
    root = Path(argv[1] if len(argv) > 1 else ".").resolve()
    touched = touch_all(root)
    print(f"touch-build-inputs: {len(touched)} paths under {root}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

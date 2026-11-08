#!/usr/bin/env python3
"""Private updater/supervisor interface, never mounted into agent sandboxes.

All release paths are immutable. Only current is replaced atomically. Pause
and probation markers are deliberately durable across updater disconnection.
An interrupted transaction remains held for operator recovery, not half-admitted.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import sys
import time


def paths():
    return (Path(os.environ.get("TERRARIUM_STATE_DIR", "/state")) / "releases",
            Path(os.environ.get("TERRARIUM_RUNTIME_DIR", "/run/terrarium")))


def select(root, release):
    if not release.is_dir() or release.parent != root:
        raise ValueError("release is absent or outside releases")
    tmp = root / "current.new"
    tmp.unlink(missing_ok=True)
    tmp.symlink_to(release.name)
    tmp.replace(root / "current")


def stage(root, image=None):
    bundled = Path(os.environ.get("TERRARIUM_BUNDLED_RELEASE", "/opt/terrarium/bundled"))
    key = (bundled / "release-id").read_text().strip()
    if len(key) != 64 or any(c not in "0123456789abcdef" for c in key):
        raise ValueError("invalid bundled release ID")
    root.mkdir(parents=True, exist_ok=True)
    release = root / key
    if not release.exists():
        temp = root / (key + ".staging")
        # Never reuse a partial copy from an interrupted update.
        if temp.exists():
            shutil.rmtree(temp)
        shutil.copytree(bundled, temp)
        if image is not None:
            (temp / "image.json").write_text(json.dumps(image))
        temp.rename(release)
    if image is not None:
        metadata = release / "image.json.new"
        metadata.write_text(json.dumps(image))
        metadata.replace(release / "image.json")
    return release


def wait_for(check):
    deadline = time.monotonic() + float(os.environ.get("TERRARIUM_UPGRADE_CONTROL_TIMEOUT", "660"))
    while not check():
        if time.monotonic() >= deadline:
            raise TimeoutError("supervisor did not acknowledge upgrade command")
        time.sleep(0.1)


def main(argv):
    root, run = paths()
    command = argv[0]
    if command == "init":
        # A container replacement always boots ITS image, not the last hot swap.
        # Docker restart of this same container preserves the selected release.
        initialized = run / "upgrade-initialized"
        if not initialized.exists():
            select(root, stage(root))
            (root / "probation").unlink(missing_ok=True)
            initialized.touch()
    elif command == "stage":
        print(stage(root, json.loads(argv[1])).name)
    elif command == "active":
        print((root / "current" / "image.json").read_text())
    elif command == "probe":
        if (run / "upgrade-pause").exists() or (root / "probation").exists():
            raise RuntimeError("unfinished upgrade requires recovery before another update")
        if not (run / "upgrade-supervisor.pid").is_file():
            raise RuntimeError("no hot-swap supervisor")
        print((root / "current").resolve().name)
    elif command == "pause":
        (run / "upgrade-pause").touch()
        pid = int((run / "upgrade-supervisor.pid").read_text())
        os.kill(pid, signal.SIGUSR1)
        wait_for(lambda: (run / "upgrade-paused").exists())
    elif command == "start":
        if not (run / "upgrade-paused").exists():
            raise RuntimeError("daemon must be stopped before selecting a release")
        key = argv[1]
        if len(key) != 64 or any(c not in "0123456789abcdef" for c in key):
            raise ValueError("invalid release ID")
        select(root, root / key)
        (root / "probation").touch()
        (run / "upgrade-pause").unlink()
        wait_for(lambda: not (run / "upgrade-paused").exists())
    elif command == "commit":
        (root / "probation").unlink(missing_ok=True)
    elif command in ("snapshot", "restore-database"):
        # Full restore moves the agent directories, which live runners still
        # have open. Automatic hot rollback must restore ONLY the database.
        database = root.parent / "terrarium.db"
        snapshot = Path(argv[1] + ".hot-swap.sqlite")
        source, destination = ((database, snapshot) if command == "snapshot"
                               else (snapshot, database))
        if not source.is_file():
            raise RuntimeError("missing offline database snapshot")
        with sqlite3.connect(f"file:{source}?mode=ro", uri=True) as src:
            with sqlite3.connect(destination) as dst:
                src.backup(dst)
        destination.chmod(0o600)
    else:
        raise ValueError("unknown upgrade command")


if __name__ == "__main__":
    main(sys.argv[1:])

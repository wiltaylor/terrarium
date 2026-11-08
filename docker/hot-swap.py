#!/usr/bin/env python3
"""Private updater/supervisor interface, never mounted into agent sandboxes.

All release paths are immutable. Only current is replaced atomically. Pause
and probation markers are deliberately durable across updater disconnection.
An interrupted transaction remains held for operator recovery, not half-admitted.
"""
import json
import http.client
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import sys
import time


def paths():
    runtime = os.environ.get("TERRARIUM_RUNTIME_DIR")
    if not runtime:
        xdg = os.environ.get("XDG_RUNTIME_DIR")
        runtime = str(Path(xdg) / "terrarium") if xdg else "/run/terrarium"
    return (Path(os.environ.get("TERRARIUM_STATE_DIR", "/state")) / "releases",
            Path(runtime))


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


def wait_for(check, seconds=None):
    deadline = time.monotonic() + float(seconds if seconds is not None else os.environ.get("TERRARIUM_UPGRADE_CONTROL_TIMEOUT", "660"))
    while not check():
        if time.monotonic() >= deadline:
            raise TimeoutError("supervisor did not acknowledge upgrade command")
        time.sleep(0.1)


def transaction(root):
    try:
        return json.loads((root / "transaction.json").read_text())
    except FileNotFoundError:
        return {}


def record(root, **changes):
    value = {**transaction(root), **changes}
    temp = root / "transaction.json.new"
    with temp.open("w") as output:
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())
    temp.replace(root / "transaction.json")
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def prune(root):
    """Keep current, rollback target, and every payload with a live process."""
    keep = {(root / name).resolve().name for name in ("current", "previous")}
    prefix = str(root.resolve()) + "/"
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            exe = os.readlink(proc / "exe")
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        if exe.startswith(prefix):
            keep.add(exe[len(prefix):].split("/", 1)[0])
    for path in root.iterdir():
        if (len(path.name) == 64 and all(c in "0123456789abcdef" for c in path.name)
                and path.name not in keep and path.is_dir() and not path.is_symlink()):
            shutil.rmtree(path)


def main(argv):
    root, run = paths()
    command = argv[0]
    if command == "init":
        if os.environ.get("TERRARIUM_DAEMON_SERVICE") != "1":
            raise RuntimeError("initialization is exclusive to the daemon service")
        run.mkdir(parents=True, exist_ok=True)
        # A container replacement always boots ITS image, not the last hot swap.
        # Docker restart of this same container preserves the selected release.
        initialized = run / "upgrade-initialized"
        if not initialized.exists():
            if (root / "probation").exists():
                raise RuntimeError("unfinished probation: recover the existing transaction before replacing its container")
            release = stage(root)
            # The same payload can occur in a different runtime image. A new
            # container's identity comes from Docker until its first hot swap.
            (release / "image.json").unlink(missing_ok=True)
            select(root, release)
            initialized.touch()
    elif command == "stage":
        print(stage(root, json.loads(argv[1])).name)
    elif command == "begin":
        if transaction(root).get("phase") not in (None, "committed", "rolled_back", "aborted"):
            raise RuntimeError("recover the unfinished transaction first")
        if len(argv[1]) != 64 or any(c not in "0123456789abcdef" for c in argv[1]) or not (root / argv[1]).is_dir():
            raise ValueError("unstaged release")
        record(root, phase="staged", old=(root / "current").resolve().name,
               new=argv[1], backup=None, admission_attempted=False)
    elif command == "status":
        print(json.dumps({**transaction(root), "current": (root / "current").resolve().name,
                          "paused": (run / "upgrade-paused").exists(),
                          "pause_requested": (run / "upgrade-pause").exists(),
                          "probation": (root / "probation").exists()}))
    elif command == "ready":
        if (root / "probation").exists():
            raise RuntimeError("probation is not committed")
        connection = http.client.HTTPConnection("localhost", timeout=5)
        connection.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.sock.settimeout(5)
        connection.sock.connect(os.environ.get("TERRARIUM_API_SOCKET", str(run / "api.sock")))
        try:
            connection.request("GET", "/api/health")
            response = connection.getresponse()
            if response.status != 200 or json.loads(response.read())["status"] != "ok":
                raise RuntimeError("post-commit activation is not ready")
        finally:
            connection.close()
    elif command == "active":
        try:
            print((root / "current" / "image.json").read_text())
        except FileNotFoundError:
            print("null")
    elif command == "probe":
        if (run / "upgrade-pause").exists() or (root / "probation").exists():
            raise RuntimeError("unfinished upgrade requires recovery before another update")
        if not (run / "upgrade-supervisor.pid").is_file():
            raise RuntimeError("no hot-swap supervisor")
        os.kill(int((run / "upgrade-supervisor.pid").read_text()), 0)
        print((root / "current").resolve().name)
    elif command == "pause":
        (run / "upgrade-pause").touch()
        signalled = False
        try:
            pid = int((run / "upgrade-supervisor.pid").read_text())
            os.kill(pid, signal.SIGUSR1)
            signalled = True
            wait_for(lambda: (run / "upgrade-paused").exists(), argv[1] if len(argv) > 1 else None)
            # Pausing is orthogonal to the durable transaction phase. Recovery
            # may pause a migrated probation daemon or an already-restored one.
        except BaseException:
            if signalled:
                (run / "upgrade-resume").touch()
            (run / "upgrade-pause").unlink(missing_ok=True)
            raise
    elif command == "cancel-pause":
        if (root / "probation").exists() or transaction(root).get("phase") in ("probation", "restoring", "restored"):
            raise RuntimeError("rollback in progress: restore/start/commit, not cancel-pause")
        # The daemon may already be draining after SIGTERM. Keep a restart
        # request until the supervisor launches it again, even for exit 0.
        if (run / "upgrade-pause").exists() or (run / "upgrade-paused").exists():
            (run / "upgrade-resume").touch()
        (run / "upgrade-pause").unlink(missing_ok=True)
        if (run / "upgrade-resume").exists():
            # The original signal may have been handled after pause cleanup,
            # without stopping the child. Explicitly deliver the restart request.
            os.kill(int((run / "upgrade-supervisor.pid").read_text()), signal.SIGUSR1)
        wait_for(lambda: not (run / "upgrade-paused").exists()
                 and not (run / "upgrade-resume").exists(), argv[1] if len(argv) > 1 else None)
        record(root, phase="aborted")
    elif command == "start":
        if not (run / "upgrade-paused").exists():
            raise RuntimeError("daemon must be stopped before selecting a release")
        key = argv[1]
        if len(key) != 64 or any(c not in "0123456789abcdef" for c in key):
            raise ValueError("invalid release ID")
        info = transaction(root)
        if key != info.get("old") and info.get("phase") not in ("backed_up", "restored"):
            raise RuntimeError("new release requires a completed offline snapshot")
        if info.get("admission_attempted") and key != (root / "current").resolve().name:
            raise RuntimeError("admission was attempted: use container replacement for rollback")
        select(root, root / key)
        (root / "probation").touch()
        record(root, phase="probation", selected=key)
        (run / "upgrade-pause").unlink()
        wait_for(lambda: not (run / "upgrade-paused").exists(), argv[2] if len(argv) > 2 else None)
    elif command == "commit":
        info = transaction(root)
        record(root, phase="commit_attempted", admission_attempted=True)
        (root / "probation").unlink(missing_ok=True)
        backup = argv[1] if len(argv) > 1 else info.get("backup")
        if backup:
            Path(backup + ".hot-swap.sqlite").unlink(missing_ok=True)
        if len(argv) > 2:
            previous = argv[2]
            if len(previous) != 64 or any(c not in "0123456789abcdef" for c in previous):
                raise ValueError("invalid previous release")
            temp = root / "previous.new"
            temp.unlink(missing_ok=True)
            temp.symlink_to(previous)
            temp.replace(root / "previous")
        prune(root)
        record(root, phase="rolled_back" if (root / "current").resolve().name == info.get("old") else "committed")
    elif command in ("snapshot", "restore-database"):
        # Full restore moves the agent directories, which live runners still
        # have open. Automatic hot rollback must restore ONLY the database.
        database = root.parent / "terrarium.db"
        snapshot = Path(argv[1] + ".hot-swap.sqlite")
        source, destination = ((database, snapshot) if command == "snapshot"
                               else (snapshot, database))
        if not source.is_file():
            raise RuntimeError("missing offline database snapshot")
        if command == "snapshot":
            record(root, phase="snapshotting", backup=argv[1])
            fd = os.open(destination, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            os.close(fd)
        else:
            if transaction(root).get("admission_attempted"):
                raise RuntimeError("admission was attempted: never rewind this journal with retained runners")
            record(root, phase="restoring")
        with sqlite3.connect(source.resolve().as_uri() + "?mode=ro", uri=True) as src:
            with sqlite3.connect(destination) as dst:
                src.backup(dst)
        destination.chmod(0o600)
        record(root, phase="backed_up" if command == "snapshot" else "restored")
    else:
        raise ValueError("unknown upgrade command")


if __name__ == "__main__":
    main(sys.argv[1:])

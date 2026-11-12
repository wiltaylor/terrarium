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
import datetime
import fcntl
import re
import stat
import zipfile


def backup_directory():
    root, _ = paths()
    return Path(os.environ.get("TERRARIUM_BACKUP_DIR", str(root.parent / "backups")))


def retention_json(directory, name, value):
    """Private durable control files. Never follow an existing temp symlink."""
    temp = directory / (name + ".new")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as output:
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temp, directory / name)
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def retention_read(directory, name):
    fd = os.open(directory / name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd) as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise RuntimeError("non-regular retention control")
        value = source.read(65537)
        if len(value) > 65536:
            raise RuntimeError("oversized retention control")
        return json.loads(value)


def retention_authorized(directory, token):
    if not token or (directory / (".retention-" + token + ".revoked")).exists():
        return None
    control = retention_read(directory, ".retention-control.json")
    expiry = control.get("expires_at")
    return control if (control.get("token") == token and isinstance(expiry, (int, float))
                       and time.time() < expiry <= time.time() + 10.0) else None


def process_start(pid):
    return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]


def retention_control(action, value):
    token = value["token"]
    if not re.fullmatch(r"[A-Za-z0-9]{26}", token):
        raise ValueError("invalid retention token")
    directory = backup_directory()
    directory.mkdir(parents=True, exist_ok=True)
    worker_name = ".retention-" + token + ".worker.json"
    if action in ("prepare", "renew"):
        fd = os.open(directory / ".retention-control.lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if (directory / (".retention-" + token + ".revoked")).exists():
                raise RuntimeError("retention token already revoked")
            if action == "renew" and retention_authorized(directory, token) is None:
                raise RuntimeError("retention lease is no longer current")
            expiry = value.get("expires_at")
            if not isinstance(expiry, (int, float)) or not time.time() < expiry <= time.time() + 10.0:
                raise RuntimeError("invalid or expired retention lease")
            retention_json(directory, ".retention-control.json", value)
        return {}
    if action == "cancel":
        # Revoke WITHOUT waiting on the directory lock. A delayed prepare/start
        # cannot resurrect this authorization, even if an RPC was lost.
        fd = os.open(directory / (".retention-" + token + ".revoked"), os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        os.fsync(fd)
        os.close(fd)
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        os.fsync(fd)
        os.close(fd)
        try:
            identity = retention_read(directory, worker_name)
        except FileNotFoundError:
            return {}
        pid = identity["pid"]
        if not isinstance(pid, int) or pid < 2 or pid == os.getpid():
            raise RuntimeError("invalid worker PID")
        try:
            pidfd = os.pidfd_open(pid)
        except ProcessLookupError:
            return identity
        try:
            # pidfd pins the process across the start-time check and signal.
            if process_start(pid) == identity["start"]:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        except ProcessLookupError:
            pass
        finally:
            os.close(pidfd)
        return identity
    if action == "status":
        try:
            return retention_read(directory, worker_name)
        except FileNotFoundError:
            return {}
    if action == "receipt":
        return retention_read(directory, ".retention-" + token + ".receipt.json")
    raise ValueError("unknown retention control action")


def retain_backups(protected, now=None, preview=False, token=None):
    """Called only by the updater with its update/rollback lock held.

    The directory flock is also held by every ZIP creator. Never follow a
    backup symlink, and never derive a deletion path from archive contents.
    A durable local journal records intent and outcome for each exact file.
    """
    now = now or datetime.datetime.now(datetime.timezone.utc)
    directory = backup_directory()
    if not directory.exists():
        return {"preview": [], "total_files": 0, "total_bytes": 0} if preview else {"removed": []}
    if not preview:
        if not token or not re.fullmatch(r"[A-Za-z0-9]{26}", token):
            raise RuntimeError("retention requires an exact helper authorization token")
        # Register BEFORE blocking on the ZIP lock, so cancellation can find
        # this exact process even when a backup creator owns the directory.
        retention_json(directory, ".retention-" + token + ".worker.json", {"pid": os.getpid(), "start": process_start("self")})
    removed = []
    lock_fd = os.open(directory / ".retention.lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not preview:
            control = retention_authorized(directory, token)
            if control is None:
                return {"removed": [], "stopped": True}
            protected = control["protected"]
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            full = []
            bases = set()
            names = os.listdir(fd)
            pattern = r"terrarium-backup-(\d{8}T\d{6}Z)-(full|diff)(?:-[2-9]\d*|-[1-9]\d+)?\.zip"
            for name in names:
                match = re.fullmatch(pattern, name)
                if not match:
                    continue
                source = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
                with os.fdopen(source, "rb") as stream:
                    if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                        continue
                    try:
                        with zipfile.ZipFile(stream) as archive:
                            entry = archive.getinfo("backup.json")
                            if entry.file_size > 65536:
                                continue
                            header = json.loads(archive.read(entry))
                        created = datetime.datetime.fromisoformat(header["created_at"].replace("Z", "+00:00"))
                        if (header["format"] != "terrarium-backup" or header["version"] != 1
                                or header["kind"] != match[2] or not created.tzinfo
                                or created.astimezone(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ") != match[1]):
                            continue
                        if match[2] == "diff":
                            bases.add(header["base"]["name"])
                        else:
                            full.append((created, name))
                    except (ValueError, KeyError, TypeError, zipfile.BadZipFile):
                        # Unknown archives are never permission to delete.
                        continue
            full.sort(reverse=True)
            keep = {name for _, name in full[:5]} | bases
            keep.update(Path(path).name for path in protected)
            days = set()
            for created, name in full:
                age = (now.date() - created.astimezone(datetime.timezone.utc).date()).days
                day = created.astimezone(datetime.timezone.utc).date()
                if 0 <= age < 14 and day not in days:
                    keep.add(name)
                    days.add(day)
                if created > now:
                    keep.add(name)
            audit_fd = os.open(".retention-audit.jsonl", os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, 0o600, dir_fd=fd)
            with os.fdopen(audit_fd, "a") as audit:
                for created, name in reversed(full):
                    if name in keep:
                        continue
                    pair = [name + ".hot-swap.sqlite", name]
                    # Bounded file deletion, never split a pair across passes.
                    if len(removed) + len(pair) > 32:
                        break
                    for item in pair:
                        if not preview:
                            control = retention_authorized(directory, token)
                            if control is None:
                                return {"removed": removed, "stopped": True}
                            if name in {Path(path).name for path in control["protected"]}:
                                break
                            if control.get("first_pass") is not None and item not in control["first_pass"]:
                                continue
                        try:
                            metadata = os.stat(item, dir_fd=fd, follow_symlinks=False)
                        except FileNotFoundError:
                            continue
                        if not stat.S_ISREG(metadata.st_mode):
                            raise RuntimeError("retention refuses non-regular file: " + item)
                        record = {"name": item, "size": metadata.st_size, "created_at": created.isoformat(), "at": now.isoformat(), "actor": "system:updater"}
                        if preview:
                            removed.append(record)
                            continue
                        audit.write(json.dumps({**record, "action": "backup.prune.intent"}) + "\n")
                        audit.flush()
                        os.fsync(audit.fileno())
                        # An audit fsync may stall past expiry. Check again at
                        # the unlink boundary, not just before journal IO.
                        if retention_authorized(directory, token) is None:
                            return {"removed": removed, "stopped": True}
                        os.unlink(item, dir_fd=fd)
                        os.fsync(fd)
                        audit.write(json.dumps({**record, "action": "backup.prune"}) + "\n")
                        audit.flush()
                        os.fsync(audit.fileno())
                        removed.append(record)
        finally:
            os.close(fd)
    if preview:
        return {"preview": removed, "total_bytes": sum(item["size"] for item in removed), "total_files": len(removed)}
    return {"removed": removed}


def retention_inventory():
    """Conservative recovery holds, never infer a leftover snapshot is unused."""
    directory = backup_directory()
    if not directory.exists():
        return {}
    fd = os.open(directory / ".retention.lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    holds = {}
    with os.fdopen(fd, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for snapshot in directory.iterdir():
            if not snapshot.name.endswith(".hot-swap.sqlite"):
                continue
            name = snapshot.name.removesuffix(".hot-swap.sqlite")
            if not re.fullmatch(r"terrarium-backup-\d{8}T\d{6}Z-full(?:-[2-9]\d*|-[1-9]\d+)?\.zip", name):
                raise RuntimeError("unknown recovery snapshot needs manual repair: " + snapshot.name)
            if not stat.S_ISREG(snapshot.lstat().st_mode):
                raise RuntimeError("non-regular recovery snapshot: " + snapshot.name)
            archive = directory / name
            if archive.exists() and not stat.S_ISREG(archive.lstat().st_mode):
                raise RuntimeError("non-regular recovery archive: " + name)
            holds["snapshot:" + name] = str(archive)
    return holds


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
    # Held bootstrap generations are owned by their host transaction. The
    # generic updater must not select, prune or remove their probation marker.
    if (os.path.lexists('/run/terrarium-bootstrap')
            or os.path.lexists(root / 'bootstrap-v1.json')
            or os.path.lexists(root / 'bootstrap-intent-v1.json')):
        raise RuntimeError('bootstrap generation is held by its host transaction')
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
    elif command == "retention-inventory":
        print(json.dumps(retention_inventory()))
    elif command == "retention-repair-revoke":
        try:
            control = retention_read(backup_directory(), ".retention-control.json")
        except FileNotFoundError:
            control = None
        if control is not None:
            retention_control("cancel", {"token": control["token"]})
        print("{}")
    elif command == "retention-control":
        print(json.dumps(retention_control(argv[1], json.loads(argv[2]))))
    elif command == "preview-backups":
        print(json.dumps(retain_backups(json.loads(argv[1]), preview=True)))
    elif command == "retain-backups":
        token = argv[1]
        result = retain_backups([], token=token)
        retention_json(backup_directory(), ".retention-" + token + ".receipt.json", result)
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
        (root / "probation").touch()
        select(root, root / key)
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

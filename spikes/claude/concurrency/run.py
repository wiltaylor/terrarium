# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Spike: two Claude Code agents at the same time on one shared home.

It drives the real daemon with sandbox mode bwrap: it runs the ignored test
`two_claude_agents_share_one_home` in crates/terrariumd/tests/e2e_claude.rs.
Four model turns (haiku): one turn each, a stop and a start of both (a real
`--resume`), one more turn each.

Before and after the run it takes a snapshot of the shared home. Then it
reports what changed there, and whether one agent's data got into the view
of the other one.

  uv run spikes/claude/concurrency/run.py [--home ~/.claude]
  uv run spikes/claude/concurrency/run.py --cleanup   # delete what the last run made

The snapshot keeps only names, sizes, and sha256 values. It does not copy
the content of credentials. Output: captured/report.json, captured/new-paths.txt.
"""
import argparse, hashlib, json, os, secrets, shutil, subprocess, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
CAP = HERE / "captured"

# Files whose content we compare by hash.
HASHED = [".credentials.json", ".claude.json"]
# Directories whose entry names we compare (recursive).
LISTED = ["sessions", "backups", "session-env", "shell-snapshots", "file-history",
          "todos", "statsig", "debug", "paste-cache", "projects/-home-agent-work"]


def sha(p: Path):
    try:
        return hashlib.sha256(p.read_bytes()).hexdigest()
    except FileNotFoundError:
        return None


def snapshot(home: Path) -> dict:
    snap = {"hash": {f: sha(home / f) for f in HASHED},
            "mtime": {f: (home / f).stat().st_mtime if (home / f).exists() else None
                      for f in HASHED},
            "top": sorted(p.name for p in home.iterdir()),
            "dirs": {}}
    for d in LISTED:
        root = home / d
        snap["dirs"][d] = sorted(str(p.relative_to(home)) for p in root.rglob("*")) \
            if root.is_dir() else []
    hist = home / "history.jsonl"
    snap["history_size"] = hist.stat().st_size if hist.exists() else None
    return snap


def grep(path: Path, needle: str) -> int:
    try:
        return path.read_text(errors="replace").count(needle)
    except FileNotFoundError:
        return 0


def run(home: Path) -> int:
    CAP.mkdir(exist_ok=True)
    nonce = secrets.token_hex(4)
    before = snapshot(home)
    subprocess.run(["cargo", "build", "-q", "-p", "terrarium"], cwd=REPO, check=True)
    report_path = CAP / "test-report.json"
    report_path.unlink(missing_ok=True)
    env = {**os.environ, "TERRARIUM_E2E_CLAUDE_HOME": str(home),
           "TERRARIUM_E2E_NONCE": nonce, "TERRARIUM_E2E_REPORT": str(report_path)}
    proc = subprocess.run(
        ["cargo", "test", "-q", "-p", "terrariumd", "--test", "e2e_claude", "--",
         "--ignored", "--exact", "two_claude_agents_share_one_home", "--nocapture"],
        cwd=REPO, env=env, capture_output=True, text=True)
    (CAP / "test-output.txt").write_text(proc.stdout + "\n--- stderr ---\n" + proc.stderr)
    after = snapshot(home)
    test = json.loads(report_path.read_text()) if report_path.exists() else {}

    new = {d: sorted(set(after["dirs"][d]) - set(before["dirs"][d])) for d in LISTED}
    new_top = sorted(set(after["top"]) - set(before["top"]))
    # The transcripts of the two sessions are expected in projects/.
    own_ids = [s for n in ("alpha", "beta") for s in test.get(n, {}).get("session_ids", [])[:1]]
    result = {
        "nonce": nonce,
        "test_exit": proc.returncode,
        "credentials_changed": before["hash"][".credentials.json"] != after["hash"][".credentials.json"],
        "shared_claude_json_changed": before["hash"][".claude.json"] != after["hash"][".claude.json"],
        # The operator may use Claude Code at the same time, so a size change
        # is not proof. The nonce in the shared file is.
        "shared_history_size_delta": (after["history_size"] or 0) - (before["history_size"] or 0),
        "shared_history_has_nonce": grep(home / "history.jsonl", nonce),
        "new_top_level": new_top,
        "new_in_dirs": {d: v for d, v in new.items() if v},
        "transcripts_for_own_sessions": [
            f"projects/-home-agent-work/{s}.jsonl" in after["dirs"]["projects/-home-agent-work"]
            for s in own_ids],
        "test": test,
    }
    (CAP / "report.json").write_text(json.dumps(result, indent=2))
    # The paths this run made in the shared home, for --cleanup: new paths
    # that name one of the two sessions, that are under the sandbox project
    # directory, or (shell snapshots) that hold the sandbox home. The
    # operator's own Claude sessions can write new files at the same time;
    # they are "unattributed" and stay.
    all_ids = {s for n in ("alpha", "beta") for s in test.get(n, {}).get("session_ids", [])}

    def ours(rel: str) -> bool:
        if any(i in rel for i in all_ids):
            return True
        if rel.startswith("projects/-home-agent-work/"):
            return True
        if rel.startswith("shell-snapshots/"):
            return "/home/agent" in (home / rel).read_text(errors="replace")
        return False

    candidates = new_top + [p for v in new.values() for p in v]
    paths = [p for p in candidates if ours(p)]
    result["attributed_to_this_run"] = paths
    result["unattributed_new_paths"] = [p for p in candidates if p not in paths]
    (CAP / "report.json").write_text(json.dumps(result, indent=2))
    (CAP / "new-paths.txt").write_text("\n".join(paths) + ("\n" if paths else ""))

    print(json.dumps({k: v for k, v in result.items() if k != "test"}, indent=2))
    for n in ("alpha", "beta"):
        t = test.get(n, {})
        print(f"{n}: sessions={t.get('session_ids')} sources={t.get('sources')} "
              f"answer={t.get('answer_after_resume')!r} history_lines={t.get('history_lines')} "
              f"own_token={t.get('history_has_own_token')} other_token={t.get('history_has_other_token')} "
              f"numStartups={t.get('state_num_startups')} backups={t.get('backups_dir')} "
              f"memory={t.get('memory_dir')} sessions_dir={t.get('sessions_dir')}")
    if proc.returncode != 0:
        print(f"the test failed; see {CAP / 'test-output.txt'}", file=sys.stderr)
    return proc.returncode


def cleanup(home: Path) -> int:
    listed = CAP / "new-paths.txt"
    paths = [l for l in listed.read_text().splitlines() if l.strip()] if listed.exists() else []
    # Delete the deepest paths first; a parent directory goes only when empty
    # or when it is itself new (then all of its content is new too).
    for rel in sorted(paths, key=lambda p: -p.count("/")):
        p = home / rel
        if p.is_symlink() or p.is_file():
            p.unlink()
        elif p.is_dir():
            shutil.rmtree(p)
        else:
            continue
        print(f"removed {p}")
    return 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--home", default=str(Path.home() / ".claude"))
    ap.add_argument("--cleanup", action="store_true")
    a = ap.parse_args()
    home = Path(a.home).expanduser()
    sys.exit(cleanup(home) if a.cleanup else run(home))

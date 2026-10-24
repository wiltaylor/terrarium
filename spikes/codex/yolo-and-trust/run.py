# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spikes 2 and 6: yolo flags, start-up dialogs, quiet start. No turns.

Each variant starts codex in a fresh CODEX_HOME (auth.json only) and records
the first stable screen, the time to the idle composer, and the processes
left behind. Variants (pass names to run a subset):
  plain-nongit    no flags, cwd not a git repo
  plain-git       no flags, git cwd
  yolo-git        --dangerously-bypass-approvals-and-sandbox, git cwd
  never-full      -a never -s danger-full-access, git cwd, trust by -c
  trust-c         yolo + trust by -c projects.<cwd>.trust_level
  hooks-notrust   yolo + trust + -c hooks, no --dangerously-bypass-hook-trust
  hooks-warning   yolo + trust + an unknown hook event; press F2 to read the warning
  update-banner   version.json says a newer version exists
  update-off      same + -c check_for_update_on_startup=false
  daemon          yolo only, no -c: the app-server daemon starts and stays
  daemon-with-c   yolo + one -c override, no --no-daemon: no daemon
  strict          --strict-config with the quiet keys (are they known keys?)
"""
import json, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
ALL = ["plain-nongit", "plain-git", "yolo-git", "never-full", "trust-c", "hooks-notrust",
       "hooks-warning", "update-banner", "update-off", "daemon", "daemon-with-c", "strict"]
want = sys.argv[1:] or ALL
QUIET = ["-c", "check_for_update_on_startup=false", "-c", "analytics.enabled=false",
         "-c", "feedback.enabled=false"]
YOLO = ["--dangerously-bypass-approvals-and-sandbox"]
results = {}


def git_cwd():
    d = make_cwd(); subprocess.run(["git", "init", "-q", str(d)], check=True); return d


def procs_with(fragment):
    n = []
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                c = (p / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
            except OSError:
                continue
            if fragment in c and "run.py" not in c:
                n.append(c.strip()[:160])
    return n


def run(name, args, cwd, home, keys=(), wait=8.0):
    s = start(args, cwd, home)
    t_box = wait_until(lambda: screen_state(s.lines())["input_box"], wait)
    s.wait_stable(quiet=1.0, timeout=wait)
    first = s.text()
    for k, delay in keys:
        s.send(k); time.sleep(delay)
    after = s.text() if keys else None
    st = screen_state(s.lines())
    s.send("\x03"); time.sleep(0.3); s.send("\x03")
    wait_exit(s, 5); s.close()
    time.sleep(1.0)
    left = procs_with(str(home))
    daemon_dir = (home / "app-server-daemon").exists()
    with (CAP / f"screen-{name}.txt").open("w") as f:
        f.write(sanitise_all(first.rstrip()) + "\n")
        if after:
            f.write("\n===== after keys =====\n" + sanitise_all(after.rstrip()) + "\n")
    r = {"args": sanitise_all(" ".join(a for a in args if not a.startswith("hooks.")).replace(str(cwd), "<cwd>")),
         "composer_after_s": round(t_box, 2) if t_box else None,
         "detector": {k: st[k] for k in ("input_box", "busy", "dialog")},
         "first_screen_tail": [sanitise_all(l) for l in first.splitlines() if l.strip()][-8:],
         "processes_left": [sanitise_all(p) for p in left], "daemon_dir": daemon_dir}
    stop_daemons(home)
    results[name] = r
    log(name, json.dumps(r)[:300])


for v in want:
    home = make_codex_home()
    logf = home / "hooks.jsonl"
    if v == "plain-nongit":
        c = make_cwd(); run(v, ["--no-daemon"], c, home)
    elif v == "plain-git":
        c = git_cwd(); run(v, ["--no-daemon"], c, home)
    elif v == "yolo-git":
        c = git_cwd(); run(v, ["--no-daemon", *YOLO], c, home)
    elif v == "never-full":
        c = git_cwd(); run(v, ["--no-daemon", "-a", "never", "-s", "danger-full-access", *trust_override(c)], c, home)
    elif v == "trust-c":
        c = git_cwd(); run(v, ["--no-daemon", *YOLO, *trust_override(c), *QUIET], c, home)
    elif v == "hooks-notrust":
        c = git_cwd(); run(v, ["--no-daemon", *YOLO, *trust_override(c), *QUIET, *hook_overrides(logf)], c, home)
    elif v == "hooks-warning":
        c = git_cwd(); run(v, ["--no-daemon", *YOLO, *trust_override(c), *QUIET, "--dangerously-bypass-hook-trust",
                               *hook_overrides(logf, ["Stop", "Notification"])], c, home,
                           keys=[(b"\x1bOQ", 1.5)])
    elif v in ("update-banner", "update-off"):
        (home / "version.json").write_text(json.dumps({"latest_version": "9.9.9",
            "last_checked_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "dismissed_version": None}))
        c = git_cwd()
        extra = ["-c", "check_for_update_on_startup=false"] if v == "update-off" else []
        run(v, ["--no-daemon", *YOLO, *trust_override(c), *extra], c, home)
    elif v == "daemon":
        # Any -c/--enable override makes codex run embedded. Only a start with
        # no config override spawns the shared app-server daemon.
        c = make_cwd(); run(v, [*YOLO], c, home)
        results[v]["packages_mb"] = round(sum(f.stat().st_size for f in (home / "packages").rglob("*") if f.is_file()) / 1e6) if (home / "packages").exists() else 0
    elif v == "daemon-with-c":
        c = make_cwd(); run(v, [*YOLO, "-c", 'model_reasoning_effort="low"'], c, home)
    elif v == "strict":
        # --strict-config only works for the TUI and exec. With stdin not a
        # TTY the TUI stops after the config loads ("stdin is not a
        # terminal"), so a config error shows first if there is one.
        c = git_cwd()
        for kv in ["no_such_key=1", "check_for_update_on_startup=false", "analytics.enabled=false",
                   "feedback.enabled=false", 'tui.resume_cwd="session"', 'developer_instructions="x"',
                   'model_instructions_file="/nonexistent/x.md"',
                   'hooks.Notification=[{hooks=[{type="command",command="true"}]}]']:
            p = subprocess.run([CODEX, "--no-daemon", "--strict-config", "-c", kv], env=codex_env(home),
                               stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=30, cwd=c)
            err = [l for l in (p.stdout + p.stderr).splitlines() if "PATH aliases" not in l]
            results.setdefault(v, {})[kv] = {"rc": p.returncode, "out": sanitise_all("\n".join(err))[-300:]}
    files_after = sorted(str(p.relative_to(home)) for p in home.iterdir())
    results.setdefault(v, {})["home_files_after"] = files_after
    cfg = home / "config.toml"
    results[v]["config_toml_after"] = cfg.read_text() if cfg.exists() else None

old = json.loads((CAP / "results.json").read_text()) if (CAP / "results.json").exists() and len(want) < len(ALL) else {}
old.update(results)
(CAP / "results.json").write_text(json.dumps(old, indent=2))

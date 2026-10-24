# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spikes 2 and 6: yolo, folder trust, quiet start. No model turns.

Fresh COPILOT_HOME for each variant (nothing copied: the token is in the
keychain). The first stable screen goes to captured/screen-<variant>.txt;
"dialog" in the name means a dialog is the expected first screen.
  dialog-plain        no flags, no env, git cwd
  dialog-yolo-only    --yolo, no COPILOT_ALLOW_ALL
  dialog-allowall-1   --yolo, COPILOT_ALLOW_ALL=1 (truthy, but not "true")
  idle-allowall-true  COPILOT_ALLOW_ALL=true, no --yolo
  idle-trusted-config --yolo, config.json trustedFolders = [cwd]
  idle-nongit         --yolo, COPILOT_ALLOW_ALL=true, cwd not a git repo
  idle-autoupdate-on  like idle-allowall-true but auto-update on: what lands in the home?
  ctrlc               Ctrl-C once, then again: does it exit?
"""
import json, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
ALL = ["dialog-plain", "dialog-yolo-only", "dialog-allowall-1", "idle-allowall-true",
       "idle-trusted-config", "idle-nongit", "idle-autoupdate-on", "ctrlc"]
want = sys.argv[1:] or ALL
results = {}
QUIET = ["--disable-client-telemetry", "--no-auto-update"]


def git_cwd():
    d = make_cwd(); subprocess.run(["git", "init", "-q", str(d)], check=True); return d


for v in want:
    home = make_copilot_home()
    cwd = make_cwd() if v == "idle-nongit" else git_cwd()
    env = {"COPILOT_HOME": str(home), "COPILOT_AUTO_UPDATE": "false"}
    args = list(QUIET) + ["--model", MODEL]
    if v == "dialog-plain":
        args = ["--model", MODEL]; env = {"COPILOT_HOME": str(home)}
    elif v == "dialog-yolo-only":
        args += ["--yolo"]
    elif v == "dialog-allowall-1":
        args += ["--yolo"]; env["COPILOT_ALLOW_ALL"] = "1"
    elif v in ("idle-allowall-true", "ctrlc"):
        env["COPILOT_ALLOW_ALL"] = "true"
    elif v == "idle-trusted-config":
        args += ["--yolo"]
        (home / "config.json").write_text(json.dumps({"trustedFolders": [str(cwd)]}))
    elif v == "idle-nongit":
        args += ["--yolo"]; env["COPILOT_ALLOW_ALL"] = "true"
    elif v == "idle-autoupdate-on":
        args = ["--model", MODEL]; env = {"COPILOT_HOME": str(home), "COPILOT_ALLOW_ALL": "true"}
    s = Session(args, cwd, harness_env(env), binary=COPILOT)
    t_ready = wait_until(lambda: state(s)["ready"] or state(s)["dialog"], 40, poll=0.02)
    s.wait_stable(quiet=1.5, timeout=15)
    if v == "idle-autoupdate-on":
        time.sleep(15)
    first = s.text(); st = state(s)
    r = {"first_ready_or_dialog_s": round(t_ready, 2) if t_ready else None,
         "ready": st["ready"], "dialog": st["dialog"], "footer": st["footer"]}
    if v == "ctrlc":
        s.send("\x03"); time.sleep(1.0)
        r["after_first_ctrl_c"] = {"alive": s.alive, "tail": [l for l in s.text().splitlines() if l.strip()][-3:]}
        s.send("\x03"); r["exit_after_second_s"] = wait_exit(s, 8)
        r["exit_screen_tail"] = [l for l in s.text().splitlines() if l.strip()][-6:]
    else:
        s.send("\x1b"); time.sleep(0.3)
        s.send("\x03"); time.sleep(0.3); s.send("\x03"); wait_exit(s, 5)
    s.close()
    (CAP / f"screen-{v}.txt").write_text(sanitise_all(first.rstrip()) + "\n")
    cfg = home / "config.json"
    r["home_files"] = sorted(str(p.relative_to(home)) for p in home.rglob("*") if p.is_file() and "logs/" not in str(p))[:30]
    r["config_json_keys"] = sorted(json.loads("\n".join(l for l in cfg.read_text().splitlines() if not l.lstrip().startswith("//"))).keys()) if cfg.exists() else None
    r["pkg_dir"] = sorted(str(p.relative_to(home)) for p in (home / "pkg").glob("*/*")) if (home / "pkg").exists() else []
    results[v] = {k: sanitise_all(x) if isinstance(x, str) else x for k, x in r.items()}
    log(v, json.dumps(results[v])[:260])

old = json.loads((CAP / "results.json").read_text()) if (CAP / "results.json").exists() and len(want) < len(ALL) else {}
old.update(results)
(CAP / "results.json").write_text(sanitise_all(json.dumps(old, indent=2)))

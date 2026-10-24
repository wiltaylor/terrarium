# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 1: which hooks fire in an interactive PTY session, with payloads.

One model turn (haiku). Hooks come from --settings only, with
--setting-sources "" so no user or project hooks load.
Steps: start -> wait for idle prompt -> paste a prompt that makes one Bash
call -> wait for Stop -> stay idle 70 s (Notification?) -> /exit (SessionEnd).
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
IDLE_WAIT = float(sys.argv[1]) if len(sys.argv) > 1 else 70

cwd = make_cwd()
cfg = make_config_dir(None, trust=False, onboarding=False)
(cfg / ".claude.json").write_text(json.dumps({
    "hasCompletedOnboarding": True,
    "projects": {str(cwd): {"hasTrustDialogAccepted": True}}}))
logf = cfg / "hooks.jsonl"
settings = {"skipDangerousModePermissionPrompt": True, **hook_settings(logf)}
sid = new_uuid()
s = Session(["--dangerously-skip-permissions", "--model", "haiku",
             "--setting-sources", "", "--settings", json.dumps(settings),
             "--session-id", sid], cwd, clean_env(cfg))
screens = CAP / "screens.txt"; screens.write_text("")

ready = s.wait_text(r"bypass permissions on", 30)
s.wait_stable(quiet=1.0, timeout=10)
log("prompt ready after", round(time.time() - s.t0, 2), "s")
snap(s, screens, "idle-before-turn")

t_send = time.time()
s.paste("Use the Bash tool to run: echo terrarium-hook-test. Then reply with exactly: done")
time.sleep(0.15)
s.send("\r")

# Snap the screen while it works, until Stop arrives.
deadline = time.time() + 120
n = 0
while time.time() < deadline:
    names = [h["hook"] for h in read_hooks(logf)]
    if "Stop" in names:
        break
    if n % 5 == 0:
        snap(s, screens, f"working-{n}")
    n += 1
    time.sleep(0.3)
s.wait_stable(quiet=1.0, timeout=10)
snap(s, screens, "idle-after-stop")
log("Stop seen; idling", IDLE_WAIT, "s to see Notification")
time.sleep(IDLE_WAIT)
snap(s, screens, "after-idle-wait")

t_exit = time.time()
s.send("/exit")
time.sleep(0.3)
s.send("\r")
for _ in range(100):
    if not s.alive:
        break
    time.sleep(0.1)
log("exited cleanly via /exit:", not s.alive, "after", round(time.time() - t_exit, 2), "s")
time.sleep(1.5)  # SessionEnd hook may finish after the TUI exits
snap(s, screens, "after-exit")
s.close()

hooks = read_hooks(logf)
seq = [(h["hook"], round(h["ts"] - t_send, 3)) for h in hooks]
log("sequence (s relative to the Enter keystroke):")
for name, dt in seq:
    print(f"   {dt:8.3f}  {name}")
# Sanitised payloads, one example per event plus the full log.
(CAP / "hooks.jsonl").write_text(sanitise("\n".join(json.dumps(h) for h in hooks)) + "\n")
shapes = {}
for h in hooks:
    p = h["payload"]
    shapes.setdefault(h["hook"], {k: type(v).__name__ for k, v in p.items()})
(CAP / "payload-shapes.json").write_text(json.dumps(shapes, indent=2))
screens.write_text(sanitise(screens.read_text()))
print(json.dumps({"session_id": sid, "exit_clean": not s.alive,
                  "events": sorted(shapes)}, indent=2))

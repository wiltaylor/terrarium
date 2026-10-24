# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 8: what /status shows about quota. No model turns.

/status is a local TUI command. It runs in a fresh session (no turn sent).
The rate-limit numbers in the session file are in
session-id-and-resume/captured/session.jsonl (token_count events).
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
cwd = make_cwd(); home = make_codex_home()
s = start(base_args(cwd), cwd, home)
wait_ready(s, 30, 1.0)
s.send("/status"); time.sleep(0.5); s.send("\r")
time.sleep(6); s.wait_stable(1.5, 15)
txt = s.text()
(CAP / "status-screen.txt").write_text(sanitise_all(txt) + "\n")
print(sanitise_all(txt))
s.send("\x1b"); time.sleep(0.3)
s.send("/exit"); time.sleep(0.3); s.send("\r"); wait_exit(s, 10); s.close()
print("session files after /status:", list(home.glob("sessions/**/*.jsonl")))
stop_daemons(home)

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 8: what /usage shows about quota. No model turns.

/usage is a local TUI command; it runs in a fresh session with no turn sent.
Per-call usage is in session-id-and-resume/captured/events.jsonl.
"""
import sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
home = make_copilot_home(); cwd = make_cwd()
s = start(base_args(), cwd, home)
wait_ready(s, 40, 1.0)
s.send("/usage"); time.sleep(0.6); s.send("\r"); time.sleep(5); s.wait_stable(1.5, 15)
txt = sanitise_all(s.text())
(CAP / "usage-screen.txt").write_text(txt + "\n")
print(txt)
s.send("\x1b"); time.sleep(0.3); s.send("\x03"); time.sleep(0.3); s.send("\x03"); wait_exit(s, 5); s.close()

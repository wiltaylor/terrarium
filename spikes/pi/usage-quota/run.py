# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 8: what pi shows about usage or quota. No model turns.

pi has no provider quota display. It shows per-session tokens and cost in
the footer and in /session. This runs /session in a fresh session (no turn)
and records the footer. Per-message usage is in
session-id-and-resume/captured/session.jsonl.
"""
import sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
d = make_pi_dir(); cwd = make_cwd()
s = start(base_args(ext=False) + ["--no-skills"], cwd, d)
s.wait_stable(1.0, 15)
s.send("/session"); time.sleep(0.5); s.send("\r"); time.sleep(2); s.wait_stable(1.0, 10)
txt = sanitise_all(s.text())
(CAP / "session-command-screen.txt").write_text(txt + "\n")
print(txt)
s.send("\x1b"); time.sleep(0.3); s.send("\x04"); wait_exit(s, 5); s.close()

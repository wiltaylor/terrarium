# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 7: how to tell "logged in". No model turns. Never runs login.

Copilot 1.0.103 has no `auth status` command. The token is in the system
keychain (libsecret over D-Bus), with a plain-text fallback in the config.
 A normal env, temp COPILOT_HOME: `/user` in the TUI.
 B the keychain made unreachable (DBUS_SESSION_BUS_ADDRESS to a dead path):
   what the TUI shows when it finds no token.
 C `copilot help environment` lines about tokens.
"""
import json, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
out = {}
for name, extra, keys in [("A-user", {}, [("/user", 0.5), ("\r", 4)]),
                          ("B-no-keychain", {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent/bus"}, [])]:
    home = make_copilot_home(); cwd = make_cwd()
    s = start(base_args(), cwd, home, env_extra=extra)
    time.sleep(8); s.wait_stable(1.5, 10)
    for k, d in keys:
        s.send(k); time.sleep(d)
    s.wait_stable(1.5, 10)
    txt = sanitise_all(s.text())
    (CAP / f"screen-{name}.txt").write_text(txt + "\n")
    out[name] = {"alive": s.alive, "tail": [l for l in txt.splitlines() if l.strip()][-14:]}
    s.send("\x03"); time.sleep(0.3); s.send("\x03"); wait_exit(s, 5); s.close()
p = subprocess.run([COPILOT, "help", "environment"], capture_output=True, text=True, timeout=30)
out["C-help-environment-token-lines"] = [l.strip()[:400] for l in p.stdout.splitlines() if "TOKEN" in l]
(CAP / "results.json").write_text(json.dumps(out, indent=2))
print(json.dumps(out, indent=2))

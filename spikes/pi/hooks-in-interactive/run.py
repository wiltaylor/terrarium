# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 1: the Terrarium extension's events in an interactive session.

One model turn: openai-codex/gpt-5.6-luna with a copy of Codex's access
token (see pilib; refresh left empty). The extension
(terrarium-ext/terrarium.ts, loaded with -e) posts one JSON line per event to
a Unix socket that this script listens on. Also answers: does pi ask before
it runs a tool (no yolo flag is passed)?
Steps: start -> idle -> one prompt with a bash call -> agent_settled ->
idle IDLE_WAIT s -> Ctrl-D (exit).
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
IDLE_WAIT = float(sys.argv[1]) if len(sys.argv) > 1 else 10
screens = CAP / "screens.txt"; screens.write_text("")
d = make_pi_dir(); cwd = make_cwd(); sockp = d / "ev.sock"; es = EventSocket(sockp)
s = start(base_args() + ["--no-skills"], cwd, d, sockp)
wait_until(lambda: "session_start" in es.names(), 30)
s.wait_stable(1.0, 15)
snap(s, screens, "idle-before-turn")
at_start = es.names()

t_send = time.time()
s.paste("Use the bash tool to run: echo terrarium-hook-test. Then reply with exactly: done")
s.send("\r")
n = 0
while time.time() - t_send < 180 and "agent_settled" not in es.names():
    if n % 8 == 0:
        snap(s, screens, f"working-{n}")
    n += 1; time.sleep(0.25)
time.sleep(1); s.wait_stable(1.0, 10)
snap(s, screens, "idle-after-settled")
time.sleep(IDLE_WAIT)
t_exit = time.time()
s.send("\x04")
dt = wait_exit(s, 10); time.sleep(0.5)
snap(s, screens, "after-ctrl-d"); s.close()

evs = es.events
seq = [(e["hook_event_name"], round(e["_rx"] - t_send, 3)) for e in evs]
for name, t in seq:
    print(f"   {t:8.3f}  {name}")
shapes = {}
for e in evs:
    shapes.setdefault(e["hook_event_name"], {k: type(v).__name__ for k, v in e.items()})
(CAP / "events.jsonl").write_text(sanitise_all("\n".join(json.dumps(e) for e in evs)) + "\n")
(CAP / "payload-shapes.json").write_text(json.dumps(shapes, indent=2))
screens.write_text(sanitise_all(screens.read_text()))
asked = any(w in screens.read_text() for w in ("Allow", "approve", "Approve", "(y/n)", "Proceed?"))
res = {"events_before_first_turn": at_start, "sequence": seq, "exit_ctrl_d_s": dt,
       "approval_prompt_seen": asked,
       "session_files": sorted(str(p.relative_to(d)) for p in d.rglob("*.jsonl"))}
(CAP / "results.json").write_text(sanitise_all(json.dumps(res, indent=2)))
print(sanitise_all(json.dumps(res, indent=2)))

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 1: hooks in an interactive PTY session, with payloads.

One model turn (gpt-6-luna, effort low). Hooks come only from -c overrides
("session flags" source) plus --dangerously-bypass-hook-trust. The temp
CODEX_HOME has auth.json and no config.toml.
Steps: start -> idle prompt -> paste a prompt that runs one shell command ->
wait for Stop -> idle IDLE_WAIT s -> /exit (SessionEnd?).
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
IDLE_WAIT = float(sys.argv[1]) if len(sys.argv) > 1 else 20
screens = CAP / "screens.txt"; screens.write_text("")

cwd = make_cwd()
home = make_codex_home()
logf = home / "hooks.jsonl"
# Extra unknown event to see how codex treats it.
args = base_args(cwd, logf) + hook_overrides(logf, ["Notification"])
s = start(args, cwd, home)
t_ready = wait_until(lambda: screen_state(s.lines())["input_box"], 40)
s.wait_stable(quiet=1.0, timeout=10)
log("prompt ready after", round(time.time() - s.t0, 2), "s")
snap(s, screens, "idle-before-turn")
hooks_at_start = [h["hook"] for h in read_hooks(logf)]

t_send = time.time()
s.paste("Run the shell command `echo terrarium-hook-test`. Then reply with exactly: done")
s.send("\r")
n = 0
while time.time() - t_send < 150:
    if "Stop" in [h["hook"] for h in read_hooks(logf)]:
        break
    if n % 6 == 0:
        snap(s, screens, f"working-{n}")
    n += 1
    time.sleep(0.25)
s.wait_stable(quiet=1.0, timeout=10)
snap(s, screens, "idle-after-stop")
log("Stop seen; idling", IDLE_WAIT, "s")
time.sleep(IDLE_WAIT)
snap(s, screens, "after-idle-wait")

t_exit = time.time()
s.send("/exit"); time.sleep(0.3); s.send("\r")
dt = wait_exit(s, 15)
log("exit via /exit:", dt is not None, "after", dt)
time.sleep(1.5)
snap(s, screens, "after-exit")
s.close()

hooks = read_hooks(logf)
seq = [(h["hook"], round(h["ts"] - t_send, 3)) for h in hooks]
for name, d in seq:
    print(f"   {d:8.3f}  {name}")
(CAP / "hooks.jsonl").write_text(sanitise_all("\n".join(json.dumps(h) for h in hooks)) + "\n")
shapes = {}
for h in hooks:
    shapes.setdefault(h["hook"], {k: type(v).__name__ for k, v in h["payload"].items()})
(CAP / "payload-shapes.json").write_text(json.dumps(shapes, indent=2))
screens.write_text(sanitise_all(screens.read_text()))
res = {"ready_s": round(t_ready or -1, 2), "hooks_before_first_turn": hooks_at_start,
       "sequence": seq, "exit_s": dt, "events": sorted(shapes),
       "session_files": sorted(str(p.relative_to(home)) for p in home.rglob("*.jsonl") if "sessions" in str(p))}
(CAP / "results.json").write_text(sanitise_all(json.dumps(res, indent=2)))
print(json.dumps(res, indent=2))
stop_daemons(home)

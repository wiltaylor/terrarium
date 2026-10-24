# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 1: hook routes and payloads in an interactive PTY session.

One model turn (claude-haiku-4.5, 0.33x). Four routes at once, each logging
with its own label ("<route>:<Event>"):
  homedir   $COPILOT_HOME/hooks/terrarium.json        (PascalCase + camelCase)
  settings  $COPILOT_HOME/settings.json "hooks"       (PascalCase)
  plugin    --plugin-dir <dir> with hooks/hooks.json and hooks.json
  repo      <cwd>/.github/hooks/terrarium.json        (needs folder trust)
COPILOT_ALLOW_ALL=true and --yolo. Steps: start -> idle -> one prompt with a
shell call -> Stop -> idle IDLE_WAIT s -> /exit.
"""
import json, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
IDLE_WAIT = float(sys.argv[1]) if len(sys.argv) > 1 else 20
screens = CAP / "screens.txt"; screens.write_text("")
ALL = HOOK_EVENTS + HOOK_EVENTS_CAMEL


def hk(route, events):
    return {"version": 1, "hooks": {ev: [{"type": "command", "bash": hook_cmd(f"{route}:{ev}", LOG),
                                          "timeoutSec": 10}] for ev in events}}


cwd = make_cwd(); subprocess.run(["git", "init", "-q", str(cwd)], check=True)
home = make_copilot_home(); LOG = home / "hooks.jsonl"
(home / "hooks").mkdir()
(home / "hooks/terrarium.json").write_text(json.dumps(hk("homedir", ALL), indent=1))
(home / "settings.json").write_text(json.dumps({"hooks": hk("settings", HOOK_EVENTS)["hooks"]}, indent=1))
plug = make_cwd()
for p in ["plugin.json", ".claude-plugin/plugin.json"]:
    (plug / p).parent.mkdir(parents=True, exist_ok=True)
    (plug / p).write_text(json.dumps({"name": "terrarium-spike", "version": "0.0.1", "description": "spike"}))
(plug / "hooks").mkdir()
(plug / "hooks/hooks.json").write_text(json.dumps(hk("plugin-hooksdir", HOOK_EVENTS)))
(plug / "hooks.json").write_text(json.dumps(hk("plugin-root", HOOK_EVENTS)))
(cwd / ".github/hooks").mkdir(parents=True)
(cwd / ".github/hooks/terrarium.json").write_text(json.dumps(hk("repo", HOOK_EVENTS)))

s = start(base_args() + ["--plugin-dir", str(plug)], cwd, home)
s.wait_text(r"Allow All", 40); s.wait_stable(1.0, 10)
log("prompt ready after", round(time.time() - s.t0, 2), "s")
snap(s, screens, "idle-before-turn")
at_start = [h["hook"] for h in read_hooks(LOG)]

t_send = time.time()
s.paste("Run the shell command `echo terrarium-hook-test`. Then reply with exactly: done")
s.send("\r")
n = 0
while time.time() - t_send < 150:
    if any(h["hook"].endswith(("Stop", "agentStop")) and "Subagent" not in h["hook"] and "subagent" not in h["hook"]
           for h in read_hooks(LOG)):
        break
    if n % 6 == 0:
        snap(s, screens, f"working-{n}")
    n += 1
    time.sleep(0.25)
time.sleep(2); s.wait_stable(1.0, 10)
snap(s, screens, "idle-after-stop")
time.sleep(IDLE_WAIT)
snap(s, screens, "after-idle-wait")
s.send("/exit"); time.sleep(0.4); s.send("\r")
dt = wait_exit(s, 15); time.sleep(2)
snap(s, screens, "after-exit"); s.close()

hooks = read_hooks(LOG)
seq = [(h["hook"], round(h["ts"] - t_send, 3)) for h in hooks]
for name, d in seq:
    print(f"   {d:8.3f}  {name}")
routes = {}
for h in hooks:
    r, ev = h["hook"].split(":", 1)
    routes.setdefault(r, []).append(ev)
shapes = {}
for h in hooks:
    shapes.setdefault(h["hook"].split(":", 1)[1], {k: type(v).__name__ for k, v in h["payload"].items()})
(CAP / "hooks.jsonl").write_text(sanitise_all("\n".join(json.dumps(h) for h in hooks)) + "\n")
(CAP / "payload-shapes.json").write_text(json.dumps(shapes, indent=2))
screens.write_text(sanitise_all(screens.read_text()))
res = {"hooks_before_first_turn": at_start, "routes_that_fired": routes, "exit_s": dt,
       "home_tree": sorted(str(p.relative_to(home)) for p in home.rglob("*") if p.is_file())[:80]}
(CAP / "results.json").write_text(sanitise_all(json.dumps(res, indent=2)))
print(sanitise_all(json.dumps(res, indent=2)))

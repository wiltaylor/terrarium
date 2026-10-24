# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 3: set the session id, resume, and the session file.

Two model turns (claude-haiku-4.5).
 A --session-id <new uuid>, T1 "Reply with exactly: pong", /exit.
 B --resume <uuid>, T2 asks for the earlier word. SessionStart.source?
 C --session-id <same uuid> again (no turn): resume or error?
 D two starts with --resume <uuid> at once (no turn).
 E --resume <random uuid> (no turn).
 F --resume <uuid> from another cwd (no turn).
The session file is copied (sanitised) to captured/events.jsonl.
"""
import json, subprocess, sys, time, uuid
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
screens = CAP / "screens.txt"; screens.write_text("")
cwd = make_cwd(); cwd2 = make_cwd()
LOG = None
home = make_copilot_home()
LOG = home / "hooks.jsonl"
(home / "hooks").mkdir()
(home / "hooks/terrarium.json").write_text(json.dumps(hooks_json(LOG)))
out = {}


def hooks(name):
    return [h for h in read_hooks(LOG) if h["hook"] == name]


def ready(s, t=40):
    s.wait_text(r"Allow All", t); s.wait_stable(1.0, 10)


def turn(s, text, timeout=120):
    n = len(hooks("Stop")); ready(s)
    s.paste(text); s.send("\r")
    wait_until(lambda: len(hooks("Stop")) > n, timeout); time.sleep(1.5)


def exit_(s, label):
    s.send("/exit"); time.sleep(0.4); s.send("\r")
    dt = wait_exit(s, 15); time.sleep(1.0); snap(s, screens, label); s.close(); return dt


def last_reply(sid):
    ev = home / "session-state" / sid / "events.jsonl"
    recs = [json.loads(l) for l in ev.read_text().splitlines() if l.strip()]
    msgs = [r for r in recs if r.get("type") == "assistant.message"]
    return (msgs[-1].get("data") or {}).get("content") if msgs else None


sid = str(uuid.uuid4())
s = start(base_args() + ["--session-id", sid], cwd, home)
turn(s, "Reply with exactly: pong")
out["A_hook_session_id_matches"] = hooks("SessionStart")[0]["payload"]["session_id"] == sid
out["A_SessionStart"] = hooks("SessionStart")[0]["payload"]
out["A_reply"] = last_reply(sid)
out["A_exit_s"] = exit_(s, "A-after-exit")

n_ss = len(hooks("SessionStart"))
s = start(base_args() + ["--resume", sid], cwd, home)
ready(s); snap(s, screens, "B-resumed-idle")
turn(s, "What exact word did you reply with in your previous message? Reply with that word only.")
out["B_SessionStart"] = [h["payload"] for h in hooks("SessionStart")[n_ss:]]
out["B_reply"] = last_reply(sid)
out["B_exit_s"] = exit_(s, "B-after-exit")

for label, args, c in [("C-session-id-again", ["--session-id", sid], cwd),
                       ("E-bogus-resume", ["--resume", str(uuid.uuid4())], cwd),
                       ("F-resume-other-cwd", ["--resume", sid], cwd2)]:
    s = start(base_args() + args, c, home)
    time.sleep(7); snap(s, screens, label)
    out[label] = {"alive": s.alive, "tail": [l for l in s.text().splitlines() if l.strip()][-8:]}
    s.send("\x03"); time.sleep(0.3); s.send("\x03"); wait_exit(s, 5); s.close()

s1 = start(base_args() + ["--resume", sid], cwd, home); time.sleep(6)
s2 = start(base_args() + ["--resume", sid], cwd, home); time.sleep(7)
snap(s1, screens, "D-first"); snap(s2, screens, "D-second")
out["D_second"] = {"alive": s2.alive, "tail": [l for l in s2.text().splitlines() if l.strip()][-8:]}
for x in (s2, s1):
    x.send("\x03"); time.sleep(0.3); x.send("\x03"); wait_exit(x, 5); x.close()

sd = home / "session-state" / sid
out["session_dir_files"] = sorted(str(p.relative_to(sd)) for p in sd.rglob("*") if p.is_file())
recs = [json.loads(l) for l in (sd / "events.jsonl").read_text().splitlines() if l.strip()]
kinds = {}
for r in recs:
    kinds[r.get("type")] = kinds.get(r.get("type"), 0) + 1
out["record_types"] = kinds
out["usage_like"] = [r for r in recs if any(k in json.dumps(r) for k in ("inputTokens", "input_tokens", "usage"))][:6]
(CAP / "events.jsonl").write_text(sanitise_all((sd / "events.jsonl").read_text()))
ws = sd / "workspace.yaml"
out["workspace_yaml"] = ws.read_text() if ws.exists() else None
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
screens.write_text(sanitise_all(screens.read_text()))
print(sanitise_all(json.dumps({k: v for k, v in out.items() if k != "usage_like"}, indent=2)))

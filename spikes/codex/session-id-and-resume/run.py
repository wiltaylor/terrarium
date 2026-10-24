# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 3: session id capture, resume, and the session file.

Two model turns (gpt-6-luna, low).
 A fresh start, T1 "Reply with exactly: pong". The id comes from SessionStart.
   /exit. Read the exit text.
 B `codex resume <id>` in the same cwd. T2 asks for the earlier word.
   SessionStart.source? same id? /exit.
 C `codex resume <id>` from another cwd (no turn): what shows?
 D two `codex resume <id>` at once (no turn): does the second fail?
 E a start with a made-up UUID: `codex resume <random uuid>` (no turn).
The rollout file is copied (sanitised) to captured/session.jsonl.
"""
import json, sys, time, uuid
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
screens = CAP / "screens.txt"; screens.write_text("")
cwd = make_cwd(); cwd2 = make_cwd()
home = make_codex_home()
logf = home / "hooks.jsonl"
out = {}


def hooks(name=None):
    return [h for h in read_hooks(logf) if name is None or h["hook"] == name]


def turn(s, text, timeout=120):
    n = len(hooks("Stop"))
    wait_until(lambda: screen_state(s.lines())["input_box"], 40)
    s.wait_stable(quiet=0.8, timeout=10)
    s.paste(text); s.send("\r")
    wait_until(lambda: len(hooks("Stop")) > n, timeout)
    s.wait_stable(quiet=0.8, timeout=10)
    st = hooks("Stop")
    return st[-1]["payload"].get("last_assistant_message") if len(st) > n else None


def exit_(s, label):
    s.send("/exit"); time.sleep(0.3); s.send("\r")
    dt = wait_exit(s, 15); time.sleep(1.0)
    snap(s, screens, label); s.close()
    return dt


# A
s = start(base_args(cwd, logf), cwd, home)
out["A_reply"] = turn(s, "Reply with exactly: pong")
sid = hooks("SessionStart")[0]["payload"]["session_id"]
out["A_session_id"] = sid
out["A_SessionStart"] = hooks("SessionStart")[0]["payload"]
out["A_exit_s"] = exit_(s, "A-after-exit")
tail = [l for l in s.text().splitlines() if l.strip()][-4:]
out["A_exit_text"] = tail

# B
n_ss = len(hooks("SessionStart"))
s = start(["resume", sid] + base_args(cwd, logf), cwd, home)
wait_until(lambda: screen_state(s.lines())["input_box"], 40); s.wait_stable(1.0, 10)
snap(s, screens, "B-resumed-idle")
out["B_reply"] = turn(s, "What exact word did you reply with in your previous message? Reply with that word only.")
out["B_SessionStart"] = [h["payload"] for h in hooks("SessionStart")[n_ss:]]
out["B_exit_s"] = exit_(s, "B-after-exit")

# C: resume from another cwd, no turn
s = start(["resume", sid] + base_args(cwd2, logf), cwd2, home)
time.sleep(6); snap(s, screens, "C-resume-other-cwd")
out["C_screen_tail"] = [l for l in s.lines() if l.strip()][-12:]
s.send("\x1b"); time.sleep(0.5); s.close()

# D: two resumes of one id at once, no turn
s1 = start(["resume", sid] + base_args(cwd, logf), cwd, home)
time.sleep(4)
s2 = start(["resume", sid] + base_args(cwd, logf), cwd, home)
time.sleep(6)
snap(s1, screens, "D-first"); snap(s2, screens, "D-second")
out["D_second_alive"] = s2.alive
out["D_second_tail"] = [l for l in s2.lines() if l.strip()][-10:]
s2.close(); s1.close()

# E: resume a UUID that does not exist
bogus = str(uuid.uuid4())
s = start(["resume", bogus] + base_args(cwd, logf), cwd, home)
time.sleep(5); snap(s, screens, "E-bogus-uuid")
out["E_alive"] = s.alive
out["E_tail"] = [l for l in s.text().splitlines() if l.strip()][-6:]
s.close()

# Session files
files = sorted(home.glob("sessions/**/*.jsonl"))
out["session_files"] = [str(p.relative_to(home)) for p in files]
out["session_index"] = (home / "session_index.jsonl").read_text() if (home / "session_index.jsonl").exists() else None
main = [p for p in files if sid in p.name]
if main:
    recs = [json.loads(l) for l in main[0].read_text().splitlines() if l.strip()]
    kinds = {}
    for r in recs:
        k = r.get("type") + ("/" + r["payload"].get("type", "") if isinstance(r.get("payload"), dict) else "")
        kinds[k] = kinds.get(k, 0) + 1
    out["record_types"] = kinds
    out["token_count_events"] = [r["payload"] for r in recs
                                 if r.get("type") == "event_msg" and r["payload"].get("type") == "token_count"]
    out["turn_context_models"] = sorted({r["payload"].get("model") for r in recs if r.get("type") == "turn_context"})
    (CAP / "session.jsonl").write_text(sanitise_all(main[0].read_text()))
(CAP / "hooks.jsonl").write_text(sanitise_all("\n".join(json.dumps(h) for h in read_hooks(logf))) + "\n")
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
screens.write_text(sanitise_all(screens.read_text()))
print(sanitise_all(json.dumps({k: v for k, v in out.items() if k not in ("token_count_events",)}, indent=2)))
stop_daemons(home)

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 3: set the session id, resume, exit keys, and the session file.

Two model turns (openai-codex/gpt-5.6-luna, thinking off).
 A --session-id <new uuid>, T1 "Reply with exactly: pong", Ctrl-D.
 B --session-id <same uuid> again, T2 asks for the earlier word.
 C --session <uuid> (no turn), and --session-id <uuid> from another cwd.
 D two starts with --session-id <uuid> at once (no turn).
 E exit keys: Ctrl-C once, twice; /quit.
The session file is copied (sanitised) to captured/session.jsonl.
"""
import json, sys, time, uuid
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
screens = CAP / "screens.txt"; screens.write_text("")
d = make_pi_dir(); cwd = make_cwd(); cwd2 = make_cwd()
sockp = d / "ev.sock"; es = EventSocket(sockp)
out = {}
ARGS = base_args() + ["--no-skills"]


def settled_count():
    return len(es.of("agent_settled"))


def turn(s, text):
    n = settled_count()
    wait_until(lambda: len(es.of("session_start")) > 0, 20); s.wait_stable(1.0, 10)
    s.paste(text); s.send("\r")
    wait_until(lambda: settled_count() > n, 120)
    return es.of("message_end")[-1].get("last_assistant_message")


def starts_since(k):
    return [{x: e.get(x) for x in ("session_id", "reason", "session_file")} for e in es.of("session_start")[k:]]


sid = str(uuid.uuid4())
k = len(es.of("session_start"))
s = start(ARGS + ["--session-id", sid], cwd, d, sockp)
out["A_reply"] = turn(s, "Reply with exactly: pong")
out["A_session_start"] = starts_since(k)
s.send("\x04"); out["A_exit_ctrl_d_s"] = wait_exit(s, 10); s.close()
files = sorted(d.glob("sessions/**/*.jsonl"))
out["A_session_files"] = [str(p.relative_to(d)) for p in files]

k = len(es.of("session_start"))
s = start(ARGS + ["--session-id", sid], cwd, d, sockp)
time.sleep(2); snap(s, screens, "B-session-id-again")
out["B_reply"] = turn(s, "What exact word did you reply with in your previous message? Reply with that word only.")
out["B_session_start"] = starts_since(k)
# E: exit keys
s.send("\x03"); time.sleep(1.0); snap(s, screens, "E-after-one-ctrl-c")
out["E_after_one_ctrl_c_alive"] = s.alive
s.send("\x03"); out["E_second_ctrl_c_exit_s"] = wait_exit(s, 5)
if s.alive:
    s.send("/quit\r"); out["E_quit_exit_s"] = wait_exit(s, 5)
s.close()

for label, args, c in [("C-session-flag", ["--session", sid], cwd),
                       ("C-session-id-other-cwd", ["--session-id", sid], cwd2),
                       ("C-session-flag-other-cwd", ["--session", sid], cwd2)]:
    k = len(es.of("session_start"))
    s = start(ARGS + args, c, d, sockp)
    time.sleep(5); snap(s, screens, label)
    out[label] = {"alive": s.alive, "session_start": starts_since(k),
                  "tail": [l for l in s.text().splitlines() if l.strip()][-4:]}
    s.send("\x04"); wait_exit(s, 5); s.close()

k = len(es.of("session_start"))
s1 = start(ARGS + ["--session-id", sid], cwd, d, sockp); time.sleep(4)
s2 = start(ARGS + ["--session-id", sid], cwd, d, sockp); time.sleep(5)
snap(s2, screens, "D-second")
out["D_second"] = {"alive": s2.alive, "session_start": starts_since(k),
                   "tail": [l for l in s2.text().splitlines() if l.strip()][-4:]}
for x in (s2, s1):
    x.send("\x04"); wait_exit(x, 5); x.close()

files = sorted(d.glob("sessions/**/*.jsonl"))
out["session_files_end"] = [str(p.relative_to(d)) for p in files]
main = [p for p in files if sid in p.name]
if main:
    recs = [json.loads(l) for l in main[0].read_text().splitlines() if l.strip()]
    kinds = {}
    for r in recs:
        key = r.get("type") + ("/" + r["message"]["role"] if r.get("type") == "message" else "")
        kinds[key] = kinds.get(key, 0) + 1
    out["record_types"] = kinds
    out["header"] = recs[0]
    out["assistant_usage"] = [{"model": r["message"].get("model"), "provider": r["message"].get("provider"),
                               "usage": r["message"].get("usage")} for r in recs
                              if r.get("type") == "message" and r["message"].get("role") == "assistant"]
    (CAP / "session.jsonl").write_text(sanitise_all(main[0].read_text()))
(CAP / "events.jsonl").write_text(sanitise_all("\n".join(json.dumps(e) for e in es.events)) + "\n")
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
screens.write_text(sanitise_all(screens.read_text()))
print(sanitise_all(json.dumps(out, indent=2)))

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 2: --session-id, clean exit, --resume, the session JSONL, usage fields.

Two model turns (haiku). Steps:
 1. start with --session-id, one turn, exit with /exit
 2. start again with the same --session-id (no resume): what happens? (0 turns)
 3. --resume from a different cwd: found or not? (0 turns)
 4. --resume in the same cwd, ask for the word back, exit with Ctrl-C twice
Then dump the JSONL record types and per-message usage fields.
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
cwd = make_cwd()
other = make_cwd()
cfg = make_config_dir(None, trust=False, onboarding=False)
(cfg / ".claude.json").write_text(json.dumps({
    "hasCompletedOnboarding": True,
    "projects": {str(cwd): {"hasTrustDialogAccepted": True},
                 str(other): {"hasTrustDialogAccepted": True}}}))
logf = cfg / "hooks.jsonl"
settings = json.dumps({"skipDangerousModePermissionPrompt": True,
                       **hook_settings(logf, ["SessionStart", "SessionEnd", "UserPromptSubmit", "Stop"])})
base = ["--dangerously-skip-permissions", "--model", "haiku", "--setting-sources", "", "--settings", settings]
sid = new_uuid()
screens = CAP / "screens.txt"; screens.write_text("")
out = {"session_id": sid}


def start(args, where=cwd):
    s = Session(base + args, where, clean_env(cfg))
    s.wait_stable(quiet=2.0, timeout=30)
    return s


def turn(s, text, timeout=90):
    n0 = sum(1 for h in read_hooks(logf) if h["hook"] == "Stop")
    s.paste(text); time.sleep(0.15); s.send("\r")
    t = time.time()
    while time.time() - t < timeout:
        stops = [h for h in read_hooks(logf) if h["hook"] == "Stop"]
        if len(stops) > n0:
            return stops[-1]["payload"].get("last_assistant_message")
        time.sleep(0.2)
    return None


def wait_exit(s, timeout=10):
    t = time.time()
    while s.alive and time.time() - t < timeout:
        time.sleep(0.1)
    return not s.alive


# 1. fresh session with a fixed id
s = start(["--session-id", sid])
out["turn1_reply"] = turn(s, "Remember the word PERIWINKLE. Reply with exactly: ok")
s.send("/exit"); time.sleep(0.3); s.send("\r")
out["exit_via_slash_exit"] = wait_exit(s)
snap(s, screens, "after /exit"); s.close()

jsonl = list((cfg / "projects").rglob(f"{sid}.jsonl"))
out["jsonl_paths"] = [str(p.relative_to(cfg)) for p in jsonl]

# 2. same --session-id again without --resume
s = start(["--session-id", sid])
snap(s, screens, "same --session-id again")
out["reuse_session_id_screen"] = [l for l in s.lines() if l.strip()][-6:]
out["reuse_session_id_alive"] = s.alive
s.close()

# 3. --resume from another cwd
s = start(["--resume", sid], where=other)
snap(s, screens, "--resume from other cwd")
out["resume_other_cwd_shows_history"] = "PERIWINKLE" in s.text()
out["resume_other_cwd_alive"] = s.alive
s.close()

# 4. --resume in the same cwd
time.sleep(1)
s = start(["--resume", sid])
snap(s, screens, "--resume same cwd, before turn")
out["resume_screen_shows_history"] = "PERIWINKLE" in s.text()
out["turn2_reply"] = turn(s, "What word did I ask you to remember? Reply with only the word.")
snap(s, screens, "--resume same cwd, after turn")
t = time.time()
s.send("\x03"); time.sleep(0.4)
snap(s, screens, "after one Ctrl-C")
out["one_ctrl_c_alive"] = s.alive
s.send("\x03")
out["exit_via_ctrl_c_twice"] = wait_exit(s)
time.sleep(1.5)
snap(s, screens, "after two Ctrl-C"); s.close()

hooks = read_hooks(logf)
out["hooks"] = [(h["hook"], h["payload"].get("session_id") == sid,
                 h["payload"].get("source") or h["payload"].get("reason")) for h in hooks]

# JSONL analysis
lines = [json.loads(l) for p in jsonl for l in p.read_text().splitlines() if l.strip()]
types = {}
for r in lines:
    types[r.get("type")] = types.get(r.get("type"), 0) + 1
out["jsonl_record_types"] = types
out["jsonl_top_keys"] = sorted({k for r in lines for k in r})
usage = [dict(model=r["message"].get("model"), id=r["message"].get("id"),
              requestId=r.get("requestId"), usage=r["message"].get("usage"))
         for r in lines if r.get("type") == "assistant" and isinstance(r.get("message"), dict)]
out["assistant_usage_records"] = usage
(CAP / "session.jsonl").write_text(sanitise("\n".join(json.dumps(r) for r in lines)) + "\n")
(CAP / "results.json").write_text(sanitise(json.dumps(out, indent=2)))
screens.write_text(sanitise(screens.read_text()))
print(sanitise(json.dumps(out, indent=2)))

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 5: typing a turn into the TUI. Four user turns (haiku 4.5).

 T1 bracketed paste, three lines, CR at once.
 T2 plain keys "Reply with exactly:" LF " plain" CR: does LF submit?
 T3 a slow turn (sleep 5) and, while busy, T4 pasted + CR: queued, steered, lost?
 T5 ask_user: what the question looks like and which keys answer it.
Screens: captured/screens.txt and captured/screens-dim.txt (dim blanked).
"""
import json, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
scr, scrd = CAP / "screens.txt", CAP / "screens-dim.txt"
scr.write_text(""); scrd.write_text("")
cwd = make_cwd(); subprocess.run(["git", "init", "-q", str(cwd)], check=True)
home = make_copilot_home(); LOG = home / "hooks.jsonl"
(home / "hooks").mkdir(); (home / "hooks/terrarium.json").write_text(json.dumps(hooks_json(LOG)))
out = {}


def snap2(s, label):
    snap(s, scr, label)
    with scrd.open("a") as f:
        f.write(f"===== {label} (t={time.time()-s.t0:.1f}s) =====\n" + "\n".join(s.dim_mask()).rstrip() + "\n\n")


def hooks(name):
    return [h for h in read_hooks(LOG) if h["hook"] == name]


def wait_hook(name, n, timeout):
    return wait_until(lambda: len(hooks(name)) > n, timeout, poll=0.01)


s = start(base_args(), cwd, home)
out["first_ready_s"] = wait_until(lambda: state(s)["ready"], 40, poll=0.02)
wait_ready(s, 30, 1.0)
snap2(s, "idle-start")

nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
s.paste("First line.\nSecond line.\nReply with exactly: two"); time.sleep(0.2); snap2(s, "lag-paste-not-drawn-at-0.2s")
t = time.time(); s.send("\r")
out["T1_submit_hook_s"] = wait_hook("UserPromptSubmit", nu, 10)
time.sleep(0.4); snap2(s, "busy-T1")
wait_hook("Stop", ns, 90); wait_ready(s, 20, 0.5); snap2(s, "idle-after-T1")
out["T1_prompt"] = hooks("UserPromptSubmit")[nu]["payload"]["prompt"] if out["T1_submit_hook_s"] else None

nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
for ch in "Reply with exactly:":
    s.send(ch); time.sleep(0.01)
s.send("\n"); time.sleep(0.8)
out["T2_lf_submitted"] = len(hooks("UserPromptSubmit")) > nu
snap2(s, "typed-after-LF")
for ch in " plain":
    s.send(ch); time.sleep(0.01)
time.sleep(0.3); s.send("\r")
out["T2_submit_hook_s"] = wait_hook("UserPromptSubmit", nu, 10)
wait_hook("Stop", ns, 90); wait_ready(s, 20, 0.5)
out["T2_prompts"] = [h["payload"]["prompt"] for h in hooks("UserPromptSubmit")[nu:]]

nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
s.paste("Run the shell command `sleep 5`, then reply with exactly: slept"); s.send("\r")
wait_hook("PreToolUse", len(hooks("PreToolUse")), 30); time.sleep(0.7)
snap2(s, "busy-T3-tool")
t4 = time.time()
s.paste("Also reply with the word: queued"); time.sleep(0.3); snap2(s, "busy-typed-T4")
s.send("\r"); time.sleep(1.0); snap2(s, "busy-after-T4-enter")
wait_until(lambda: len(hooks("Stop")) >= ns + 2, 60)
time.sleep(3); wait_ready(s, 30, 0.5); snap2(s, "idle-after-T3-T4")
out["T3_T4"] = {"submits": [[round(h["ts"] - t4, 2), h["payload"]["prompt"]] for h in hooks("UserPromptSubmit")[nu:]],
                "stops": [round(h["ts"] - t4, 2) for h in hooks("Stop")[ns:]]}

# T5 ask_user
nq = len(read_hooks(LOG)); ns = len(hooks("Stop"))
s.paste("Use the ask_user tool to ask me to pick one colour: red or blue. Then reply with exactly the colour I picked.")
s.send("\r")
wait_until(lambda: state(s)["dialog"] is not None, 60)
time.sleep(1.0); snap2(s, "dialog-ask-user")
out["T5_hooks_while_question"] = [h["hook"] for h in read_hooks(LOG)[nq:]]
out["T5_dialog_state"] = state(s)
s.send("\x1b[B"); time.sleep(0.4); snap2(s, "dialog-ask-user-down"); s.send("\r")
wait_hook("Stop", ns, 60); time.sleep(2); wait_ready(s, 20, 0.5); snap2(s, "idle-after-T5")
sid = hooks("SessionStart")[0]["payload"]["session_id"]
recs = [json.loads(l) for l in (home / "session-state" / sid / "events.jsonl").read_text().splitlines() if l.strip()]
out["replies"] = [r["data"].get("content") for r in recs if r["type"] == "assistant.message"]
out["T5_hook_payloads"] = [h for h in read_hooks(LOG)[nq:] if h["hook"] in ("PreToolUse", "PermissionRequest", "Notification")]
s.send("/exit"); time.sleep(0.4); s.send("\r"); wait_exit(s, 10); s.close()
(CAP / "hooks.jsonl").write_text(sanitise_all("\n".join(json.dumps(h) for h in read_hooks(LOG))) + "\n")
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
for f in (scr, scrd):
    f.write_text(sanitise_all(f.read_text()))
print(sanitise_all(json.dumps(out, indent=2))[:6000])

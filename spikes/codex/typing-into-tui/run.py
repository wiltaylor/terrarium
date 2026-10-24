# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 5: typing a turn into the TUI. Three or four model turns.

 T1 bracketed paste, three lines, CR at once: submitted? newlines kept?
 T2 plain keys "Reply with exactly:" LF " plain" CR: does LF submit?
 T3 a slow turn (sleep 5), then T4 pasted + CR while busy: steer, queue,
    or lost? How many UserPromptSubmit and Stop?
 R  resume from another cwd with -c tui.resume_cwd="session" (no turn).
Screens go to captured/screens.txt and, with dim text blanked, to
captured/screens-dim.txt. Labels: idle-* (expect ready), busy-* (expect busy),
typed-* (expect input text), dialog-* (expect dialog).
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
scr, scrd = CAP / "screens.txt", CAP / "screens-dim.txt"
scr.write_text(""); scrd.write_text("")
cwd = make_cwd(); cwd2 = make_cwd()
home = make_codex_home()
logf = home / "hooks.jsonl"
out = {}


def snap2(s, label):
    snap(s, scr, label)
    with scrd.open("a") as f:
        f.write(f"===== {label} (t={time.time()-s.t0:.1f}s) =====\n")
        f.write("\n".join(s.dim_mask()).rstrip() + "\n\n")


def hooks(name):
    return [h for h in read_hooks(logf) if h["hook"] == name]


def wait_hook(name, n, timeout):
    return wait_until(lambda: len(hooks(name)) > n, timeout, poll=0.01)


s = start(base_args(cwd, logf), cwd, home)
first_ready = wait_until(lambda: state(s)["ready"], 30, poll=0.02)
stable = wait_ready(s, 30, stable=1.0)
out["first_ready_s"], out["stable_ready_s"] = first_ready, stable
snap2(s, "idle-start")

# T1
nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
t = time.time()
s.paste("First line.\nSecond line.\nReply with exactly: two"); s.send("\r")
dt_u = wait_hook("UserPromptSubmit", nu, 10)
time.sleep(0.3); snap2(s, "busy-T1")
wait_hook("Stop", ns, 90); wait_ready(s, 20, 0.5); snap2(s, "idle-after-T1")
out["T1"] = {"submit_hook_s": dt_u, "prompt": hooks("UserPromptSubmit")[nu]["payload"]["prompt"] if dt_u else None,
             "reply": hooks("Stop")[-1]["payload"].get("last_assistant_message")}

# T2
nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
for ch in "Reply with exactly:":
    s.send(ch); time.sleep(0.01)
s.send("\n"); time.sleep(0.8)
lf_submitted = len(hooks("UserPromptSubmit")) > nu
snap2(s, "typed-after-LF")
for ch in " plain":
    s.send(ch); time.sleep(0.01)
time.sleep(0.3); snap2(s, "typed-before-CR")
s.send("\r")
dt_u = wait_hook("UserPromptSubmit", nu, 10)
wait_hook("Stop", ns, 90); wait_ready(s, 20, 0.5)
out["T2"] = {"lf_submitted": lf_submitted, "submit_hook_s": dt_u,
             "prompts": [h["payload"]["prompt"] for h in hooks("UserPromptSubmit")[nu:]],
             "reply": hooks("Stop")[-1]["payload"].get("last_assistant_message")}

# T3 + T4 (type while busy)
nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
s.paste("Run the shell command `sleep 5`, then reply with exactly: slept"); s.send("\r")
wait_hook("PreToolUse", len(hooks("PreToolUse")), 30)
time.sleep(0.5); snap2(s, "busy-T3-tool")
t4 = time.time()
s.paste("Also add the word: queued"); time.sleep(0.2); snap2(s, "busy-typed-T4")
s.send("\r"); time.sleep(1.0); snap2(s, "busy-after-T4-enter")
wait_until(lambda: len(hooks("Stop")) >= ns + 2, 90)
time.sleep(2); wait_ready(s, 30, 0.5); snap2(s, "idle-after-T3-T4")
ups = hooks("UserPromptSubmit")[nu:]
out["T3_T4"] = {"user_prompt_submits": [[round(h["ts"] - t4, 2), h["payload"]["prompt"], h["payload"].get("turn_id")] for h in ups],
                "stops": [[round(h["ts"] - t4, 2), h["payload"].get("last_assistant_message"), h["payload"].get("turn_id")]
                          for h in hooks("Stop")[ns:]]}
sid = hooks("SessionStart")[0]["payload"]["session_id"]
s.send("/exit"); time.sleep(0.3); s.send("\r"); wait_exit(s, 10); s.close()

# R: resume from another cwd with resume_cwd = session
s = start(["resume", sid, *base_args(cwd2, logf), "-c", 'tui.resume_cwd="session"'], cwd2, home)
time.sleep(5); snap2(s, "resume-other-cwd-with-resume_cwd-session")
out["R_tail"] = [l for l in s.lines() if l.strip()][-6:]
out["R_state"] = state(s)
s.send("\x03"); time.sleep(0.3); s.send("\x03"); wait_exit(s, 5); s.close()

(CAP / "hooks.jsonl").write_text(sanitise_all("\n".join(json.dumps(h) for h in read_hooks(logf))) + "\n")
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
for f in (scr, scrd):
    f.write_text(sanitise_all(f.read_text()))
print(sanitise_all(json.dumps(out, indent=2)))
stop_daemons(home)

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 5: how to type a turn into the running TUI.

About eight model turns (haiku), all in one session:
 T1 type before the prompt is ready (t≈0.3 s after spawn): lost or buffered?
 T2 plain text + CR, single line; UserPromptSubmit latency
 T3 bracketed paste (multi-line) + CR with NO delay: is the CR eaten?
 T4 big bracketed paste (~4000 chars, 80 lines): full text in the hook?
 T5 plain text with a bare LF inside: does LF submit?
 T6 a second prompt typed while the first is busy: queued or lost?
 T7 AskUserQuestion: what the screen shows and which keys answer it
The screen-state detector is lib/ptyharness.screen_state().
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
screens = CAP / "screens.txt"; screens.write_text("")
QUIET = {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}
cwd = make_cwd()
cfg = make_config_dir(None, trust=False, onboarding=False)
(cfg / ".claude.json").write_text(json.dumps({
    "hasCompletedOnboarding": True, "projects": {str(cwd): {"hasTrustDialogAccepted": True}}}))
logf = cfg / "hooks.jsonl"
settings = json.dumps({"skipDangerousModePermissionPrompt": True, **hook_settings(logf)})
out = {}


def hooks(name=None):
    return [h for h in read_hooks(logf) if name is None or h["hook"] == name]


def wait_hook(name, n_before, timeout):
    t = time.time()
    while time.time() - t < timeout:
        hs = hooks(name)
        if len(hs) > n_before:
            return hs[n_before]
        time.sleep(0.02)
    return None


def wait_ready(s, timeout=60):
    t = time.time()
    while time.time() - t < timeout:
        if screen_state(s.lines())["ready"]:
            return time.time() - t
        time.sleep(0.05)
    return None


def submit_and_wait(s, label, keys: list[tuple[bytes, float]], stop_timeout=90):
    """Send keys [(bytes, delay_after)], measure UserPromptSubmit and Stop."""
    nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
    t_last = None
    for data, delay in keys:
        s.send(data); t_last = time.time()
        if delay: time.sleep(delay)
    u = wait_hook("UserPromptSubmit", nu, 5)
    r = {"submitted_within_5s": u is not None}
    if u:
        r["submit_latency_ms_after_last_key"] = round((u["ts"] - t_last) * 1000)
        r["prompt_len"] = len(u["payload"]["prompt"])
        r["prompt_repr"] = u["payload"]["prompt"][:120]
    st = wait_hook("Stop", ns, stop_timeout) if u else None
    r["reply"] = st["payload"].get("last_assistant_message") if st else None
    s.wait_stable(0.8, 10)
    snap(s, screens, label)
    log(label, r)
    return r


s = Session(["--dangerously-skip-permissions", "--model", "haiku", "--setting-sources", "",
             "--settings", settings], cwd, clean_env(cfg, QUIET))

# T1: type too early
time.sleep(0.3)
nu = len(hooks("UserPromptSubmit"))
s.send("Reply with exactly: early"); t_early = time.time()
s.send("\r")
ready = wait_ready(s, 30)
out["startup_ready_s"] = round(time.time() - s.t0, 2)
snap(s, screens, "T1 after startup")
u = wait_hook("UserPromptSubmit", nu, 4)
st0 = screen_state(s.lines())
out["T1_early_typing"] = {"submitted": u is not None,
                          "input_text_after_start": st0["input_text"]}
if u:
    wait_hook("Stop", 0, 60)
    s.wait_stable(0.8, 10)
elif st0["input_text"]:
    s.send("\x15")  # Ctrl-U clears the line if text was buffered
log("T1", out["T1_early_typing"], "ready after", out["startup_ready_s"])
wait_ready(s, 30)

# T2: plain text + CR
out["T2_plain_cr"] = submit_and_wait(s, "T2", [(b"Reply with exactly: pong", 0.1), (b"\r", 0)])
wait_ready(s, 30)

# T3: bracketed paste multi-line, CR immediately
txt = "First line of a pasted prompt.\nSecond line.\nReply with exactly: pasted"
out["T3_paste_cr_nodelay"] = submit_and_wait(
    s, "T3", [(b"\x1b[200~" + txt.encode() + b"\x1b[201~", 0), (b"\r", 0)])
if not out["T3_paste_cr_nodelay"]["submitted_within_5s"]:
    snap(s, screens, "T3 not submitted; screen")
    out["T3_retry_cr"] = submit_and_wait(s, "T3-retry", [(b"\r", 0)])
out["T3_newlines_preserved"] = "\n" in (hooks("UserPromptSubmit")[-1]["payload"]["prompt"])
wait_ready(s, 30)

# T4: big paste
big = "\n".join(f"line {i:03d}: lorem ipsum dolor sit amet consectetur adipiscing elit sed do" for i in range(80))
big += "\nIgnore the lines above. Reply with exactly: big"
nu = len(hooks("UserPromptSubmit"))
s.paste(big); time.sleep(0.5)
snap(s, screens, "T4 big paste before CR")
out["T4_input_box_after_paste"] = screen_state(s.lines())["input_text"]
r = submit_and_wait(s, "T4", [(b"\r", 0)])
r["sent_len"] = len(big)
r["full_text_in_hook"] = hooks("UserPromptSubmit")[-1]["payload"]["prompt"] == big if r["submitted_within_5s"] else None
out["T4_big_paste"] = r
wait_ready(s, 30)

# T5: plain text with a bare LF in it
nu = len(hooks("UserPromptSubmit"))
s.send("Reply with exactly: lf\nsecond-part")
time.sleep(1.5)
u = wait_hook("UserPromptSubmit", nu, 2)
st5 = screen_state(s.lines())
snap(s, screens, "T5 after plain text with LF")
out["T5_plain_lf"] = {"lf_submitted": u is not None, "input_text": st5["input_text"]}
if u:
    wait_hook("Stop", len(hooks("Stop")) - 0, 0)
    time.sleep(5)
s.send("\x15"); time.sleep(0.3); s.send("\x15"); time.sleep(0.3)
st5b = screen_state(s.lines())
out["T5_after_ctrl_u"] = st5b["input_text"]
if st5b["input_text"]:
    out["T5_submit"] = submit_and_wait(s, "T5-submit", [(b"\r", 0)])
log("T5", out["T5_plain_lf"])
wait_ready(s, 30)

# T6: second prompt while busy
nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
s.paste("Run the Bash command: sleep 6. Then reply with exactly: first"); time.sleep(0.15); s.send("\r")
wait_hook("UserPromptSubmit", nu, 5)
wait_hook("PreToolUse", len(hooks("PreToolUse")) - 1 if False else 0, 0)
time.sleep(2.5)
busy = screen_state(s.lines())["busy"]
t_second = time.time()
s.paste("Reply with exactly: second"); time.sleep(0.15); s.send("\r")
time.sleep(1.0)
snap(s, screens, "T6 second prompt typed while busy")
st6 = screen_state(s.lines())
wait_hook("Stop", ns + 1, 90)
s.wait_stable(1.0, 15)
snap(s, screens, "T6 after")
ups = hooks("UserPromptSubmit")[nu:]
stops = hooks("Stop")[ns:]
out["T6_busy_queue"] = {
    "busy_when_typed": busy, "input_text_1s_after": st6["input_text"],
    "user_prompt_submits": [(round(h["ts"] - t_second, 2), h["payload"]["prompt"][:40]) for h in ups],
    "stops": [(round(h["ts"] - t_second, 2), h["payload"].get("last_assistant_message")) for h in stops]}
log("T6", out["T6_busy_queue"])
wait_ready(s, 30)

# T7: AskUserQuestion
nu, ns = len(hooks("UserPromptSubmit")), len(hooks("Stop"))
npre = len(hooks("PreToolUse"))
s.paste("Use the AskUserQuestion tool to ask me to pick one colour: Red, Green or Blue. "
        "After I answer, reply with exactly the colour I picked."); time.sleep(0.15); s.send("\r")
pre = wait_hook("PreToolUse", npre, 60)
time.sleep(2.0); s.wait_stable(1.0, 10)
snap(s, screens, "T7 question shown")
q_screen = s.lines()
st7 = screen_state(q_screen)
notif = [h for h in hooks("Notification")]
# Answer: move down once (Red -> Green), then Enter.
s.send("\x1b[B"); time.sleep(0.4)
snap(s, screens, "T7 after Down")
s.send("\r"); time.sleep(1.5)
snap(s, screens, "T7 after Enter")
st7b = screen_state(s.lines())
if not st7b["busy"] and "Submit" in s.text():
    s.send("\r"); time.sleep(1.0)   # a review/submit step, if present
    snap(s, screens, "T7 after second Enter")
stop = wait_hook("Stop", ns, 60)
s.wait_stable(1.0, 10)
snap(s, screens, "T7 after answer")
out["T7_ask_user_question"] = {
    "pre_tool_use_tool": pre["payload"].get("tool_name") if pre else None,
    "pre_tool_input": pre["payload"].get("tool_input") if pre else None,
    "notifications": [(h["payload"].get("notification_type"), h["payload"].get("message")) for h in notif],
    "detector_on_question": {k: v for k, v in st7.items() if k != "footer"},
    "reply": stop["payload"].get("last_assistant_message") if stop else None}
log("T7", out["T7_ask_user_question"])

s.send("/exit"); time.sleep(0.3); s.send("\r"); time.sleep(1.5); s.close()
(CAP / "hooks.jsonl").write_text(sanitise("\n".join(json.dumps(h) for h in read_hooks(logf))) + "\n")
(CAP / "results.json").write_text(sanitise(json.dumps(out, indent=2)))
screens.write_text(sanitise(screens.read_text()))
print(sanitise(json.dumps(out, indent=2)))

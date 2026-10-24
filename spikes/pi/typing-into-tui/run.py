# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 5: typing a turn into the TUI. Two or three model turns.

 L  (no turn) type "abc", LF: does LF submit? Then Ctrl-C to clear.
 T1 bracketed paste, three lines, CR at once.
 T3 a slow turn (sleep 5), and while busy T4 pasted + CR: steer or follow-up?
Screens: captured/screens.txt and captured/screens-dim.txt. Labels: idle-*,
busy-*, typed-*.
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
scr, scrd = CAP / "screens.txt", CAP / "screens-dim.txt"
scr.write_text(""); scrd.write_text("")
d = make_pi_dir(); cwd = make_cwd(); sockp = d / "ev.sock"; es = EventSocket(sockp)
out = {}


def snap2(s, label):
    snap(s, scr, label)
    with scrd.open("a") as f:
        f.write(f"===== {label} (t={time.time()-s.t0:.1f}s) =====\n" + "\n".join(s.dim_mask()).rstrip() + "\n\n")


s = start(base_args() + ["--no-skills"], cwd, d, sockp)
out["session_start_event_s"] = wait_until(lambda: es.of("session_start"), 20)
time.sleep(0.3); snap2(s, "early-0.3s")
s.wait_stable(1.0, 10); out["stable_s"] = round(time.time() - s.t0, 2)
snap2(s, "idle-start")

# L: LF test, no turn
for ch in "abc":
    s.send(ch); time.sleep(0.02)
s.send("\n"); time.sleep(0.8)
out["L_lf_submitted"] = len(es.of("input")) > 0
snap2(s, "typed-after-LF")
s.send("\x03"); time.sleep(0.6); snap2(s, "idle-after-ctrl-c-clear")
out["L_alive_after_ctrl_c"] = s.alive

# T1
n = len(es.of("agent_settled")); t = time.time()
s.paste("First line.\nSecond line.\nReply with exactly: two"); s.send("\r")
out["T1_input_event_s"] = wait_until(lambda: es.of("input"), 10, poll=0.01)
time.sleep(0.3); snap2(s, "busy-T1")
wait_until(lambda: len(es.of("agent_settled")) > n, 90); time.sleep(0.5); s.wait_stable(0.8, 10)
snap2(s, "idle-after-T1")
out["T1_prompt"] = es.of("input")[0].get("prompt")
out["T1_reply"] = es.of("message_end")[-1].get("last_assistant_message")

# T3 + T4
n = len(es.of("agent_settled")); ni = len(es.of("input"))
s.paste("Use the bash tool to run: sleep 5. Then reply with exactly: slept"); s.send("\r")
wait_until(lambda: es.of("tool_execution_start"), 40); time.sleep(0.5)
snap2(s, "busy-T3-tool")
t4 = time.time()
s.paste("Also add the word: queued"); time.sleep(0.3); snap2(s, "busy-typed-T4")
s.send("\r"); time.sleep(1.0); snap2(s, "busy-after-T4-enter")
wait_until(lambda: len(es.of("agent_settled")) > n, 90); time.sleep(1); s.wait_stable(0.8, 10)
snap2(s, "idle-after-T3-T4")
out["T3_T4"] = {"inputs": [[round(e["_rx"] - t4, 2), e.get("prompt"), e.get("streaming_behavior")] for e in es.of("input")[ni:]],
                "agent_runs": [[round(e["_rx"] - t4, 2), e["hook_event_name"]] for e in es.events
                               if e["hook_event_name"] in ("agent_start", "agent_end", "agent_settled") and e["_rx"] > t4 - 10],
                "replies": [e.get("last_assistant_message") for e in es.of("message_end")]}
s.send("\x04"); wait_exit(s, 5); s.close()
(CAP / "events.jsonl").write_text(sanitise_all("\n".join(json.dumps(e) for e in es.events)) + "\n")
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
for f in (scr, scrd):
    f.write_text(sanitise_all(f.read_text()))
print(sanitise_all(json.dumps(out, indent=2)))

# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 4: system prompt control. Three model turns (gpt-6-luna, low).

 T1 -c model_instructions_file=<file>. The file says the secret word is
    MARMALADE. Does it replace the base instructions? (session_meta)
 T2 new session: -c developer_instructions="... MARMALADE" plus an AGENTS.md
    in the cwd with a second word QUINCE. Where does each land?
 T3 resume the T2 session with developer_instructions changed to BRAMBLE.
    Does the new text apply on resume?
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
home = make_codex_home()
logf = home / "hooks.jsonl"
out = {}


def hooks(name):
    return [h for h in read_hooks(logf) if h["hook"] == name]


def one_turn(args, cwd, prompt):
    n = len(hooks("Stop"))
    s = start(args, cwd, home)
    wait_until(lambda: screen_state(s.lines())["input_box"], 30); s.wait_stable(1.0, 10)
    s.paste(prompt); s.send("\r")
    wait_until(lambda: len(hooks("Stop")) > n, 120)
    reply = hooks("Stop")[-1]["payload"].get("last_assistant_message") if len(hooks("Stop")) > n else None
    s.send("/exit"); time.sleep(0.3); s.send("\r"); wait_exit(s, 10); s.close()
    sid = hooks("SessionStart")[-1]["payload"]["session_id"]
    return reply, sid


def rollout(sid):
    p = next(home.glob(f"sessions/**/*{sid}.jsonl"))
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()]


def where(recs, word):
    """Which records mention word: (type, payload.type, role)."""
    hits = []
    for r in recs:
        if word in json.dumps(r):
            p = r.get("payload", {})
            hits.append([r.get("type"), p.get("type"), p.get("role")])
    return hits


# T1
cwd1 = make_cwd()
instr = make_cwd() / "instructions.md"  # outside the workspace
instr.write_text("You are a terse test assistant. The secret word is MARMALADE. "
                 "When asked for the secret word, reply with it only.\n")
r1, sid1 = one_turn(base_args(cwd1, logf) + ["-c", f"model_instructions_file={toml_str(str(instr))}"],
                    cwd1, "What is the secret word? Reply with the word only.")
recs = rollout(sid1)
meta = next(r["payload"] for r in recs if r["type"] == "session_meta")
bi = meta.get("base_instructions")
out["T1"] = {"reply": r1, "base_instructions_is_file": (bi.get("text") if isinstance(bi, dict) else bi or "").strip() == instr.read_text().strip(),
             "base_instructions_type": type(meta.get("base_instructions")).__name__,
             "base_instructions_head": json.dumps(meta.get("base_instructions"))[:300],
             "MARMALADE_in": where(recs, "MARMALADE")}

# T2
cwd2 = make_cwd()
(cwd2 / "AGENTS.md").write_text("The second secret word is QUINCE.\n")
dev = 'developer_instructions="The secret word is MARMALADE."'
r2, sid2 = one_turn(base_args(cwd2, logf) + ["-c", dev], cwd2,
                    "Reply with the secret word and the second secret word, separated by one space. Nothing else.")
recs = rollout(sid2)
meta = next(r["payload"] for r in recs if r["type"] == "session_meta")
out["T2"] = {"reply": r2, "MARMALADE_in": where(recs, "MARMALADE"), "QUINCE_in": where(recs, "QUINCE"),
             "base_instructions_head": json.dumps(meta.get("base_instructions"))[:200]}

# T3: resume with changed developer instructions
dev3 = 'developer_instructions="The secret word is BRAMBLE. Any earlier secret word is no longer valid."'
n = len(hooks("Stop"))
s = start(["resume", sid2] + base_args(cwd2, logf) + ["-c", dev3], cwd2, home)
wait_until(lambda: screen_state(s.lines())["input_box"], 30); s.wait_stable(1.0, 10)
s.paste("What is the secret word now? Reply with the word only."); s.send("\r")
wait_until(lambda: len(hooks("Stop")) > n, 120)
r3 = hooks("Stop")[-1]["payload"].get("last_assistant_message") if len(hooks("Stop")) > n else None
s.send("/exit"); time.sleep(0.3); s.send("\r"); wait_exit(s, 10); s.close()
recs = rollout(sid2)
out["T3"] = {"reply": r3, "BRAMBLE_in": where(recs, "BRAMBLE"), "MARMALADE_in": where(recs, "MARMALADE")}

(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
(CAP / "session-T2-T3.jsonl").write_text(sanitise_all("\n".join(json.dumps(r) for r in recs)) + "\n")
print(sanitise_all(json.dumps(out, indent=2)))
stop_daemons(home)

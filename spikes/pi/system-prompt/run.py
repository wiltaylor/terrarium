# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 4: --system-prompt and --append-system-prompt. Two model turns.

 T1 --system-prompt "<...QUINCE...>" and --append-system-prompt "<...MARMALADE...>"
    (text form). Ask for both words. The extension reports the full system
    prompt (TERRARIUM_FULL_PROMPT=1) in before_agent_start.
 T2 the same --session-id again with --append-system-prompt <file> that says
    BRAMBLE: is the file form read, and does a changed prompt apply on resume?
"""
import json, sys, time, uuid
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
d = make_pi_dir(); cwd = make_cwd(); sockp = d / "ev.sock"; es = EventSocket(sockp)
X = {"TERRARIUM_FULL_PROMPT": "1"}
sid = str(uuid.uuid4())
out = {}


def run(extra, prompt):
    n = len(es.of("agent_settled")); k = len(es.of("before_agent_start"))
    s = start(base_args() + ["--no-skills", "--session-id", sid] + extra, cwd, d, sockp, env_extra=X)
    wait_until(lambda: es.of("session_start"), 20); s.wait_stable(1.0, 10)
    s.paste(prompt); s.send("\r")
    wait_until(lambda: len(es.of("agent_settled")) > n, 120)
    s.send("\x04"); wait_exit(s, 5); s.close()
    bas = es.of("before_agent_start")[k]
    sp = bas.get("system_prompt") or ""
    return {"reply": es.of("message_end")[-1].get("last_assistant_message"),
            "system_prompt_len": len(sp), "system_prompt": sp,
            "has": {w: w in sp for w in ("QUINCE", "MARMALADE", "BRAMBLE")},
            "default_pi_text_kept": "operating inside pi" in sp}


out["T1"] = run(["--system-prompt", "You are a terse test assistant. The secret word is QUINCE.",
                 "--append-system-prompt", "The second secret word is MARMALADE."],
                "Reply with the secret word and the second secret word, separated by one space. Nothing else.")
f = make_cwd() / "append.md"
f.write_text("The second secret word is now BRAMBLE. Any earlier second word is no longer valid.\n")
out["T2"] = run(["--system-prompt", "You are a terse test assistant. The secret word is QUINCE.",
                 "--append-system-prompt", str(f)],
                "Reply with the secret word and the second secret word now, separated by one space. Nothing else.")
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
print(sanitise_all(json.dumps(out, indent=2)))

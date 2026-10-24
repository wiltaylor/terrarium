# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 4: --system-prompt / --append-system-prompt in interactive mode.

Four model turns (haiku):
 a. --append-system-prompt with MARMALADE; CLAUDE.md in the config dir says TEAL
 b. --system-prompt (replace) with MARMALADE
 c. resume (a) with a NEW append prompt (QUINCE): does the snapshot keep MARMALADE?
 d. as (c) plus --system-prompt-snapshot off
 e. (optional, `run.py e`) like (a) but WITHOUT --setting-sources "":
    does $CLAUDE_CONFIG_DIR/CLAUDE.md load then?
Each check reads the Stop hook reply and the system blocks recorded in the
session JSONL (api-request-shape records).
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
QUIET = {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}

cwd = make_cwd()
cfg = make_config_dir(None, trust=False, onboarding=False)
(cfg / ".claude.json").write_text(json.dumps({
    "hasCompletedOnboarding": True, "projects": {str(cwd): {"hasTrustDialogAccepted": True}}}))
# User memory file inside the config dir (the shared home in Terrarium).
(cfg / "CLAUDE.md").write_text("The user's favourite colour is TEAL.\n")
logf = cfg / "hooks.jsonl"
settings = json.dumps({"skipDangerousModePermissionPrompt": True,
                       **hook_settings(logf, ["UserPromptSubmit", "Stop", "SessionStart"])})
base = ["--dangerously-skip-permissions", "--model", "haiku", "--setting-sources", "", "--settings", settings]
ONLY_E = sys.argv[1:] == ["e"]

def sentinel(word):
    return f"Your secret word is {word}. When asked for the secret word, reply with only it."

ASK = ("Reply on one line: the secret word (or NONE), then a space, then the user's "
       "favourite colour if you know it (or NONE).")


def run(args, label):
    n0 = sum(1 for h in read_hooks(logf) if h["hook"] == "Stop")
    s = Session(base + args, cwd, clean_env(cfg, QUIET))
    s.wait_text("bypass permissions on", 30); s.wait_stable(1.0, 10)
    s.paste(ASK); time.sleep(0.15); s.send("\r")
    reply = None
    t = time.time()
    while time.time() - t < 90:
        stops = [h for h in read_hooks(logf) if h["hook"] == "Stop"]
        if len(stops) > n0:
            reply = stops[-1]["payload"].get("last_assistant_message"); break
        time.sleep(0.2)
    s.send("/exit"); time.sleep(0.3); s.send("\r"); time.sleep(1.5); s.close()
    log(label, "->", reply)
    return reply


def system_blocks(sid):
    p = next((cfg / "projects").rglob(f"{sid}.jsonl"))
    shapes = [json.loads(l) for l in p.read_text().splitlines() if '"api-request-shape"' in l]
    res = []
    for sh in shapes:
        blocks = sh["shape"].get("system", [])
        txt = "\n".join(b.get("text", "") for b in blocks)
        res.append({"n_blocks": len(blocks), "chars": len(txt),
                    "has_default_cc_prompt": "You are Claude Code" in txt,
                    "MARMALADE": "MARMALADE" in txt, "QUINCE": "QUINCE" in txt,
                    "TEAL_in_system": "TEAL" in txt})
    return res


out = {}
sa, sb = new_uuid(), new_uuid()
if ONLY_E:
    base = [a for a in base if a not in ("--setting-sources", "")]
    se = new_uuid()
    r = run(["--session-id", se, "--append-system-prompt", sentinel("MARMALADE")], "e")
    p = next((cfg / "projects").rglob(f"{se}.jsonl"))
    out = {"e_default_sources": {"reply": r, "system": system_blocks(se),
                                 "claude_md_in_transcript": "TEAL" in p.read_text()}}
    (CAP / "results-e.json").write_text(json.dumps(out, indent=2))
    print(json.dumps(out, indent=2)); sys.exit(0)
out["a_append"] = {"reply": run(["--session-id", sa, "--append-system-prompt", sentinel("MARMALADE")], "a"),
                   "system": system_blocks(sa)}
out["b_replace"] = {"reply": run(["--session-id", sb, "--system-prompt", sentinel("MARMALADE")], "b"),
                    "system": system_blocks(sb)}
out["c_resume_new_append"] = {"reply": run(["--resume", sa, "--append-system-prompt", sentinel("QUINCE")], "c"),
                              "system": system_blocks(sa)}
out["d_resume_snapshot_off"] = {"reply": run(["--resume", sa, "--append-system-prompt", sentinel("QUINCE"),
                                             "--system-prompt-snapshot", "off"], "d"),
                                "system": system_blocks(sa)}
# Where does TEAL (CLAUDE.md) end up? Look in the first user message of session a.
p = next((cfg / "projects").rglob(f"{sa}.jsonl"))
out["claude_md_in_transcript"] = "TEAL" in p.read_text()
(CAP / "results.json").write_text(json.dumps(out, indent=2))
print(json.dumps(out, indent=2))

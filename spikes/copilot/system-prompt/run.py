# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 4: system prompt through the CopilotCustomSystemPrompt extension.

Three model turns (claude-haiku-4.5). The npm package copilot-custom-system-prompt
goes into a temp npm prefix (--ignore-scripts). Its `install` runs with
COPILOT_HOME set to a temp home. The real ~/.copilot is not touched.
 T1 installed extension + APPEND_SYSTEM.md (MARMALADE), --experimental
 T2 no install: --plugin-dir <package dir> + SYSTEM.md (QUINCE), --experimental
 T3 resume the T2 session with SYSTEM.md changed to BRAMBLE: new prompt used?
The system.message records in events.jsonl show the prompt that was sent.
"""
import json, os, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
prefix = make_home("npm")
subprocess.run(["npm", "install", "-g", "--prefix", str(prefix), "--ignore-scripts",
                "copilot-custom-system-prompt@0.1.2"], check=True, capture_output=True, cwd="/tmp")
pkg = prefix / "lib/node_modules/copilot-custom-system-prompt"
out = {"package_version": json.loads((pkg / "package.json").read_text())["version"]}


def run_turn(home, args, cwd, prompt, resume=None):
    log_ = home / "hooks.jsonl"
    n = len([h for h in read_hooks(log_) if h["hook"] == "Stop"])
    a = base_args() + ["--experimental"] + args + (["--resume", resume] if resume else [])
    s = start(a, cwd, home)
    s.wait_text(r"Allow All", 40); s.wait_stable(1.0, 10)
    s.paste(prompt); s.send("\r")
    wait_until(lambda: len([h for h in read_hooks(log_) if h["hook"] == "Stop"]) > n, 120)
    time.sleep(1.5)
    s.send("/exit"); time.sleep(0.4); s.send("\r"); wait_exit(s, 10); s.close()
    st = [h for h in read_hooks(log_) if h["hook"] == "Stop"][-1]["payload"]
    sid = st["session_id"]
    recs = [json.loads(l) for l in (home / "session-state" / sid / "events.jsonl").read_text().splitlines() if l.strip()]
    reply = [r["data"].get("content") for r in recs if r["type"] == "assistant.message"][-1]
    sysmsgs = [r["data"] for r in recs if r["type"] == "system.message"]
    return sid, reply, sysmsgs, recs


def summarise(sysmsgs, words):
    res = []
    for m in sysmsgs:
        c = m.get("content") or ""
        res.append({"keys": sorted(m), "len": len(c), "head": c[:160],
                    "has": {w: (w in c) for w in words},
                    "has_default_copilot_text": "GitHub Copilot" in c or "Copilot CLI" in c})
    return res


# T1
home1 = make_copilot_home(hooks=hooks_json(Path("/dev/null")))
(home1 / "hooks/terrarium.json").write_text(json.dumps(hooks_json(home1 / "hooks.jsonl")))
p = subprocess.run([str(pkg / "dist/install.js"), "install"], env={**os.environ, "COPILOT_HOME": str(home1)},
                   capture_output=True, text=True)
out["install_stdout"] = sanitise_all(p.stdout.replace(str(home1), "$COPILOT_HOME"))
(home1 / "APPEND_SYSTEM.md").write_text("The secret word is MARMALADE. When asked for the secret word, reply with it only.\n")
cwd1 = make_cwd()
sid, reply, sm, _ = run_turn(home1, [], cwd1, "What is the secret word? Reply with the word only.")
out["T1"] = {"reply": reply, "system_messages": summarise(sm, ["MARMALADE"])}

# T2
home2 = make_copilot_home()
(home2 / "hooks").mkdir(); (home2 / "hooks/terrarium.json").write_text(json.dumps(hooks_json(home2 / "hooks.jsonl")))
(home2 / "SYSTEM.md").write_text("You are a terse test assistant. The secret word is QUINCE. "
                                 "When asked for the secret word, reply with it only.\n")
cwd2 = make_cwd()
sid2, reply, sm, _ = run_turn(home2, ["--plugin-dir", str(pkg)], cwd2, "What is the secret word? Reply with the word only.")
out["T2"] = {"reply": reply, "system_messages": summarise(sm, ["QUINCE"])}

# T3: resume with a changed SYSTEM.md
(home2 / "SYSTEM.md").write_text("You are a terse test assistant. The secret word is BRAMBLE. "
                                 "Any earlier secret word is no longer valid.\n")
_, reply, sm, recs = run_turn(home2, ["--plugin-dir", str(pkg)], cwd2,
                              "What is the secret word now? Reply with the word only.", resume=sid2)
out["T3"] = {"reply": reply, "system_messages": summarise(sm, ["QUINCE", "BRAMBLE"])}
(CAP / "results.json").write_text(sanitise_all(json.dumps(out, indent=2)))
(CAP / "events-T2-T3-system-messages.json").write_text(sanitise_all(json.dumps(sm, indent=2)))
print(sanitise_all(json.dumps(out, indent=2)))

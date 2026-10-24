# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 1b: which settings sources load hooks, and what --bare does.

No model turns. It uses SessionStart, which fires at startup.
Three hook sources, each tagged in the log:
  FLAG    -- --settings '<json>'
  USER    -- $CLAUDE_CONFIG_DIR/settings.json  (stands in for the user's own hooks)
  PROJECT -- <cwd>/.claude/settings.json       (stands in for a repo's hooks)
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)


def hooks_for(tag, logfile):
    cmd = f"{sys.executable} {HOOKLOG} {tag}:SessionStart {logfile}"
    return {"SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}]}


CASES = {
    "default":            [],
    "sources-user":       ["--setting-sources", "user"],
    "sources-project":    ["--setting-sources", "project"],
    "sources-empty":      ["--setting-sources", ""],
    "bare":               ["--bare"],
}

results = {}
for name, extra in CASES.items():
    cwd = make_cwd()
    cfg = make_config_dir(None, trust=False, onboarding=False)
    (cfg / ".claude.json").write_text(json.dumps({
        "hasCompletedOnboarding": True,
        "projects": {str(cwd): {"hasTrustDialogAccepted": True}}}))
    logf = cwd.parent / f"{cwd.name}.hooks.jsonl"
    (cfg / "settings.json").write_text(json.dumps({"hooks": hooks_for("USER", logf)}))
    (cwd / ".claude").mkdir()
    (cwd / ".claude" / "settings.json").write_text(json.dumps({"hooks": hooks_for("PROJECT", logf)}))
    flag = {"skipDangerousModePermissionPrompt": True, "hooks": hooks_for("FLAG", logf)}
    s = Session(["--dangerously-skip-permissions", "--model", "haiku",
                 "--settings", json.dumps(flag), *extra], cwd, clean_env(cfg))
    s.wait_stable(quiet=3, timeout=25)
    time.sleep(1)
    fired = sorted({h["hook"] for h in read_hooks(logf)})
    screen = s.text()
    (CAP / f"sources-screen-{name}.txt").write_text(sanitise(screen))
    s.close()
    first = next((l.strip() for l in screen.splitlines() if l.strip() and len(l.strip()) > 2), "")
    results[name] = {"args": extra, "hooks_fired": fired,
                     "idle_prompt": "bypass permissions on" in screen,
                     "not_logged_in": ("Not logged in" in screen or "/login" in screen
                                       or "API key" in screen)}
    log(name, results[name])
    logf.unlink(missing_ok=True)

(CAP / "sources-results.json").write_text(json.dumps(results, indent=2))

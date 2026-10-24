# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 3: yolo flag, first-run dialogs, trust pre-seed, quiet env.

No model turns. Each variant starts claude, waits for the screen to settle,
classifies the first screen, and saves it under captured/.
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import *

CAP = Path(__file__).parent / "captured"
CAP.mkdir(exist_ok=True)

SKIP = json.dumps({"skipDangerousModePermissionPrompt": True})
QUIET = {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_ERROR_REPORTING": "1",
         "DISABLE_UPDATES": "1", "CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY": "1",
         "DISABLE_INSTALLATION_CHECKS": "1"}

VARIANTS = {
    # name: (onboarding seeded, trust seeded, extra args, extra env)
    "A-fresh":            (False, False, [], {}),
    "B-onboarded":        (True,  False, [], {}),
    "C-onboarded-trust":  (True,  True,  [], {}),
    "D-all-seeded-skip":  (True,  True,  ["--settings", SKIP], {}),
    "E-D-plus-quiet-env": (True,  True,  ["--settings", SKIP], QUIET),
    "F-trust-only-skip":  (False, True,  ["--settings", SKIP], {}),
    "G-untrusted-skip":   (True,  False, ["--settings", SKIP], {}),
    # H: the minimal seed only: two keys, written by hand below.
    "H-minimal-seed":     ("minimal", "minimal", ["--settings", SKIP], QUIET),
    # I: trust seeded on the PARENT of the cwd. Does trust inherit?
    "I-parent-trust":     ("minimal", "parent", ["--settings", SKIP], QUIET),
}


def classify(t: str) -> str:
    if "Choose the text style" in t: return "theme-picker (onboarding)"
    if "Do you trust" in t or "trust this folder" in t or "Is this a project you" in t: return "workspace-trust dialog"
    if "Bypass Permissions mode" in t and "Yes, I accept" in t: return "bypass-permissions warning"
    if "bypass permissions on" in t.lower(): return "idle prompt (bypass on)"
    return "other"


results = {}
only = sys.argv[1:] or list(VARIANTS)
for name in only:
    ob, tr, args, env = VARIANTS[name]
    cwd = make_cwd()
    if ob == "minimal":
        cfg = make_config_dir(None, trust=False, onboarding=False)
        if tr == "parent":
            cwd = cwd / "sub" / "workspace"; cwd.mkdir(parents=True)
            trusted = cwd.parent.parent
        else:
            trusted = cwd
        (cfg / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "projects": {str(trusted): {"hasTrustDialogAccepted": True}}}))
    else:
        cfg = make_config_dir(cwd, trust=tr, onboarding=ob)
    s = Session(["--dangerously-skip-permissions", "--model", "haiku", *args], cwd,
                clean_env(cfg, env))
    s.wait_stable(quiet=2.5, timeout=25)
    t = s.text()
    kind = classify(t)
    (CAP / f"screen-{name}.txt").write_text(sanitise(t))
    after = json.loads((cfg / ".claude.json").read_text()) if (cfg / ".claude.json").exists() else {}
    results[name] = {"first_screen": kind, "t_settled_s": round(time.time() - s.t0, 1),
                     "changelogLastFetched_set": "changelogLastFetched" in after,
                     "keys_after": sorted(after)}
    s.close()
    log(name, "->", kind)
    remove_config_dir(cfg)

(CAP / "results.json").write_text(json.dumps(results, indent=2))
print(json.dumps(results, indent=2))

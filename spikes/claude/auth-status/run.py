# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Spike 7: `claude auth status` output for subscription detection.

Read-only. Never runs login or logout. Three cases:
  real      -- the user's real config (no CLAUDE_CONFIG_DIR override)
  copied    -- a temp CLAUDE_CONFIG_DIR with only .credentials.json copied in
  empty     -- a temp CLAUDE_CONFIG_DIR with nothing in it (logged out)
For each: default output, --json, --text, and the exit code.
"""
import json, os, subprocess, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import CLAUDE, clean_env, make_config_dir, sanitise

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
copied = make_config_dir(None, trust=False, onboarding=False)
empty = make_config_dir(None, trust=False, onboarding=False)
(empty / ".credentials.json").unlink(missing_ok=True)

real_env = clean_env(Path("/nonexistent")); real_env.pop("CLAUDE_CONFIG_DIR")
cases = {"real": real_env, "copied": clean_env(copied), "empty": clean_env(empty)}
out = {}
for name, env in cases.items():
    for flag in ([], ["--json"], ["--text"]):
        p = subprocess.run([CLAUDE, "auth", "status", *flag], env=env, capture_output=True, text=True, timeout=30)
        key = f"{name} {' '.join(flag) or '(default)'}"
        out[key] = {"rc": p.returncode, "stdout": sanitise(p.stdout), "stderr": sanitise(p.stderr)}
        print(f"--- {key} rc={p.returncode}\n{sanitise(p.stdout)}{sanitise(p.stderr)}")
# Shape of the credentials file (key names only, never values).
creds = json.loads((copied / ".credentials.json").read_text()) if (copied / ".credentials.json").exists() else {}
out["credentials_file_shape"] = {k: sorted(v) if isinstance(v, dict) else type(v).__name__ for k, v in creds.items()}
(CAP / "results.json").write_text(json.dumps(out, indent=2))

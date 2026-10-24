# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 7: `pi auth check` per provider, and token reuse. No model turns.

Never runs /login or /logout, never prints a credential (--credentials is
not passed), always --no-refresh. Cases:
  real     PI_CODING_AGENT_DIR unset (~/.pi/agent), provider names in auth.json
  empty    temp dir with auth.json = {}
  env-anthropic  empty + ANTHROPIC_OAUTH_TOKEN = Claude Code's access token
  codex-copy     temp auth.json with an openai-codex entry built from a copy
                 of ~/.codex/auth.json (refresh left empty; see pilib)
The one real Anthropic request (rejected: "out of extra usage") is in
captured/anthropic-oauth-env-400.txt (from the hooks spike, run 2).
"""
import json, subprocess, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
import pilib
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
PROVIDERS = ["anthropic", "openai-codex", "github-copilot", "google", "xai", "openrouter",
             "glm53-exl3", "vllm", "qwen38-helios"]
real = json.loads((REAL_DIR / "auth.json").read_text())
out = {"real_auth_json_providers": sorted(real),
       "real_auth_json_oauth_providers": sorted(k for k, v in real.items() if isinstance(v, dict) and v.get("type") == "oauth")}
empty = make_home("pi", {}, {"auth.json": "{}"})
codex = make_home("pi", {}, {"auth.json": json.dumps({"openai-codex": pilib.codex_credential()})})
(REAL := None)
cases = {
    "real": (harness_env({}), None),
    "empty": (harness_env({"PI_CODING_AGENT_DIR": str(empty), "PI_OFFLINE": "1"}), None),
    "env-anthropic": (harness_env({"PI_CODING_AGENT_DIR": str(empty), "PI_OFFLINE": "1", **claude_oauth_env()}), ["anthropic"]),
    "codex-copy": (harness_env({"PI_CODING_AGENT_DIR": str(codex), "PI_OFFLINE": "1"}), ["openai-codex"]),
}
for name, (env, only) in cases.items():
    res = {}
    for p in only or PROVIDERS:
        r = subprocess.run([PI, "auth", "check", "--provider", p, "--json", "--no-refresh"], env=env,
                           capture_output=True, text=True, timeout=60)
        res[p] = {"rc": r.returncode, "stdout": sanitise_all(r.stdout.strip())[:300],
                  "stderr": sanitise_all(r.stderr.strip())[:200]}
    out[name] = res
    print(name, json.dumps(res)[:600])
r = subprocess.run([PI, "auth", "check", "--provider", "openai-codex"], env=cases["codex-copy"][0],
                   capture_output=True, text=True, timeout=60)
out["codex-copy-text"] = {"rc": r.returncode, "stdout": sanitise_all(r.stdout), "stderr": sanitise_all(r.stderr)}
(CAP / "results.json").write_text(json.dumps(out, indent=2))

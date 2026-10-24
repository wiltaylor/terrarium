# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 7: `codex login status` output and where the plan shows.

Read-only. Never runs login or logout. Cases: real (~/.codex as is),
copied (temp CODEX_HOME with auth.json), empty (temp CODEX_HOME, nothing).
Also the claim names in the id_token JWT (values only for the plan claim).
"""
import base64, json, subprocess, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
copied = make_codex_home()
empty = make_home("codex")
real_env = harness_env()
cases = {"real": real_env, "copied": codex_env(copied), "empty": codex_env(empty)}
out = {}
for name, env in cases.items():
    p = subprocess.run([CODEX, "login", "status"], env=env, capture_output=True, text=True, timeout=30)
    out[name] = {"rc": p.returncode, "stdout": sanitise_all(p.stdout),
                 "stderr": sanitise_all("\n".join(l for l in p.stderr.splitlines() if "PATH aliases" not in l))}
    print(name, out[name])

auth = json.loads(REAL_AUTH.read_text())
out["auth_json_shape"] = {k: (sorted(v) if isinstance(v, dict) else type(v).__name__) for k, v in auth.items()}
idt = auth.get("tokens", {}).get("id_token", "")
if idt.count(".") == 2:
    seg = idt.split(".")[1]; seg += "=" * (-len(seg) % 4)
    claims = json.loads(base64.urlsafe_b64decode(seg))
    out["id_token_claim_names"] = sorted(claims)
    oa = claims.get("https://api.openai.com/auth", {})
    out["openai_auth_claim_names"] = sorted(oa)
    out["chatgpt_plan_type"] = oa.get("chatgpt_plan_type")
(CAP / "results.json").write_text(json.dumps(out, indent=2))
print(json.dumps({k: out[k] for k in out if k not in cases}, indent=2))

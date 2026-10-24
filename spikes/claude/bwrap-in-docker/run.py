# /// script
# requires-python = ">=3.11"
# ///
"""Spike 6: does bwrap work inside docker run, and with which options?

Builds the scratch image, then runs inside.sh under a matrix of
seccomp / capability / AppArmor / systempaths options. Writes captured/results.json.
"""
import json, subprocess, sys
from pathlib import Path

here = Path(__file__).resolve().parent
cap = here / "captured"; cap.mkdir(exist_ok=True)
IMG = "terrarium-spike-bwrap"
subprocess.run([sys.executable, str(here / "make_profiles.py")], check=True)
subprocess.run(["docker", "build", "-q", "-t", IMG, str(here)], check=True)

U = str(here / "seccomp-userns.json")
UM = str(here / "seccomp-userns-mount.json")
MATRIX = {
    "a default":                               [],
    "a2 default +SYS_ADMIN":                   ["--cap-add", "SYS_ADMIN"],
    "b seccomp=unconfined":                    ["--security-opt", "seccomp=unconfined"],
    "b2 seccomp=unconfined +SYS_ADMIN":        ["--security-opt", "seccomp=unconfined", "--cap-add", "SYS_ADMIN"],
    "b3 seccomp=unconfined systempaths=unconfined": ["--security-opt", "seccomp=unconfined", "--security-opt", "systempaths=unconfined"],
    "c custom(clone/unshare)":                 ["--security-opt", f"seccomp={U}"],
    "c2 custom(clone/unshare) +SYS_ADMIN":     ["--security-opt", f"seccomp={U}", "--cap-add", "SYS_ADMIN"],
    "c3 custom(clone/unshare/mount/pivot_root)": ["--security-opt", f"seccomp={UM}"],
    "c4 custom(+mount) systempaths=unconfined": ["--security-opt", f"seccomp={UM}", "--security-opt", "systempaths=unconfined"],
    "c5 custom(clone/unshare) systempaths=unconfined": ["--security-opt", f"seccomp={U}", "--security-opt", "systempaths=unconfined"],
    "c6 min-a(clone/unshare/mount/pivot_root) systempaths=unconfined": ["--security-opt", f"seccomp={here}/seccomp-min-a.json", "--security-opt", "systempaths=unconfined"],
    "c7 min-b(min-a+umount2) systempaths=unconfined": ["--security-opt", f"seccomp={here}/seccomp-min-b.json", "--security-opt", "systempaths=unconfined"],
    "d apparmor=unconfined only":              ["--security-opt", "apparmor=unconfined"],
}
res = {}
for name, opts in MATRIX.items():
    p = subprocess.run(["docker", "run", "--rm", *opts, IMG], capture_output=True, text=True, timeout=60)
    outp = (p.stdout + p.stderr).strip().replace(str(here), "<spike-dir>")
    res[name] = {"opts": [o.replace(str(here), "<spike-dir>") for o in opts], "rc": p.returncode, "output": outp.splitlines()}
    print(f"--- {name}  rc={p.returncode}\n{outp}\n", flush=True)
(cap / "results.json").write_text(json.dumps(res, indent=2))

# /// script
# requires-python = ">=3.11"
# ///
"""Derive two seccomp profiles from Docker's default profile.

docker-default-seccomp.json is a copy of
https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json
  seccomp-userns.json       : + clone, clone3, unshare allowed with no arg/cap filter
  seccomp-userns-mount.json : + mount, umount2, pivot_root, setns as well
  seccomp-min-a.json        : clone, clone3, unshare, mount, pivot_root
  seccomp-min-b.json        : min-a + umount2
"""
import copy, json
from pathlib import Path

here = Path(__file__).parent
base = json.loads((here / "docker-default-seccomp.json").read_text())


def derive(names, out):
    p = copy.deepcopy(base)
    # Drop the clone3 ENOSYS rule so the allow rule wins.
    p["syscalls"] = [s for s in p["syscalls"]
                     if not (s.get("action") == "SCMP_ACT_ERRNO" and "clone3" in s["names"])]
    p["syscalls"].append({"names": names, "action": "SCMP_ACT_ALLOW"})
    (here / out).write_text(json.dumps(p, indent=1))


derive(["clone", "clone3", "unshare"], "seccomp-userns.json")
derive(["clone", "clone3", "unshare", "mount", "umount2", "pivot_root", "setns"], "seccomp-userns-mount.json")
derive(["clone", "clone3", "unshare", "mount", "pivot_root"], "seccomp-min-a.json")
derive(["clone", "clone3", "unshare", "mount", "umount2", "pivot_root"], "seccomp-min-b.json")
print("wrote seccomp-userns.json, seccomp-userns-mount.json")

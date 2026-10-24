# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spikes 2 and 6: tool approval, project trust, quiet start. No model turns.

Tool approval: the hooks spike ran a bash tool with no prompt (no flag).
Here, project trust and the start-up screen. Each variant gets a fresh
PI_CODING_AGENT_DIR. "dialog" in the name means a prompt is expected.
  dialog-project-trust   cwd has .pi/settings.json, no flag
  idle-approve           same + --approve
  idle-no-approve        same + --no-approve
  idle-ext-trust         same + TERRARIUM_TRUST=yes (extension answers project_trust)
  idle-online            no --offline, no PI_OFFLINE: update notice? changelog?
  idle-offline-default   --offline, PI_TELEMETRY=0, skills and context files on
  idle-offline-bare      same + --no-skills --no-context-files
"""
import json, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import *

CAP = Path(__file__).parent / "captured"; CAP.mkdir(exist_ok=True)
ALL = ["dialog-project-trust", "idle-approve", "idle-no-approve", "idle-ext-trust",
       "idle-online", "idle-offline-default", "idle-offline-bare"]
want = sys.argv[1:] or ALL
results = {}
for v in want:
    d = make_pi_dir(); cwd = make_cwd(); sockp = d / "ev.sock"; es = EventSocket(sockp)
    args = base_args() + ["--no-skills"]
    extra = {}
    if v in ("dialog-project-trust", "idle-approve", "idle-no-approve", "idle-ext-trust"):
        (cwd / ".pi").mkdir(); (cwd / ".pi/settings.json").write_text("{}")
    if v == "idle-approve":
        args += ["--approve"]
    elif v == "idle-no-approve":
        args += ["--no-approve"]
    elif v == "idle-ext-trust":
        extra = {"TERRARIUM_TRUST": "yes"}
    elif v == "idle-online":
        args = [a for a in args if a != "--offline"]
        extra = {"PI_OFFLINE": "", "PI_TELEMETRY": ""}
    elif v == "idle-offline-default":
        args = [a for a in args if a != "--no-skills"]
    elif v == "idle-offline-bare":
        args += ["--no-context-files"]
    env = pi_env(d, sockp, extra)
    for k in [k for k, x in env.items() if x == ""]:
        env.pop(k)
    s = Session(args, cwd, env, binary=PI)
    t_ready = wait_until(lambda: state(s)["ready"] or state(s)["dialog"], 20, poll=0.02)
    s.wait_stable(1.5, 10)
    if v == "idle-online":
        time.sleep(6)
    first = s.text(); st = state(s)
    s.send("\x04"); wait_exit(s, 3)
    if s.alive:
        s.send("\x03"); s.send("\x04"); wait_exit(s, 3)
    s.close()
    (CAP / f"screen-{v}.txt").write_text(sanitise_all(first.rstrip()) + "\n")
    results[v] = {"first_ready_or_dialog_s": round(t_ready, 2) if t_ready else None,
                  "ready": st["ready"], "dialog": st["dialog"],
                  "events": es.names(),
                  "project_trust_events": [{k: e.get(k) for k in ("project_cwd",)} for e in es.of("project_trust")],
                  "sections_on_screen": [l.strip() for l in first.splitlines() if l.strip().startswith("[")],
                  "agent_dir_files": sorted(str(p.relative_to(d)) for p in d.rglob("*") if p.is_file()),
                  "settings_after": json.loads((d / "settings.json").read_text())}
    es.close()
    log(v, json.dumps(results[v])[:300])
old = json.loads((CAP / "results.json").read_text()) if (CAP / "results.json").exists() and len(want) < len(ALL) else {}
old.update(results)
(CAP / "results.json").write_text(sanitise_all(json.dumps(old, indent=2)))

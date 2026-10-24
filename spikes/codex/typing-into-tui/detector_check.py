# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Codex spike 5b: run screen_state() over every captured screen. No turns.

Uses screens-dim.txt (dim text blanked) as the dim mask for screens.txt.
Expected values come from the labels: idle-* ready, busy-* busy, typed-*
input text, and the yolo-and-trust screens (dialog or idle).
"""
import re, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from codexlib import screen_state

HERE = Path(__file__).parent


def split(path):
    parts = re.split(r"^===== (.+?) \(t=.*?\) =====\n", path.read_text(), flags=re.M)
    return {parts[i]: parts[i + 1].splitlines() for i in range(1, len(parts), 2)}


bad = 0
txt, dim = split(HERE / "captured/screens.txt"), split(HERE / "captured/screens-dim.txt")
for label, lines in txt.items():
    st = screen_state(lines, dim.get(label))
    exp = ("ready" if label.startswith("idle") else "busy" if label.startswith("busy")
           else "typed" if label.startswith("typed") else "ready" if label.startswith("resume") else None)
    ok = {"ready": st["ready"], "busy": st["busy"] and not st["ready"],
          "typed": bool(st["input_text"]) and not st["ready"], None: True}[exp]
    bad += not ok
    print(f"{'OK ' if ok else 'BAD'} {label:45} {exp!s:6} ready={st['ready']} busy={st['busy']} "
          f"dialog={st['dialog']!r} input={st['input_text']!r:.40}")

yt = HERE.parent / "yolo-and-trust/captured"
for f in sorted(yt.glob("screen-*.txt")):
    lines = f.read_text().split("===== after keys =====")[0].splitlines()
    st = screen_state(lines)
    exp_dialog = f.name in ("screen-plain-git.txt", "screen-yolo-git.txt", "screen-hooks-notrust.txt")
    # without a dim mask the placeholder counts as input; use input_box instead
    ok = (st["dialog"] is not None) == exp_dialog and (st["input_box"] != exp_dialog)
    bad += not ok
    print(f"{'OK ' if ok else 'BAD'} {f.name:45} dialog={st['dialog']!r} input_box={st['input_box']}")
# resume dialogs from the session spike: the cwd chooser and "open in another app"
ss = split(HERE.parent / "session-id-and-resume/captured/screens.txt")
for label in ("C-resume-other-cwd", "D-second"):
    st = screen_state(ss[label])
    ok = st["dialog"] is not None and not st["ready"]
    bad += not ok
    print(f"{'OK ' if ok else 'BAD'} {label:45} dialog={st['dialog']!r}")
print("mismatches:", bad)

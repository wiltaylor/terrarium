# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Copilot spike 5b: screen_state() over every captured screen. No turns.

Labels: idle-* ready, busy-* busy, typed-* input text, dialog-* dialog,
lag-* informational (the screen had not drawn the paste yet).
Also the yolo-and-trust screens (trust dialog or idle).
"""
import re, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from copilotlib import screen_state

HERE = Path(__file__).parent


def split(path):
    parts = re.split(r"^===== (.+?) \(t=.*?\) =====\n", path.read_text(), flags=re.M)
    return {parts[i]: parts[i + 1].splitlines() for i in range(1, len(parts), 2)}


bad = 0
txt, dim = split(HERE / "captured/screens.txt"), split(HERE / "captured/screens-dim.txt")
for label, lines in txt.items():
    st = screen_state(lines, dim.get(label))
    kind = label.split("-")[0]
    ok = {"idle": st["ready"], "busy": st["busy"] and not st["ready"],
          "typed": bool(st["input_text"]) and not st["ready"],
          "dialog": st["dialog"] is not None and not st["ready"]}.get(kind, True)
    bad += not ok
    print(f"{'OK ' if ok else 'BAD'} {label:28} ready={st['ready']} busy={st['busy']} "
          f"dialog={st['dialog']!r:.40} input={st['input_text']!r:.30}")
yt = HERE.parent / "yolo-and-trust/captured"
for f in sorted(yt.glob("screen-*.txt")):
    st = screen_state(f.read_text().splitlines())
    exp_dialog = "dialog" in f.name
    ok = (st["dialog"] is not None) == exp_dialog and st["ready"] != exp_dialog
    bad += not ok
    print(f"{'OK ' if ok else 'BAD'} {f.name:28} ready={st['ready']} dialog={st['dialog']!r:.50}")
print("mismatches:", bad)

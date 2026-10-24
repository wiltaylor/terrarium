# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""pi spike 5b: screen_state() over every captured screen. No turns.

Labels: idle-* ready, busy-* busy, typed-* input text, early-* (0.3 s after
spawn) expected ready. Also the session spike's "fork?" prompt as a dialog.
"""
import re, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from pilib import screen_state

HERE = Path(__file__).parent


def split(path):
    parts = re.split(r"^===== (.+?) \(t=.*?\) =====\n", path.read_text(), flags=re.M)
    return {parts[i]: parts[i + 1].splitlines() for i in range(1, len(parts), 2)}


bad = 0
txt, dim = split(HERE / "captured/screens.txt"), split(HERE / "captured/screens-dim.txt")
for label, lines in txt.items():
    st = screen_state(lines, dim.get(label))
    kind = label.split("-")[0]
    ok = {"idle": st["ready"], "early": st["ready"], "busy": st["busy"] and not st["ready"],
          "typed": bool(st["input_text"]) and not st["ready"]}.get(kind, True)
    bad += not ok
    print(f"{'OK ' if ok else 'BAD'} {label:28} ready={st['ready']} busy={st['busy']} "
          f"dialog={st['dialog']!r:.30} input={st['input_text']!r:.30}")
ss = split(HERE.parent / "session-id-and-resume/captured/screens.txt")
st = screen_state(ss["C-session-flag-other-cwd"])
ok = st["dialog"] is not None and not st["ready"]; bad += not ok
print(f"{'OK ' if ok else 'BAD'} {'C-session-flag-other-cwd':28} dialog={st['dialog']!r}")
print("mismatches:", bad)

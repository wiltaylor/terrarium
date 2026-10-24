# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "pyte>=0.8.2"]
# ///
"""Offline check: run screen_state() over every captured screen. No claude run."""
import re, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from ptyharness import screen_state

root = Path(__file__).resolve().parent.parent
for f in [root / "typing-into-tui/captured/screens.txt", root / "hooks-in-interactive/captured/screens.txt"]:
    parts = re.split(r"===== (.*?) =====\n", f.read_text())
    for i in range(1, len(parts), 2):
        st = screen_state(parts[i + 1].splitlines()); st.pop("footer")
        st["input_text"] = (st["input_text"] or "")[:30]
        print(parts[i][:40].ljust(40), st)
for f in sorted((root / "yolo-and-trust/captured").glob("screen-*.txt")):
    st = screen_state(f.read_text().splitlines()); st.pop("footer")
    print(f.name[:40].ljust(40), st)

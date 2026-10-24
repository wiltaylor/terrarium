"""Re-run sanitise() over captured files.

Usage: python resanitise.py [--all] <files>
--all also applies sanitise_all() (codex, copilot, pi homes and tokens).
"""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from ptyharness import sanitise, sanitise_all
args = sys.argv[1:]
fn = sanitise
if args and args[0] == "--all":
    fn, args = sanitise_all, args[1:]
for f in args:
    p = Path(f); p.write_text(fn(p.read_text()))

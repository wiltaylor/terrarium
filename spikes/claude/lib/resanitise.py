"""Re-run sanitise() over captured files. Usage: python resanitise.py <files>"""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from ptyharness import sanitise
for f in sys.argv[1:]:
    p = Path(f); p.write_text(sanitise(p.read_text()))

#!/usr/bin/env python3
"""Hook logger: append {hook, ts, payload} to a JSONL file. Stdlib only.

Usage (as a Claude Code hook command): hooklog.py <EventName> <logfile>
"""
import json
import sys
import time

ev, path = sys.argv[1], sys.argv[2]
raw = sys.stdin.read()
try:
    payload = json.loads(raw)
except Exception:
    payload = {"_raw": raw}
with open(path, "a") as f:
    f.write(json.dumps({"hook": ev, "ts": time.time(), "payload": payload}) + "\n")
sys.exit(0)

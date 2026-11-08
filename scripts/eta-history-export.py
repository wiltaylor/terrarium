#!/usr/bin/env python3
"""Read-only card history export for the ETA backtest. Never commit the output."""
import json
import subprocess
import sys


def read(*args):
    return json.loads(subprocess.check_output(["terrarium", *args, "--json"]))


def main():
    cards = read("card", "ls")
    # Only the minimum history fields needed by the store test are retained.
    result = []
    for card in cards:
        events = read("card", "events", card["id"])
        result.append({
            "id": card["id"], "labels": card["labels"],
            "events": [{"seq": e["seq"], "at": e["at"], "project": e["project"],
                        "data": {k: v for k, v in e["data"].items()
                                 if k in ("change", "eta_at", "job", "labels", "from", "to", "column", "column_role")}}
                       for e in events if e["kind"] == "card"],
        })
    with open(sys.argv[1], "w") as output:
        json.dump(result, output)
    print(f"Exported {len(result)} cards")


if __name__ == "__main__":
    main()

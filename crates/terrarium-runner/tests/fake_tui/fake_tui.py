#!/usr/bin/env python3
"""A fake harness TUI for the runner tests.

It prints a `> ` prompt and reads a line. For a normal line it sends the
UserPromptSubmit hook, prints `working...`, waits, prints `done: <text>`, and
sends the Stop hook. Then it prints the prompt again.

Commands:
  /quit       exit 0 with no hooks
  /stall      submit, then stay silent for FAKE_TUI_STALL_MS before Stop
  /question   submit, send a Notification hook, wait for one answer line

Env:
  TERRARIUM_SOCKET     the runner socket (the runner sets it)
  FAKE_TUI_DELAY_MS    the work time of a normal turn (default 300)
  FAKE_TUI_STALL_MS    the silent time of /stall (default 1500)
  FAKE_TUI_NO_SUBMIT   when "1", never send UserPromptSubmit or Stop
"""

import json
import os
import socket
import sys
import time

SOCK = os.environ.get("TERRARIUM_SOCKET")
DELAY = float(os.environ.get("FAKE_TUI_DELAY_MS", "300")) / 1000
STALL = float(os.environ.get("FAKE_TUI_STALL_MS", "1500")) / 1000
NO_SUBMIT = os.environ.get("FAKE_TUI_NO_SUBMIT") == "1"


def hook(name, payload):
    """The tiny inline client: one JSON line per connection."""
    if not SOCK:
        return
    msg = json.dumps({"type": "hook", "hook": name, "payload": payload}) + "\n"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.connect(SOCK)
            s.sendall(msg.encode())
    except OSError as e:
        sys.stderr.write(f"hook {name} failed: {e}\n")


def out(text):
    sys.stdout.write(text)
    sys.stdout.flush()


def main():
    out("fake tui 1.0\n")
    while True:
        out("> ")
        line = sys.stdin.readline()
        if not line:
            return 0
        text = line.replace("\x1b[200~", "").replace("\x1b[201~", "").strip()
        if not text:
            continue
        if text == "/quit":
            out("bye\n")
            return 0
        if NO_SUBMIT:
            out("ignored\n")
            continue
        hook("UserPromptSubmit", {"prompt": text})
        out("working...\n")
        if text == "/stall":
            time.sleep(STALL)
        elif text == "/question":
            hook("Notification", {"message": "Allow the tool?"})
            out("Allow the tool? [y/n] ")
            answer = sys.stdin.readline().strip()
            out(f"answer: {answer}\n")
        else:
            time.sleep(DELAY)
        out(f"done: {text}\n")
        hook("Stop", {})


if __name__ == "__main__":
    sys.exit(main())

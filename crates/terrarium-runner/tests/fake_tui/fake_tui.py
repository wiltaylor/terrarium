#!/usr/bin/env python3
"""A fake harness TUI for the runner tests.

It prints a `> ` prompt and reads a line. For a normal line it sends the
UserPromptSubmit hook, prints `working...`, waits, prints `done: <text>`, and
sends the Stop hook. Then it prints the prompt again.

Arguments:
  --session ID   send a SessionStart hook (source "startup") with this ID
  --resume ID    send a SessionStart hook (source "resume") with this ID

Commands:
  /quit       exit 0 with no hooks
  /exit N     exit with code N, with no hooks
  /stall      submit, then stay silent for FAKE_TUI_STALL_MS before Stop
  /question   submit, send a Notification hook, wait for one answer line

Env:
  TERRARIUM_SOCKET     the runner socket (the runner sets it)
  FAKE_TUI_DELAY_MS    the work time of a normal turn (default 300)
  FAKE_TUI_STALL_MS    the silent time of /stall (default 1500)
  FAKE_TUI_NO_SUBMIT   when "1", never send UserPromptSubmit or Stop
  FAKE_TUI_NO_SUBMIT_HOOK  when "1", do the turn and send Stop, but never
                       send UserPromptSubmit (only the screen shows the turn)
  FAKE_TUI_RESUME_FAIL when "1", `--resume` prints an error and exits 1
  FAKE_TUI_EXIT_AFTER_MS  when set, exit with FAKE_TUI_EXIT_CODE (default 1)
                       this long after the start, with no hooks
"""

import json
import os
import socket
import sys
import threading
import time

SOCK = os.environ.get("TERRARIUM_SOCKET")
DELAY = float(os.environ.get("FAKE_TUI_DELAY_MS", "300")) / 1000
STALL = float(os.environ.get("FAKE_TUI_STALL_MS", "1500")) / 1000
NO_SUBMIT = os.environ.get("FAKE_TUI_NO_SUBMIT") == "1"
NO_SUBMIT_HOOK = os.environ.get("FAKE_TUI_NO_SUBMIT_HOOK") == "1"


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


def arg(name):
    if name in sys.argv:
        i = sys.argv.index(name)
        if i + 1 < len(sys.argv):
            return sys.argv[i + 1]
    return None


def main():
    out("fake tui 1.0\n")
    resume = arg("--resume")
    session = resume or arg("--session")
    if resume and os.environ.get("FAKE_TUI_RESUME_FAIL") == "1":
        out(f"No conversation found with session ID: {resume}\n")
        return 1
    if session:
        source = "resume" if resume else "startup"
        out(f"session {session} ({source})\n")
        hook("SessionStart", {"session_id": session, "source": source})
    exit_after = os.environ.get("FAKE_TUI_EXIT_AFTER_MS")
    if exit_after:
        code = int(os.environ.get("FAKE_TUI_EXIT_CODE", "1"))

        def die():
            time.sleep(float(exit_after) / 1000)
            sys.stdout.flush()
            os._exit(code)

        threading.Thread(target=die, daemon=True).start()
    while True:
        out("> ")
        line = sys.stdin.readline()
        if not line:
            return 0
        # A bracketed paste with newlines is one turn.
        while "\x1b[200~" in line and "\x1b[201~" not in line:
            more = sys.stdin.readline()
            if not more:
                break
            line += more
        text = line.replace("\x1b[200~", "").replace("\x1b[201~", "").strip()
        if not text:
            continue
        if text == "/quit":
            out("bye\n")
            return 0
        if text.startswith("/exit "):
            out("bye\n")
            return int(text.split()[1])
        if NO_SUBMIT:
            out("ignored\n")
            continue
        if not NO_SUBMIT_HOOK:
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

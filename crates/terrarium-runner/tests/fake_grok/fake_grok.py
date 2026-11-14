#!/usr/bin/env python3
"""A fake Grok Build TUI for the grok adapter lifecycle test (TER-1192).

It is a fixture: it follows the documented grok surface (docs.x.ai,
"Hooks", "Keyboard shortcuts", the 1.0.77 help), not a capture of a
signed-in grok. It sends the hooks the way Terrarium's personal hook file
makes grok send them (`terrarium hook <Name>` with the JSON on stdin): one
JSON line per connection to TERRARIUM_SOCKET, with the documented
camelCase fields `hookEventName`, `sessionId`, `cwd`, `workspaceRoot`.

Arguments (the ones the adapter passes):
  --session-id UUID   a new conversation: SessionStart, source "startup"
  --resume ID         SessionStart, source "resume"
  --cwd, --model, --append-system-prompt, --system-prompt  read and shown

Input (cooked mode; the paste markers are removed):
  /quit           exactly: send SessionEnd and exit 0 (the exit keys)
  hold ...        a turn that runs until Ctrl-C (SIGINT): it prints
                  `cancelled` and the prompt, with no Stop hook
  any other line  UserPromptSubmit, `working...`, `done: <text>`, Stop

It exits with code 3 when a permission flag (--always-approve, --yolo,
--trust, --dangerously-skip-permissions, --permission-mode) is passed.

Env:
  TERRARIUM_SOCKET   the runner socket (the runner sets it)
  GROK_HOME          must be set (the adapter sets it); shown at start
"""

import json
import os
import signal
import socket
import sys
import time

SOCK = os.environ.get("TERRARIUM_SOCKET")
FORBIDDEN = (
    "--always-approve",
    "--yolo",
    "--trust",
    "--dangerously-skip-permissions",
    "--permission-mode",
)
cancel = False
busy = False


def arg(name):
    if name in sys.argv:
        i = sys.argv.index(name)
        if i + 1 < len(sys.argv):
            return sys.argv[i + 1]
    return None


SESSION = arg("--session-id") or arg("--resume") or ""
CWD = arg("--cwd") or os.getcwd()


def hook(name, extra=None):
    if not SOCK:
        return
    body = {
        "hookEventName": name,
        "sessionId": SESSION,
        "cwd": CWD,
        "workspaceRoot": CWD,
    }
    body.update(extra or {})
    msg = json.dumps({"type": "hook", "hook": name, "payload": body}) + "\n"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.connect(SOCK)
            s.sendall(msg.encode())
    except OSError as e:
        sys.stderr.write(f"hook {name} failed: {e}\n")


def out(text):
    sys.stdout.write(text)
    sys.stdout.flush()


def on_sigint(_sig, _frame):
    global cancel
    if busy:
        cancel = True


def main():
    global busy, cancel
    if any(a in FORBIDDEN for a in sys.argv):
        out("refused: a permission flag\r\n")
        sys.exit(3)
    if not os.environ.get("GROK_HOME"):
        out("no GROK_HOME\r\n")
        sys.exit(4)
    signal.signal(signal.SIGINT, on_sigint)
    prompt_flag = "append" if arg("--append-system-prompt") else (
        "replace" if arg("--system-prompt") else "none")
    out(f"fake grok 1.0.77 model={arg('--model') or '-'} prompt={prompt_flag}\r\n")
    source = "resume" if arg("--resume") else "startup"
    hook("SessionStart", {"source": source})
    while True:
        out("> ")
        line = sys.stdin.readline()
        if not line:
            hook("SessionEnd", {"reason": "eof"})
            return
        text = line.replace("\x1b[200~", "").replace("\x1b[201~", "").rstrip("\r\n")
        if text == "/quit":
            hook("SessionEnd", {"reason": "exit"})
            out("bye\r\n")
            sys.exit(0)
        if not text.strip():
            continue
        hook("UserPromptSubmit", {"prompt": text})
        out("working...\r\n")
        busy = True
        cancel = False
        if text.startswith("hold"):
            while not cancel:
                time.sleep(0.02)
        else:
            hook("PreToolUse", {"toolName": "Bash", "toolInput": {"command": "true"}})
            time.sleep(0.2)
            hook("PostToolUse", {"toolName": "Bash", "toolInput": {"command": "true"}})
        busy = False
        if cancel:
            cancel = False
            out("cancelled\r\n")
            continue
        out(f"done: {text}\r\n")
        hook("Stop")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A fake Grok Build TUI for the grok adapter lifecycle test (TER-1192).

It is a fixture that follows the signed-in grok 1.0.77 capture (TER-1312,
`crates/adapters/grok/tests/fixtures/`). It sends the hooks the way
Terrarium's personal hook file makes grok send them (`terrarium hook
<Name>` with the JSON on stdin): one JSON line per connection to
TERRARIUM_SOCKET. Like grok, each field comes twice: `hookEventName` in
snake_case (`pre_tool_use`) beside `hook_event_name` in PascalCase, and
`sessionId` beside `session_id`.

Each state redraws the whole screen like grok's TUI: the recent lines,
the busy status line (`⠋ Waiting for response… [stop]`) during a turn,
the input box with `❯`, and the key hint line (`Ctrl+c:cancel` during a
turn). The cursor waits in the box, so typed text shows there.

Arguments (the ones the adapter passes):
  --session-id UUID   a new conversation: SessionStart, source "new"
  --resume ID         SessionStart, source "load"
  --cwd, --model, --append-system-prompt, --system-prompt  read and shown

Input (cooked mode; the paste markers are removed):
  /quit           exactly: send SessionEnd, then Stop with reason
                  `shutdown` (as grok), and exit 0 (the exit keys)
  hold ...        a turn that runs until Ctrl-C (SIGINT): it prints
                  `cancelled` and the idle box, with no Stop hook (as
                  grok)
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
LOG = []
COLS = 80
IDLE_HINT = "Shift+Tab:mode  │  Ctrl+x:shortcuts"
BUSY_HINT = "Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+x:shortcuts"


def arg(name):
    if name in sys.argv:
        i = sys.argv.index(name)
        if i + 1 < len(sys.argv):
            return sys.argv[i + 1]
    return None


SESSION = arg("--session-id") or arg("--resume") or ""
CWD = arg("--cwd") or os.getcwd()


def snake(name):
    return "".join("_" + c.lower() if c.isupper() else c for c in name).lstrip("_")


def hook(name, extra=None):
    if not SOCK:
        return
    body = {
        "hookEventName": snake(name),
        "hook_event_name": name,
        "sessionId": SESSION,
        "session_id": SESSION,
        "cwd": CWD,
        "workspaceRoot": CWD,
        "permissionMode": "default",
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


def frame(status=None):
    """Redraw the screen: recent lines, status, box, key hints."""
    rows = LOG[-10:]
    text = "\x1b[2J\x1b[H" + "".join(f"{l}\r\n" for l in rows)
    if status:
        text += f"   ⠋ {status}… 0.0s [stop]\r\n"
    inner = COLS - 6
    text += "\r\n  ╭" + "─" * inner + "╮\r\n"
    text += "  │ ❯" + " " * (inner - 2) + "│\r\n"
    text += "  ╰" + "─" * inner + "╯\r\n\r\n"
    text += "  " + (BUSY_HINT if status else IDLE_HINT) + "\r\n"
    box_row = len(rows) + (1 if status else 0) + 3
    text += f"\x1b[{box_row};7H"
    out(text)


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
    LOG.append(f"fake grok 1.0.77 model={arg('--model') or '-'} prompt={prompt_flag}")
    source = "load" if arg("--resume") else "new"
    hook("SessionStart", {"source": source})
    while True:
        frame()
        line = sys.stdin.readline()
        if not line:
            hook("SessionEnd", {"reason": "eof"})
            return
        text = line.replace("\x1b[200~", "").replace("\x1b[201~", "").rstrip("\r\n")
        if text == "/quit":
            hook("SessionEnd", {"reason": "shutdown"})
            hook("Stop", {"reason": "shutdown"})
            out("\x1b[2J\x1b[Hbye\r\nResume this session with:\r\n")
            out(f"  grok --resume {SESSION}\r\n")
            sys.exit(0)
        if not text.strip():
            continue
        hook("UserPromptSubmit", {"prompt": text})
        LOG.append(f"❯ {text}")
        frame("Waiting for response")
        busy = True
        cancel = False
        if text.startswith("hold"):
            while not cancel:
                time.sleep(0.02)
        else:
            tool = {"toolName": "run_terminal_command", "toolUseId": "call-1-0",
                    "toolInput": {"command": "true", "description": "Run true"}}
            hook("PreToolUse", tool)
            time.sleep(0.2)
            hook("PostToolUse", tool)
        busy = False
        if cancel:
            cancel = False
            LOG.append("Turn cancelled by user. cancelled")
            continue
        LOG.append(f"done: {text}")
        hook("Stop", {"reason": "end_turn"})


if __name__ == "__main__":
    main()

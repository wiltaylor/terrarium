"""Shared helpers for the GitHub Copilot CLI spikes (1.0.103).

Reuses spikes/claude/lib/ptyharness.py. Nothing here writes ~/.copilot.
The OAuth token is in the system keychain (libsecret), not in a file, so a
temp COPILOT_HOME needs no copied credential.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "claude" / "lib"))
from ptyharness import *  # noqa: F401,F403

COPILOT = os.environ.get("COPILOT_BIN", str(Path.home() / ".local/share/npm-global/bin/copilot"))
MODEL = os.environ.get("COPILOT_MODEL", "claude-haiku-4.5")  # 0.33x; gpt-5-mini (0x) is "Unavailable"

# Event names from the user's existing hook file (PascalCase, Claude style).
HOOK_EVENTS = ["SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse",
               "PostToolUse", "PostToolUseFailure", "SubagentStart", "SubagentStop",
               "PreCompact", "Stop", "ErrorOccurred", "PermissionRequest", "Notification"]
# camelCase names from the GitHub docs for .github/hooks/*.json
HOOK_EVENTS_CAMEL = ["sessionStart", "sessionEnd", "userPromptSubmitted", "preToolUse",
                     "postToolUse", "errorOccurred", "agentStop", "subagentStop"]


def hooks_json(logfile: Path, events=HOOK_EVENTS) -> dict:
    return {"version": 1, "hooks": {ev: [{"type": "command", "bash": hook_cmd(ev, logfile),
                                          "timeoutSec": 10}] for ev in events}}


def make_copilot_home(settings: dict | None = None, hooks: dict | None = None) -> Path:
    files = {}
    if settings is not None:
        files["settings.json"] = json.dumps(settings, indent=2)
    if hooks is not None:
        files["hooks/terrarium.json"] = json.dumps(hooks, indent=2)
    return make_home("copilot", {}, files)


QUIET_ENV = {"COPILOT_AUTO_UPDATE": "false", "COPILOT_DISABLE_TERMINAL_TITLE": "1"}


def copilot_env(home: Path, allow_all: bool = True, extra: dict | None = None) -> dict:
    e = {"COPILOT_HOME": str(home), **QUIET_ENV}
    if allow_all:
        e["COPILOT_ALLOW_ALL"] = "true"
    return harness_env({**e, **(extra or {})})


def base_args(model: str = MODEL, yolo: bool = True) -> list[str]:
    a = ["--model", model, "--disable-client-telemetry", "--no-auto-update"]
    if yolo:
        a += ["--yolo"]
    return a


def start(args, cwd, home, allow_all=True, env_extra=None, rawlog=None) -> Session:
    return Session(args, cwd, copilot_env(home, allow_all, env_extra), binary=COPILOT, rawlog=rawlog)


# Screen-state detector for the copilot TUI (1.0.103, alt screen).
# Idle bottom of the screen:
#   ──────────── (rule)
#   ❯ <input>
#   ──────────── (rule)
#    ← open sidebar · Interactive · Allow All · / commands · ? help ...   <model>
# Busy and dialog markers are refined from the typing-into-tui captures.
RULE_RX = re.compile(r"^\s*─{20,}\s*$")
# Busy footer: " ◉ Working esc esc edit prompt" / " ◎ Working · 256 B esc esc interrupt · ctrl+q enqueue"
BUSY_RX = re.compile(r"^\s*[◉◎○●]\s+Working\b|esc esc interrupt", re.M)
# Dialogs: folder trust "↑/↓ to navigate · enter to select · esc to cancel",
# ask_user "↑/↓ select · enter accept · ctrl+d decline · esc cancel",
# model picker "↑/↓ to navigate · ... · enter to select · esc to cancel".
DIALOG_RX = re.compile(r"↑/↓ (to )?(navigate|select)|enter (to )?(select|accept|confirm)", re.I)


def screen_state(lines: list[str], dim: list[str] | None = None) -> dict:
    st = {"input_box": False, "input_text": None, "busy": False, "ready": False,
          "dialog": None, "footer": None}
    nz = [l for l in lines if l.strip()]
    for l in nz[-6:]:
        if DIALOG_RX.search(l):
            st["dialog"] = l.strip()[:80]
    rules = [i for i, l in enumerate(lines) if RULE_RX.match(l)]
    for j in range(len(rules) - 1, 0, -1):
        a, b = rules[j - 1], rules[j]
        inner = lines[a + 1:b]
        if inner and inner[0].lstrip().startswith("❯"):
            src = dim[a + 1:b] if dim is not None else inner
            txt = "\n".join(src)
            st["input_box"] = True
            st["input_text"] = txt.split("❯", 1)[1].strip() if "❯" in txt else "\n".join(inner).split("❯", 1)[1].strip()
            footer = "\n".join(l for l in lines[b + 1:] if l.strip())
            st["footer"] = footer
            above = "\n".join(lines[max(0, a - 3):a])
            st["busy"] = bool(BUSY_RX.search(footer) or BUSY_RX.search(above))
            break
    st["ready"] = st["input_box"] and not st["busy"] and st["dialog"] is None and st["input_text"] == ""
    return st


def state(sess) -> dict:
    return screen_state(sess.lines(), sess.dim_mask())


def wait_ready(sess, timeout: float = 40, stable: float = 1.0):
    t0 = time.time(); since = None
    while time.time() - t0 < timeout:
        if state(sess)["ready"]:
            since = since or time.time()
            if time.time() - since >= stable:
                return since - t0
        else:
            since = None
        time.sleep(0.05)
    return None

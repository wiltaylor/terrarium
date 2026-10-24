"""Shared helpers for the Codex CLI spikes (codex-cli 0.160.0).

Reuses spikes/claude/lib/ptyharness.py (PTY, pyte screen, hook logger).
Nothing here reads or writes ~/.codex except one read-only copy of
~/.codex/auth.json into a temp CODEX_HOME.
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
from ptyharness import _CREATED  # noqa: F401

CODEX = os.environ.get("CODEX_BIN", str(Path.home() / ".local/bin/codex"))
REAL_AUTH = Path.home() / ".codex" / "auth.json"
MODEL = os.environ.get("CODEX_MODEL", "gpt-6-luna")  # "Fast and affordable"
EFFORT = "low"

# Hook events in 0.160.0 (event list in the binary). "Notification" is not
# one of them: codex counts 11 hooks when it is passed with these 11.
HOOK_EVENTS = ["SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse",
               "PostToolUse", "PermissionRequest", "Stop", "SubagentStart",
               "SubagentStop", "PreCompact", "PostCompact"]


def make_codex_home(config_toml: str = "") -> Path:
    """Temp CODEX_HOME with auth.json copied in and an optional config.toml."""
    files = {"config.toml": config_toml} if config_toml else {}
    return make_home("codex", {REAL_AUTH: "auth.json"}, files)


def toml_str(s: str) -> str:
    return json.dumps(s)  # a JSON string is a valid TOML basic string


def hook_overrides(logfile: Path, events=HOOK_EVENTS) -> list[str]:
    """-c overrides that register one logging hook per event (session flags)."""
    out = []
    for ev in events:
        cmd = toml_str(hook_cmd(ev, logfile))
        out += ["-c", f'hooks.{ev}=[{{hooks=[{{type="command",command={cmd}}}]}}]']
    return out


def trust_override(cwd: Path) -> list[str]:
    # The dotted path is split on "."; the cwd must not contain a dot.
    assert "." not in str(cwd), cwd
    return ["-c", f'projects.{cwd}.trust_level="trusted"']


def base_args(cwd: Path, logfile: Path | None = None, model: str = MODEL,
              yolo: bool = True, quiet: bool = True) -> list[str]:
    a = ["--no-daemon", "-m", model, "-c", f'model_reasoning_effort="{EFFORT}"']
    if yolo:
        a += ["--dangerously-bypass-approvals-and-sandbox"]
    if logfile is not None:
        a += ["--dangerously-bypass-hook-trust", *hook_overrides(logfile)]
    a += trust_override(cwd)
    if quiet:
        a += ["-c", "check_for_update_on_startup=false",
              "-c", "analytics.enabled=false",
              "-c", "feedback.enabled=false"]
    return a


def codex_env(home: Path, extra: dict | None = None) -> dict:
    return harness_env({"CODEX_HOME": str(home), **(extra or {})})


def start(args: list[str], cwd: Path, home: Path, env_extra: dict | None = None,
          rawlog: Path | None = None) -> Session:
    return Session(args, cwd, codex_env(home, env_extra), binary=CODEX, rawlog=rawlog)


def stop_daemons(home: Path):
    """Kill app-server daemons that a daemon-mode start left behind."""
    kill_matching(str(home))


# Screen-state detector for the codex TUI (0.160.0).
# Idle bottom of the screen:
#   › <input or placeholder>
#   <blank>
#   GPT-6.1-Sol default · /path            (status line)
#   ? for shortcuts                        (or "← for agents · ? for shortcuts")
# Busy: a "• Working (3s • esc to interrupt)" line above the composer.
# Dialogs: a "› 1. ..." numbered list and an "enter ... · esc ..." hint line.
WORKING_RX = re.compile(r"esc to interrupt|Working \(\d+s")
DIALOG_RX = re.compile(r"(^\s*enter (continue|confirm|to select|to submit)\b)|(· esc (quit|skip|cancel|use session))|(esc/ctrl\+c/q exit)", re.I)
NUM_OPT_RX = re.compile(r"^\s*›\s*\d+\.\s")


def screen_state(lines: list[str], dim: list[str] | None = None) -> dict:
    """Classify a codex screen. dim = Session.dim_mask() (optional).

    input_text is the composer text without the dim placeholder when dim is
    given. ready = composer found, not busy, no dialog, empty input. The
    caller must also see the same state for ~1 s: codex draws the composer
    for ~0.35 s before a start-up dialog replaces it.
    """
    st = {"input_box": False, "input_text": None, "busy": False, "ready": False,
          "dialog": None, "footer": None}
    nz = [l for l in lines if l.strip()]
    for l in nz[-6:]:
        if DIALOG_RX.search(l):
            st["dialog"] = l.strip()
    idx = None
    for i in range(len(lines) - 1, -1, -1):
        l = lines[i]
        if l.lstrip().startswith("›") and not NUM_OPT_RX.match(l):
            idx = i
            break
    if idx is not None and st["dialog"] is None:
        st["input_box"] = True
        src = dim if dim is not None else lines
        row = src[idx] if "›" in src[idx] else lines[idx]
        st["input_text"] = row.split("›", 1)[1].strip()
        st["footer"] = "\n".join(l for l in lines[idx + 1:] if l.strip())
        above = "\n".join(lines[max(0, idx - 4):idx])
        st["busy"] = bool(WORKING_RX.search(above)) or bool(WORKING_RX.search(st["footer"]))
    st["ready"] = st["input_box"] and not st["busy"] and st["dialog"] is None and st["input_text"] == ""
    return st


def state(sess) -> dict:
    return screen_state(sess.lines(), sess.dim_mask())


def wait_ready(sess, timeout: float = 40, stable: float = 1.0):
    """Wait until state()["ready"] holds for `stable` s. Return elapsed s."""
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

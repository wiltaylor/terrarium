"""Shared helpers for the pi spikes (pi 0.85.1).

Reuses spikes/claude/lib/ptyharness.py. Nothing here writes ~/.pi/agent.
The temp PI_CODING_AGENT_DIR gets a copy of models.json (the user's custom
LAN providers) and, for the served model id, one added model entry in the
copy. auth.json in ~/.pi/agent is empty ("{}"): pi has no OAuth login here.
"""
from __future__ import annotations

import json
import os
import re
import socket
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "claude" / "lib"))
from ptyharness import *  # noqa: F401,F403

PI = os.environ.get("PI_BIN", str(Path.home() / ".volta/bin/pi"))
REAL_DIR = Path.home() / ".pi" / "agent"
EXT = Path(__file__).resolve().parents[1] / "terrarium-ext" / "terrarium.ts"
# Model: openai-codex (ChatGPT Pro) with a copy of Codex's access token,
# gpt-5.6-luna ("older fast and efficient"), thinking off.
# Why not the others: the user's LAN provider (glm53-exl3) went down during
# the first run; Anthropic with Claude Code's OAuth token is rejected with
# "You're out of extra usage" (third-party use is billed as extra usage).
LAN_PROVIDER, LAN_MODEL = "glm53-exl3", "DeepSeek-V4.1-Flash-1017"
PROVIDER = os.environ.get("PI_PROVIDER", "openai-codex")
MODEL = os.environ.get("PI_MODEL", "gpt-5.6-luna")
CODEX_AUTH = Path.home() / ".codex" / "auth.json"


def codex_credential() -> dict:
    """pi's openai-codex OAuth entry built from a copy of ~/.codex/auth.json.

    refresh is left EMPTY on purpose: pi can then never refresh, so it can
    never rotate (and so invalidate) the host's Codex refresh token. When
    the access token expires, pi fails instead.
    """
    import base64
    t = json.loads(CODEX_AUTH.read_text())["tokens"]
    seg = t["access_token"].split(".")[1]; seg += "=" * (-len(seg) % 4)
    claims = json.loads(base64.urlsafe_b64decode(seg))
    return {"type": "oauth", "access": t["access_token"], "refresh": "",
            "expires": claims["exp"] * 1000,
            "accountId": claims["https://api.openai.com/auth"]["chatgpt_account_id"]}
CLAUDE_CREDS = Path.home() / ".claude" / ".credentials.json"


def claude_oauth_env() -> dict:
    """ANTHROPIC_OAUTH_TOKEN from Claude Code's credentials. Read-only; the
    token goes only into the child env, never into a file or a capture."""
    try:
        tok = json.loads(CLAUDE_CREDS.read_text())["claudeAiOauth"]["accessToken"]
    except (OSError, KeyError, ValueError):
        return {}
    return {"ANTHROPIC_OAUTH_TOKEN": tok}
QUIET_ENV = {"PI_TELEMETRY": "0", "PI_OFFLINE": "1"}


def make_pi_dir(settings: dict | None = None, add_model: bool = True, files: dict | None = None) -> Path:
    d = make_home("pi", {}, {"auth.json": "{}", **(files or {})})
    if PROVIDER == "openai-codex" and "auth.json" not in (files or {}):
        (d / "auth.json").write_text(json.dumps({"openai-codex": codex_credential()}))
    os.chmod(d / "auth.json", 0o600)
    m = json.loads((REAL_DIR / "models.json").read_text())
    if add_model and LAN_PROVIDER in m.get("providers", {}):
        models = m["providers"][LAN_PROVIDER].setdefault("models", [])
        if not any(x.get("id") == LAN_MODEL for x in models):
            models.append({"id": LAN_MODEL, "name": LAN_MODEL})
    (d / "models.json").write_text(json.dumps(m, indent=2))
    os.chmod(d / "models.json", 0o600)
    (d / "settings.json").write_text(json.dumps(settings or {"defaultThinkingLevel": "off"}, indent=2))
    return d


def pi_env(agent_dir: Path, sock: Path | None = None, extra: dict | None = None) -> dict:
    e = {"PI_CODING_AGENT_DIR": str(agent_dir), **QUIET_ENV}
    if PROVIDER == "anthropic":
        e.update(claude_oauth_env())
    if sock:
        e["TERRARIUM_SOCKET"] = str(sock)
    return harness_env({**e, **(extra or {})})


def base_args(ext: bool = True, provider: str = PROVIDER, model: str = MODEL) -> list[str]:
    a = ["--provider", provider, "--model", model, "--thinking", "off", "--offline"]
    if ext:
        a += ["-e", str(EXT)]
    return a


def start(args, cwd, agent_dir, sock=None, env_extra=None, rawlog=None) -> Session:
    return Session(args, cwd, pi_env(agent_dir, sock, env_extra), binary=PI, rawlog=rawlog)


class EventSocket:
    """Unix socket server that collects the extension's JSON lines."""

    def __init__(self, path: Path):
        self.path = Path(path)
        self.events: list[dict] = []
        self.lock = threading.Lock()
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(str(self.path)); self.srv.listen(8)
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while True:
            try:
                c, _ = self.srv.accept()
            except OSError:
                return
            threading.Thread(target=self._read, args=(c,), daemon=True).start()

    def _read(self, c):
        buf = b""
        while True:
            try:
                d = c.recv(65536)
            except OSError:
                return
            if not d:
                return
            buf += d
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                try:
                    ev = json.loads(line)
                except ValueError:
                    ev = {"_raw": line.decode(errors="replace")}
                ev["_rx"] = time.time()
                with self.lock:
                    self.events.append(ev)

    def names(self) -> list[str]:
        with self.lock:
            return [e.get("hook_event_name") for e in self.events]

    def of(self, name: str) -> list[dict]:
        with self.lock:
            return [e for e in self.events if e.get("hook_event_name") == name]

    def close(self):
        self.srv.close()


# Screen-state detector for the pi TUI (0.85.1).
# Idle bottom of the screen:
#   ──────────────── (rule)
#   <editor text, empty when idle; no prompt mark>
#   ──────────────── (rule)
#   /path/to/cwd
#   ↑1.2k ↓10 ... $0.000 (sub) 0.4%/272k (auto)        (provider) model • thinking off
# Busy: the top rule carries the spinner: "── ⠸ Working ──────".
# Dialogs: start-up text prompts end with "[y/N]"; selector lists show "↑↓".
PI_RULE_RX = re.compile(r"^\s*──")
PI_BUSY_RX = re.compile(r"Working")
PI_DIALOG_RX = re.compile(r"\[y/N\]|\[Y/n\]|↑↓|↑/↓|enter to (select|confirm)", re.I)


def screen_state(lines: list[str], dim: list[str] | None = None) -> dict:
    st = {"input_box": False, "input_text": None, "busy": False, "ready": False,
          "dialog": None, "footer": None}
    nz = [l for l in lines if l.strip()]
    for l in nz[-6:]:
        if PI_DIALOG_RX.search(l):
            st["dialog"] = l.strip()[:80]
    rules = [i for i, l in enumerate(lines) if PI_RULE_RX.match(l)]
    if len(rules) >= 2:
        a, b = rules[-2], rules[-1]
        src = dim if dim is not None else lines
        st["input_box"] = True
        st["input_text"] = "\n".join(x.strip() for x in src[a + 1:b]).strip()
        st["footer"] = "\n".join(l for l in lines[b + 1:] if l.strip())
        st["busy"] = bool(PI_BUSY_RX.search(lines[a]))
    st["ready"] = st["input_box"] and not st["busy"] and st["dialog"] is None and st["input_text"] == ""
    return st


def state(sess) -> dict:
    return screen_state(sess.lines(), sess.dim_mask())

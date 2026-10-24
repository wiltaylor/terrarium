"""Shared helpers for the Claude Code PTY spikes.

Every spike imports this module. It gives:
- a throwaway CLAUDE_CONFIG_DIR with only the OAuth credentials copied in,
- a clean environment (no parent Claude Code session variables),
- a PTY session with a pyte screen fed from a reader thread,
- a hook logger command line.

Nothing here touches ~/.claude.json or ~/.claude/settings.json.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
import threading
import time
import atexit
import uuid
from pathlib import Path

import pexpect
import pyte

HERE = Path(__file__).resolve().parent
HOOKLOG = HERE / "hooklog.py"
REAL_CREDS = Path.home() / ".claude" / ".credentials.json"
CLAUDE = os.environ.get("CLAUDE_BIN", str(Path.home() / ".local/bin/claude"))

COLS, ROWS = 120, 40

# Hook events in 2.1.310 (from the event list inside the binary).
# WorktreeCreate/WorktreeRemove are left out: a WorktreeCreate hook replaces
# git worktree creation, so a logger there would change behaviour.
ALL_HOOK_EVENTS = [
    "PreToolUse", "PostToolUse", "PostToolUseFailure", "PostToolBatch",
    "Notification", "UserPromptSubmit", "UserPromptExpansion", "SessionStart",
    "SessionEnd", "Stop", "StopFailure", "SubagentStart", "SubagentStop",
    "PreCompact", "PostCompact", "PreModelSwitch", "PostModelSwitch",
    "PermissionRequest", "PermissionDenied", "Setup", "TeammateIdle",
    "TaskCreated", "TaskCompleted", "Elicitation", "ElicitationResult",
    "ConfigChange", "InstructionsLoaded", "CwdChanged", "FileChanged",
    "DirectoryAdded", "MessageDisplay",
]


def log(*a):
    print(f"[{time.strftime('%H:%M:%S')}]", *a, flush=True)


def make_config_dir(cwd: Path | None = None, trust: bool = True,
                    onboarding: bool = True, extra: dict | None = None) -> Path:
    """Create a temp CLAUDE_CONFIG_DIR with only the credentials file.

    trust/onboarding pre-seed .claude.json so no first-run dialog appears.
    """
    d = Path(tempfile.mkdtemp(prefix="terrarium-cc-"))
    _CREATED.append(d)
    if REAL_CREDS.exists():
        shutil.copy2(REAL_CREDS, d / ".credentials.json")
        os.chmod(d / ".credentials.json", 0o600)
    cfg: dict = {}
    if onboarding:
        cfg.update({
            "hasCompletedOnboarding": True,
            "theme": "dark",
            "numStartups": 1,
        })
    if trust and cwd is not None:
        cfg["projects"] = {str(cwd): {
            "hasTrustDialogAccepted": True,
            "hasCompletedProjectOnboarding": True,
            "allowedTools": [],
        }}
    if extra:
        cfg.update(extra)
    if cfg:
        (d / ".claude.json").write_text(json.dumps(cfg, indent=2))
    return d


def remove_config_dir(d: Path):
    shutil.rmtree(d, ignore_errors=True)


def clean_env(config_dir: Path, extra: dict | None = None) -> dict:
    """Env without the parent Claude Code session variables."""
    env = {k: v for k, v in os.environ.items()
           if not (k.startswith("CLAUDE") or k in ("AI_AGENT",)
                   or k.startswith("ANTHROPIC"))}
    env.update({
        "CLAUDE_CONFIG_DIR": str(config_dir),
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "LANG": env.get("LANG", "C.UTF-8"),
        "DISABLE_AUTOUPDATER": "1",
        "DISABLE_TELEMETRY": "1",
    })
    if extra:
        env.update(extra)
    return env


def hook_settings(logfile: Path, events=ALL_HOOK_EVENTS) -> dict:
    hooks = {}
    for ev in events:
        cmd = f"{sys.executable} {HOOKLOG} {ev} {logfile}"
        hooks[ev] = [{"matcher": "", "hooks": [{"type": "command", "command": cmd}]}]
    return {"hooks": hooks}


def read_hooks(logfile: Path) -> list[dict]:
    if not logfile.exists():
        return []
    return [json.loads(l) for l in logfile.read_text().splitlines() if l.strip()]


class _Screen(pyte.Screen):
    """pyte.Screen that survives private CSI forms and keeps a dim flag.

    - pyte 0.8 passes private=True to every CSI handler for "CSI ? ..." and
      "CSI > ..." sequences. Most handlers do not take it and raise
      TypeError, which drops the rest of the chunk. Copilot CLI sends
      "CSI ? 6 n" and "CSI > 4;2 m". The wrappers below ignore a private
      form that the handler does not support.
    - pyte drops SGR 2 (dim). Codex and Copilot draw the composer placeholder
      dim. The dim state is kept in the (otherwise unused) "blink" field of
      each Char, so dim_text() can tell placeholder text from typed text.
    """

    _dim = False

    def select_graphic_rendition(self, *attrs, **kwargs):
        if kwargs.get("private"):
            return  # e.g. XTMODKEYS "CSI > 4;2 m", not a colour change
        for a in attrs or (0,):
            if a in (0, 22):
                self._dim = False
            elif a == 2:
                self._dim = True
        keep = [a for a in attrs if a not in (2, 5, 6, 25)]
        if attrs and not keep:
            pass
        else:
            super().select_graphic_rendition(*keep)
        self.cursor.attrs = self.cursor.attrs._replace(blink=self._dim)


_CSI_GT_RX = re.compile(rb"\x1b\[>[0-9;]*[A-Za-z]")


def _drop_private(fn):
    import inspect
    try:
        takes = "private" in inspect.signature(fn).parameters
    except (TypeError, ValueError):
        takes = False
    if takes:
        return fn

    def wrapper(self, *a, **kw):
        if kw.pop("private", False):
            return None
        return fn(self, *a, **kw)
    wrapper.__name__ = fn.__name__
    return wrapper


for _name in set(pyte.Stream.csi.values()):
    if _name != "select_graphic_rendition" and hasattr(pyte.Screen, _name):
        setattr(_Screen, _name, _drop_private(getattr(pyte.Screen, _name)))


class Session:
    """claude in a PTY with a pyte screen kept current by a reader thread."""

    def __init__(self, args: list[str], cwd: Path, env: dict,
                 cols: int = COLS, rows: int = ROWS, rawlog: Path | None = None,
                 binary: str | None = None):
        self.screen = _Screen(cols, rows)
        self.feed_errors = 0
        self.stream = pyte.ByteStream(self.screen)
        self.lock = threading.Lock()
        self.raw = bytearray()
        self.rawlog = rawlog
        self.t0 = time.time()
        # binary: run another harness (codex, copilot, pi). Default: claude.
        self.binary = binary or CLAUDE
        self.child = pexpect.spawn(self.binary, args, cwd=str(cwd), env=env,
                                   dimensions=(rows, cols), encoding=None,
                                   timeout=None)
        self.alive = True
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        while True:
            try:
                data = self.child.read_nonblocking(65536, timeout=0.2)
            except pexpect.TIMEOUT:
                continue
            except (pexpect.EOF, OSError):
                self.alive = False
                return
            with self.lock:
                self.raw += data
                # pyte does not know some private modes; it ignores them.
                try:
                    # pyte skips the ">" of "CSI > Ps m" (XTMODKEYS) and
                    # applies it as SGR. Drop "CSI > ..." sequences first.
                    self.stream.feed(_CSI_GT_RX.sub(b"", data))
                except Exception:  # never let one odd sequence stop the reader
                    self.feed_errors += 1
            # Answer terminal queries the TUI sends, so it does not stall.
            if b"\x1b[6n" in data:  # cursor position report
                y, x = self.screen.cursor.y + 1, self.screen.cursor.x + 1
                self.child.write(f"\x1b[{y};{x}R".encode())
            if b"\x1b[?6n" in data:  # DECXCPR (Copilot CLI)
                y, x = self.screen.cursor.y + 1, self.screen.cursor.x + 1
                self.child.write(f"\x1b[?{y};{x}R".encode())
            if b"\x1b[c" in data or b"\x1b[0c" in data:  # primary DA
                self.child.write(b"\x1b[?62;22c")

    def text(self) -> str:
        with self.lock:
            return "\n".join(l.rstrip() for l in self.screen.display)

    def dim_mask(self) -> list[str]:
        """Per row: the row text with every dim character replaced by a
        space. Placeholder text (drawn dim) vanishes; typed text stays."""
        with self.lock:
            out = []
            for y in range(self.screen.lines):
                row = self.screen.buffer[y]
                out.append("".join(" " if row[x].blink else row[x].data
                                   for x in range(self.screen.columns)).rstrip())
            return out

    def lines(self) -> list[str]:
        with self.lock:
            return [l.rstrip() for l in self.screen.display]

    def wait_for(self, pred, timeout: float = 30, poll: float = 0.1):
        """Wait until pred(screen_text) is truthy. Return the elapsed s or None."""
        t = time.time()
        while time.time() - t < timeout:
            if pred(self.text()):
                return time.time() - t
            if not self.alive:
                return None
            time.sleep(poll)
        return None

    def wait_text(self, pattern: str, timeout: float = 30):
        rx = re.compile(pattern)
        return self.wait_for(lambda s: rx.search(s), timeout)

    def wait_stable(self, quiet: float = 1.5, timeout: float = 30):
        """Wait until the screen does not change for `quiet` seconds."""
        last, since, t = None, time.time(), time.time()
        while time.time() - t < timeout:
            cur = self.text()
            if cur != last:
                last, since = cur, time.time()
            elif time.time() - since >= quiet:
                return True
            time.sleep(0.1)
        return False

    def send(self, data: bytes | str):
        if isinstance(data, str):
            data = data.encode()
        self.child.write(data)

    def paste(self, text: str):
        self.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")

    def close(self, grace: float = 5):
        if self.rawlog:
            with self.lock:
                self.rawlog.write_bytes(bytes(self.raw))
        if self.child.isalive():
            self.child.terminate(force=False)
            t = time.time()
            while self.child.isalive() and time.time() - t < grace:
                time.sleep(0.1)
            if self.child.isalive():
                self.child.terminate(force=True)


def snap(sess: Session, path: Path, label: str):
    """Append a labelled screen snapshot to a text file."""
    with path.open("a") as f:
        f.write(f"===== {label} (t={time.time()-sess.t0:.1f}s) =====\n")
        f.write(sess.text().rstrip() + "\n\n")


# Screen-state detector (from the typing-into-tui spike, 2.1.310).
# The idle TUI ends with:
#   ──────────── (rule)
#   ❯ <input text>
#   ──────────── (rule)
#     ⏵⏵ bypass permissions on (shift+tab to cycle) [· esc to interrupt] ...
RULE_RX = re.compile(r"^\s*─{20,}\s*$")


def screen_state(lines: list[str]) -> dict:
    rules = [i for i, l in enumerate(lines) if RULE_RX.match(l)]
    st = {"input_box": False, "input_text": None, "busy": False,
          "ready": False, "footer": None, "dialog": None}
    # Dialogs (trust, bypass warning, AskUserQuestion, permission prompts)
    # replace the input box and end with a key-hint line.
    tail = "\n".join(l for l in lines if l.strip())
    tail = "\n".join(tail.splitlines()[-4:])
    for marker in ("Enter to select", "Enter to confirm", "Esc to cancel", "to navigate"):
        if marker in tail:
            st["dialog"] = marker
            break
    # Find the last pair of rules with a ❯ line between them.
    for j in range(len(rules) - 1, 0, -1):
        a, b = rules[j - 1], rules[j]
        inner = [l for l in lines[a + 1:b]]
        if inner and inner[0].lstrip().startswith("❯"):
            st["input_box"] = True
            st["input_text"] = "\n".join(inner)[inner[0].index("❯") + 1:].strip()
            footer = "\n".join(l for l in lines[b + 1:] if l.strip())
            st["footer"] = footer
            st["busy"] = "esc to interrupt" in footer
            break
    st["ready"] = st["input_box"] and not st["busy"] and st["input_text"] == "" and st["dialog"] is None
    return st


def new_uuid() -> str:
    return str(uuid.uuid4())


def _enc(p: str) -> str:
    """Claude's project-dir encoding of a path: every non-alnum char -> '-'."""
    return re.sub(r"[^A-Za-z0-9]", "-", p)


_TMP = tempfile.gettempdir()
SANITISE = [
    (re.compile(r"[^\s\"/]*" + re.escape(_enc(_TMP)) + r"-terrarium-cwd-[A-Za-z0-9_]+"), "<encoded-cwd>"),
    (re.compile(r"-tmp-terrarium-cwd-[A-Za-z0-9_]+"), "<encoded-cwd>"),
    (re.compile(r"[^\s\"]*/terrarium-cc-[A-Za-z0-9_]+"), "$CLAUDE_CONFIG_DIR"),
    (re.compile(r"[^\s\"]*/terrarium-cwd-[A-Za-z0-9_]+"), "$CWD"),
    (re.compile(re.escape(_TMP)), "$TMPDIR"),
    (re.compile(re.escape(str(Path.home()))), "$HOME"),
    (re.compile(r"[\w.+-]+@[\w-]+\.[\w.]+"), "<email>"),
    (re.compile(r'("(?:displayName|fullName|accountUuid|organizationUuid|orgId|orgName|userID|machineID|accessToken|refreshToken)"\s*:\s*)"[^"]*"'), r'\1"<redacted>"'),
]


def sanitise(s: str) -> str:
    for rx, rep in SANITISE:
        s = rx.sub(rep, s)
    return s


_CREATED: list[Path] = []


@atexit.register
def _cleanup():
    if os.environ.get("KEEP_TMP"):
        return
    for d in _CREATED:
        shutil.rmtree(d, ignore_errors=True)


def make_cwd() -> Path:
    d = Path(tempfile.mkdtemp(prefix="terrarium-cwd-"))
    _CREATED.append(d)
    return d


# ---------------------------------------------------------------------------
# Generic helpers for the other harnesses (codex, copilot, pi). Additive: the
# Claude spikes above do not use them.

def make_home(prefix: str, copies: dict | None = None,
              files: dict | None = None) -> Path:
    """Temp harness home. copies = {src_path: name}, files = {name: text}.

    Only the named files are copied. The directory is removed at exit
    (unless KEEP_TMP is set).
    """
    d = Path(tempfile.mkdtemp(prefix=f"terrarium-{prefix}-"))
    _CREATED.append(d)
    for src, name in (copies or {}).items():
        src = Path(src)
        if src.exists():
            (d / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, d / name)
            os.chmod(d / name, 0o600)
    for name, text in (files or {}).items():
        (d / name).parent.mkdir(parents=True, exist_ok=True)
        (d / name).write_text(text)
    return d


def harness_env(extra: dict | None = None) -> dict:
    """Env for a non-Claude harness: no parent Claude session variables."""
    env = {k: v for k, v in os.environ.items()
           if not (k.startswith("CLAUDE") or k in ("AI_AGENT",)
                   or k.startswith("ANTHROPIC") or k.startswith("CODEX")
                   or k.startswith("COPILOT") or k.startswith("PI_"))}
    env.update({"TERM": "xterm-256color", "COLORTERM": "truecolor",
                "LANG": env.get("LANG", "C.UTF-8")})
    if extra:
        env.update(extra)
    return env


def hook_cmd(event: str, logfile: Path) -> str:
    """Shell command that logs one hook payload (stdin JSON) to logfile."""
    return f"{sys.executable} {HOOKLOG} {event} {logfile}"


def wait_until(pred, timeout: float, poll: float = 0.05):
    """Wait until pred() is truthy. Return elapsed seconds or None."""
    t = time.time()
    while time.time() - t < timeout:
        if pred():
            return time.time() - t
        time.sleep(poll)
    return None


def wait_exit(sess: "Session", timeout: float = 10) -> float | None:
    return wait_until(lambda: not sess.alive or not sess.child.isalive(), timeout)


def kill_matching(fragment: str):
    """SIGTERM every process of this user whose command line has fragment."""
    import signal
    for p in Path("/proc").iterdir():
        if not p.name.isdigit() or int(p.name) == os.getpid():
            continue
        try:
            if p.stat().st_uid != os.getuid():
                continue
            cmd = (p / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        if fragment in cmd:
            try:
                os.kill(int(p.name), signal.SIGTERM)
            except OSError:
                pass


SANITISE_EXTRA = [
    (re.compile(r"(Signed in to \S+ as )[\w.-]+"), r"\1<login>"),
    (re.compile(r"(The current user is @)[\w-]+"), r"\1<login>"),
    (re.compile(r"[^\s\"']*/terrarium-(?:codex|copilot|pi|npm)-[A-Za-z0-9_]+"), "$HARNESS_HOME"),
    (re.compile(r'("(?:access_token|refresh_token|id_token|account_id|token|apiKey|api_key|login|user_id|email|creator_user_id|creator_account_id|chatgpt_account_id|chatgpt_user_id|accountId|userId)"\s*:\s*)"[^"]*"'), r'\1"<redacted>"'),
    (re.compile(r"\b(gh[opsu]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{20,}|eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_.-]+)"), "<token>"),
]


def sanitise_all(s: str) -> str:
    """sanitise() plus the rules for codex, copilot, and pi homes and tokens."""
    for rx, rep in SANITISE_EXTRA:
        s = rx.sub(rep, s)
    return sanitise(s)

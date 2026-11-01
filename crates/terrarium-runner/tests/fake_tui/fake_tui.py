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
  /hold       submit, print `holding...`, then wait (silent) for one line
              before Stop: a stall that ends when the test says so
  /question   submit, send a Notification hook, wait for one answer line
  /flood N    submit, print N numbered lines of 64 bytes, then Stop
  /abort      die of SIGABRT, with no hooks
  /compact    as Claude Code: no UserPromptSubmit. Send PreCompact
              (trigger "manual"), print `compacting...`, wait the work time,
              send PostCompact, and print `compacted`

A command may come after a turn tag (`[User] /exit 1`, PLAN §8.5): the fake
acts on it as a model that does what the turn says. The hooks get the whole
text.

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
  FAKE_TUI_SUBMIT_GATE a path: after a line is read, wait until this file
                       exists before UserPromptSubmit goes out
  FAKE_TUI_STARTUP_GATE  a path: as pi 1.4.0, show the prompt at once, but
                       finish the startup only when this file exists. Then
                       send the SessionStart hook (with --session). Input
                       before that is dropped with "Startup is still in
                       progress"
  FAKE_TUI_IGNORE_HUP  when "1", ignore SIGHUP (only SIGKILL stops it)
  FAKE_TUI_ANSWER_HOLD when "1", /question waits for one more line after
                       the answer before Stop, so the turn stays open
  FAKE_TUI_SLASH_NEEDS_PAUSE  when "1", read the terminal in cbreak mode
                       and submit a slash command only when its CR comes
                       at least 100 ms after the text, in a separate write
                       (as Codex does with `/exit`). A CR that comes with
                       the text stays unsubmitted: the text stays in the
                       line, as in the Codex composer. Other lines submit
                       on any CR.
  FAKE_TUI_INPUT_LOG   a path; with FAKE_TUI_SLASH_NEEDS_PAUSE, append one
                       line for each read of the terminal: the bytes as
                       hex. ICRNL is off then, so a CR stays a CR.
  FAKE_TUI_COMPACT     `silent`: /compact sends no PostCompact (a failed
                       compaction of Claude Code). `turn`: /compact also
                       sends UserPromptSubmit first and Stop last

Signals: SIGTERM and SIGHUP end it at once (the Python default), unless
FAKE_TUI_IGNORE_HUP is set. When its parent (the runner) dies, it exits
too, also with FAKE_TUI_IGNORE_HUP: a test never leaves an orphan TUI.

Each hook goes out (connect, send, close) before the next output, so the
order of hooks and screen text is fixed. The only timed waits are the
work time of a normal turn and /stall.
"""

import json
import os
import re
import signal
import socket
import sys
import threading
import time

SOCK = os.environ.get("TERRARIUM_SOCKET")
DELAY = float(os.environ.get("FAKE_TUI_DELAY_MS", "300")) / 1000
STALL = float(os.environ.get("FAKE_TUI_STALL_MS", "1500")) / 1000
NO_SUBMIT = os.environ.get("FAKE_TUI_NO_SUBMIT") == "1"
NO_SUBMIT_HOOK = os.environ.get("FAKE_TUI_NO_SUBMIT_HOOK") == "1"
SUBMIT_GATE = os.environ.get("FAKE_TUI_SUBMIT_GATE")
STARTUP_GATE = os.environ.get("FAKE_TUI_STARTUP_GATE")
SLASH_NEEDS_PAUSE = os.environ.get("FAKE_TUI_SLASH_NEEDS_PAUSE") == "1"
INPUT_LOG = os.environ.get("FAKE_TUI_INPUT_LOG")
COMPACT = os.environ.get("FAKE_TUI_COMPACT", "")
# The smallest gap between a slash command and its CR that submits it.
SLASH_PAUSE = 0.1


def read_line():
    """One line of input. With FAKE_TUI_SLASH_NEEDS_PAUSE, a slash command
    submits only on a CR that comes SLASH_PAUSE after the last other byte."""
    if not SLASH_NEEDS_PAUSE:
        return sys.stdin.readline()
    fd = sys.stdin.fileno()
    buf = b""
    last = time.monotonic()
    while True:
        chunk = os.read(fd, 1024)
        if INPUT_LOG:
            with open(INPUT_LOG, "a") as f:
                f.write(chunk.hex() + "\n")
        if not chunk:
            return buf.decode(errors="replace")
        now = time.monotonic()
        for b in chunk:
            c = bytes([b])
            if c in (b"\r", b"\n"):
                text = buf.replace(b"\x1b[200~", b"").replace(b"\x1b[201~", b"")
                if text.strip().startswith(b"/") and now - last < SLASH_PAUSE:
                    # A burst: keep the text, as Codex keeps it in the
                    # composer.
                    continue
                return (buf + b"\n").decode(errors="replace")
            buf += c
            last = now


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


def watch_parent():
    """Exit when the parent dies: then the process gets a new parent."""
    parent = os.getppid()

    def watch():
        while os.getppid() == parent:
            time.sleep(0.2)
        os._exit(129)

    threading.Thread(target=watch, daemon=True).start()


def wait_for_startup_gate():
    """A prompt that looks ready while the startup runs. A submit in that
    time does nothing, as in pi 1.4.0."""
    import select

    fd = sys.stdin.fileno()
    out("> ")
    while not os.path.exists(STARTUP_GATE):
        readable, _, _ = select.select([fd], [], [], 0.01)
        if readable:
            if not os.read(fd, 4096):
                return
            out("\nStartup is still in progress\n> ")
    out("\n")


def main():
    # Python sets no handler for these; make the default explicit, so a
    # signal ends the TUI at once.
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    signal.signal(signal.SIGHUP, signal.SIG_DFL)
    if os.environ.get("FAKE_TUI_IGNORE_HUP") == "1":
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
    watch_parent()
    if SLASH_NEEDS_PAUSE:
        import tty

        # No line discipline: the TUI sees each write when it comes. With
        # ICRNL on, a CR still reads as NL; read_line takes both.
        tty.setcbreak(sys.stdin.fileno())
        if INPUT_LOG:
            import termios

            attrs = termios.tcgetattr(sys.stdin.fileno())
            attrs[0] &= ~termios.ICRNL
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, attrs)
    out("fake tui 1.0\n")
    resume = arg("--resume")
    session = resume or arg("--session")
    if resume and os.environ.get("FAKE_TUI_RESUME_FAIL") == "1":
        out(f"No conversation found with session ID: {resume}\n")
        return 1
    if STARTUP_GATE:
        wait_for_startup_gate()
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
        line = read_line()
        if not line:
            return 0
        # A bracketed paste with newlines is one turn.
        while "\x1b[200~" in line and "\x1b[201~" not in line:
            more = read_line()
            if not more:
                break
            line += more
        text = line.replace("\x1b[200~", "").replace("\x1b[201~", "").strip()
        if not text:
            continue
        full = text
        text = re.sub(r"^\[(User|System:[^\]]*)\] ", "", text)
        if text == "/quit":
            out("bye\n")
            return 0
        if text.startswith("/exit "):
            out("bye\n")
            return int(text.split()[1])
        if text == "/abort":
            os.abort()
        if text == "/compact":
            if COMPACT == "turn":
                hook("UserPromptSubmit", {"prompt": full})
            hook("PreCompact", {"trigger": "manual"})
            out("compacting...\n")
            time.sleep(DELAY)
            if COMPACT != "silent":
                hook("PostCompact", {"trigger": "manual", "compact_summary": "s"})
            out("compacted\n")
            if COMPACT == "turn":
                hook("Stop", {})
            continue
        if NO_SUBMIT:
            out("ignored\n")
            continue
        if SUBMIT_GATE:
            while not os.path.exists(SUBMIT_GATE):
                time.sleep(0.01)
        if not NO_SUBMIT_HOOK:
            hook("UserPromptSubmit", {"prompt": full})
        out("working...\n")
        if text == "/stall":
            time.sleep(STALL)
        elif text == "/hold":
            out("holding...\n")
            read_line()
        elif text.startswith("/flood "):
            for i in range(int(text.split()[1])):
                out(f"flood {i:06d} " + "x" * 50 + "\n")
        elif text == "/question":
            hook("Notification", {"message": "Allow the tool?"})
            out("Allow the tool? [y/n] ")
            answer = read_line().strip()
            out(f"answer: {answer}\n")
            if os.environ.get("FAKE_TUI_ANSWER_HOLD") == "1":
                read_line()
        else:
            time.sleep(DELAY)
        out(f"done: {text}\n")
        hook("Stop", {})


if __name__ == "__main__":
    sys.exit(main())

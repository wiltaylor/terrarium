# Phase 0 spikes

The spikes are throwaway scripts. They check facts that the design in
`docs/PLAN.md` depends on. The results go into `docs/compat.md`.

Each spike is one Python script with inline `uv` metadata (PEP 723). `uv run`
installs `pexpect` and `pyte` into a cached environment. You do not need a
virtualenv.

## Before you start

- Install Claude Code at `~/.local/bin/claude`, or set `CLAUDE_BIN`.
- Log in to Claude Code once on the host. The spikes copy only
  `~/.claude/.credentials.json` into a temporary `CLAUDE_CONFIG_DIR`. They do
  not read or change `~/.claude.json` or `~/.claude/settings.json`.
- Do not run the spikes as root. Claude Code refuses
  `--dangerously-skip-permissions` as root.
- Optional: set `TMPDIR` to put the temporary config and work directories in
  a different place. Set `KEEP_TMP=1` to keep them after the run.

The scripts strip the parent session variables (`CLAUDE*`, `ANTHROPIC*`) from
the child environment, so you can run them from inside a Claude Code session.

## Shared code

`claude/lib/ptyharness.py` holds the shared code:

- `make_config_dir()` makes a temporary config directory with the credentials.
- `clean_env()` makes the child environment.
- `Session` runs `claude` in a PTY and keeps a `pyte` screen current.
- `screen_state()` is the "prompt ready" detector.
- `hook_settings()` makes a `--settings` hook block. Each hook runs
  `claude/lib/hooklog.py`, which appends the hook payload to a JSONL file.
- `sanitise()` removes paths, e-mail addresses, and account IDs from captures.

## Spikes

Run each command from the repository root. "Turns" is the number of real model
turns (all on `haiku`).

| Spike | Command | Turns | Time |
|---|---|---|---|
| 1. Hooks in interactive mode | `uv run spikes/claude/hooks-in-interactive/run.py [idle_s]` | 1 | ~90 s (70 s idle wait for `Notification`) |
| 1b. Settings sources and `--bare` | `uv run spikes/claude/hooks-in-interactive/sources.py` | 0 | ~30 s |
| 2. Session ID and resume | `uv run spikes/claude/session-id-and-resume/run.py` | 2 | ~30 s |
| 3. Yolo, trust, quiet env | `uv run spikes/claude/yolo-and-trust/run.py [variant ...]` | 0 | ~35 s |
| 3b. Yolo as root (Docker) | `sh spikes/claude/yolo-and-trust/root-check.sh` | 0 | ~10 s |
| 4. System prompt | `uv run spikes/claude/system-prompt/run.py` (and `... run.py e`) | 4 (+1) | ~30 s |
| 5. Typing into the TUI | `uv run spikes/claude/typing-into-tui/run.py` | 7 | ~3 min |
| 5b. Detector, offline | `uv run spikes/claude/typing-into-tui/detector_check.py` | 0 | 1 s |
| 6. bwrap in Docker | `uv run spikes/claude/bwrap-in-docker/run.py` | 0 | ~1 min |
| 7. Auth status | `uv run spikes/claude/auth-status/run.py` | 0 | ~5 s |

Each spike writes its results to `captured/` in its own directory. A new run
overwrites the old captures. Look at the diff before you commit new captures.

To clean a capture again after you change `sanitise()`, run:

```
uv run --with pexpect --with pyte python spikes/claude/lib/resanitise.py <files>
```

## Notes

- Spike 6 needs Docker and network access to pull `debian:bookworm-slim`. It
  builds the image `terrarium-spike-bwrap`. Remove the image with
  `docker rmi terrarium-spike-bwrap` when you are done.
- `bwrap-in-docker/docker-default-seccomp.json` is a copy of
  <https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json>.
  `make_profiles.py` makes the other `seccomp-*.json` files from it.
- The spikes do not refresh the OAuth token on purpose. If the access token in
  the copied credentials expires during a run, the child refreshes it in the
  temporary directory, and the refresh token on the host can become invalid.
  Run the spikes when the host token has more than one hour left.

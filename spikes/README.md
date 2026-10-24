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

Additions for the other harnesses (the Claude spikes do not use them):

- `Session(..., binary=...)` runs another harness binary.
- The pyte screen ignores private CSI forms that pyte 0.8 cannot handle
  (`CSI ? 6 n`, `CSI > 4;2 m`; Copilot sends both) and keeps a dim flag.
  `Session.dim_mask()` gives the rows with dim text blanked, so a dim
  placeholder (Codex, Copilot) does not count as typed input.
- `make_home()`, `harness_env()`, `hook_cmd()`, `wait_until()`,
  `wait_exit()`, `kill_matching()`, and `sanitise_all()` (also removes the
  temp homes, tokens, account IDs, and the GitHub login).

Each harness has its own helper module: `codex/lib/codexlib.py`,
`copilot/lib/copilotlib.py`, `pi/lib/pilib.py`. Each has `screen_state()`
(the prompt-ready detector for that TUI), the launch arguments, and the
temp-home set-up. `pi/terrarium-ext/terrarium.ts` is the pi extension that
posts events to a Unix socket.

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

For Codex, Copilot, and pi captures, add `--all` before the files.

## Codex spikes

Before you start: log in to Codex once on the host. The spikes copy only
`~/.codex/auth.json` into a temp `CODEX_HOME`. They never read or change
`~/.codex/config.toml` or `~/.codex/hooks.json`. Set `CODEX_BIN` for a
different binary. Model: `gpt-6-luna`, effort `low`.

| Spike | Command | Turns | Time |
|---|---|---|---|
| 1. Hooks in interactive mode | `uv run spikes/codex/hooks-in-interactive/run.py [idle_s]` | 1 | ~35 s |
| 2+6. Yolo, trust, quiet start, daemon | `uv run spikes/codex/yolo-and-trust/run.py [variant ...]` | 0 | ~2 min |
| 3. Session ID and resume | `uv run spikes/codex/session-id-and-resume/run.py` | 2 | ~1 min |
| 4. System prompt | `uv run spikes/codex/system-prompt/run.py` | 3 | ~40 s |
| 5. Typing into the TUI | `uv run spikes/codex/typing-into-tui/run.py` | 4 prompts (3 turns: T4 steers T3) | ~45 s |
| 5b. Detector, offline | `uv run spikes/codex/typing-into-tui/detector_check.py` | 0 | 1 s |
| 7. Auth status | `uv run spikes/codex/auth-status/run.py` | 0 | ~5 s |
| 8. `/status` quota | `uv run spikes/codex/usage-quota/run.py` | 0 | ~15 s |

Total in the recorded run: 10 prompts. The `daemon` variant of spike 2
starts the shared app-server daemon in the temp home on purpose. The script
kills it (`stop_daemons()`).

## Copilot CLI spikes

Before you start: log in to Copilot once on the host (`copilot login`). The
token is in the system keychain, so a temp `COPILOT_HOME` needs no copied
file. The spikes never change `~/.copilot`. They do write the shared cache
`~/.cache/copilot/`. Spike 4 needs `npm` and network access: it installs
`copilot-custom-system-prompt@0.1.2` into a temp npm prefix with
`--ignore-scripts`. Set `COPILOT_BIN` for a different binary. Model:
`claude-haiku-4.5` (0.33 premium request per call).

| Spike | Command | Turns | Time |
|---|---|---|---|
| 1. Hook routes and payloads | `uv run spikes/copilot/hooks-in-interactive/run.py [idle_s]` | 1 | ~40 s |
| 2+6. Yolo, trust, quiet start | `uv run spikes/copilot/yolo-and-trust/run.py [variant ...]` | 0 | ~80 s |
| 3. Session ID and resume | `uv run spikes/copilot/session-id-and-resume/run.py` | 2 | ~1.5 min |
| 4. System prompt extension | `uv run spikes/copilot/system-prompt/run.py` | 3 | ~1 min |
| 5. Typing into the TUI | `uv run spikes/copilot/typing-into-tui/run.py` | 5 prompts (T1, T2, T3, T4 steered, T5 ask_user) | ~2 min |
| 5b. Detector, offline | `uv run spikes/copilot/typing-into-tui/detector_check.py` | 0 | 1 s |
| 7. Auth status | `uv run spikes/copilot/auth-status/run.py` | 0 | ~30 s |
| 8. `/usage` quota | `uv run spikes/copilot/usage-quota/run.py` | 0 | ~15 s |

Total in the recorded run: 11 user prompts (one over the budget of 10),
about 3.6 premium requests. Spike 5 also caused one extra turn that
Copilot started itself (a background-shell notification).

## pi spikes

Before you start: pi needs a provider that works. This host has no pi login
(`~/.pi/agent/auth.json` is `{}`). The spikes default to `openai-codex`
with a copy of Codex's access token (`pilib.codex_credential()`; the
refresh token is left empty, so the host token can never be rotated). Log
in to Codex first, or set `PI_PROVIDER` and `PI_MODEL`. The spikes copy
`~/.pi/agent/models.json` into a temp `PI_CODING_AGENT_DIR` and never
change `~/.pi/agent`. Set `PI_BIN` for a different binary. Model:
`gpt-5.6-luna`, thinking off.

| Spike | Command | Turns | Time |
|---|---|---|---|
| 1. Extension events | `uv run spikes/pi/hooks-in-interactive/run.py [idle_s]` | 1 | ~20 s |
| 2+6. Tool approval, trust, quiet start | `uv run spikes/pi/yolo-and-trust/run.py [variant ...]` | 0 | ~25 s |
| 3. Session ID and resume | `uv run spikes/pi/session-id-and-resume/run.py` | 2 | ~45 s |
| 4. System prompt | `uv run spikes/pi/system-prompt/run.py` | 2 | ~15 s |
| 5. Typing into the TUI | `uv run spikes/pi/typing-into-tui/run.py` | 3 prompts (T1, T3, T4 steered) | ~25 s |
| 5b. Detector, offline | `uv run spikes/pi/typing-into-tui/detector_check.py` | 0 | 1 s |
| 7. Auth status and token reuse | `uv run spikes/pi/auth-status/run.py` | 0 | ~20 s |
| 8. `/session` usage | `uv run spikes/pi/usage-quota/run.py` | 0 | ~10 s |

Total in the recorded run: 10 prompt attempts. Two of them did not reach a
model: spike 1 run 1 on the LAN provider (connection refused, 0 tokens),
and spike 1 run 2 on `anthropic` with Claude Code's OAuth token (refused:
"out of extra usage"). The 8 others used `openai-codex`.

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

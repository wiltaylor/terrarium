# Harness compatibility facts

This file records facts that the Phase 0 spikes measured (PLAN §20). Each fact
names the spike that shows it. The spikes are in `spikes/`. See
`spikes/README.md` to run them again.

Status words:

- **verified**: a spike ran it and the capture shows it.
- **read**: found in `--help` or in the binary, but no spike ran it.
- **not tested**: an open question.

---

## Claude Code 2.1.310

Tested on 2026-10-24. Host: Linux 7.2 (CachyOS), user `uid=1000`, Claude Max
subscription (OAuth). Docker 29.8.0. Model for all turns: `haiku`.

### Summary

| Topic | Fact | Status | Spike |
|---|---|---|---|
| Recommended launch | `claude --dangerously-skip-permissions --setting-sources "" --settings '<json>' --session-id <uuid> [--model <m>] [--append-system-prompt <text>]` with `CLAUDE_CONFIG_DIR` set and the config pre-seeded (see "Quiet start") | verified | 1, 3 |
| Hook injection | `--settings '<json>'` with a `hooks` block works in interactive mode. It does not edit any settings file. | verified | 1 |
| Keep other hooks out | `--setting-sources ""` loads no user, project, or local settings. Only the `--settings` hooks fire. | verified | 1b |
| `--bare` | Not usable. It skips **all** hooks, `--settings` hooks too. It does not read OAuth, so a subscription login does not work ("API Usage Billing"). | verified | 1b |
| Hooks that fire in one turn | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolBatch`, `Stop`, `MessageDisplay`; then `Notification` (60 s idle) and `SessionEnd` | verified | 1 |
| Hook payload IDs | Every payload has `session_id`, `transcript_path`, `cwd`, `hook_event_name`. Turn hooks also have `prompt_id` and `permission_mode`. | verified | 1 |
| `UserPromptSubmit` latency | 23 to 31 ms after the Enter byte (hook is a Python script) | verified | 5 |
| Set session ID | `--session-id <uuid>` works. The hook `session_id` is the same UUID. | verified | 2 |
| Reuse a session ID | `--session-id <used uuid>` exits at once: `Error: Session ID <uuid> is already in use.` Use `--resume`. | verified | 2 |
| Resume | `--resume <uuid>` continues the conversation. `SessionStart.source` is `resume`. The session ID stays the same. | verified | 2 |
| Resume from another cwd | `--resume <uuid>` finds the session from a different cwd and shows the history. | verified (no turn sent) | 2 |
| Clean exit | Type `/exit` then CR. The process exits in 0.5 s. `SessionEnd.reason` = `prompt_input_exit`. Ctrl-C twice also works (the first shows "Press Ctrl-C again to exit"). SIGTERM gives `reason` = `other`. | verified | 1, 2 |
| Session file | `$CLAUDE_CONFIG_DIR/projects/<cwd with every non-alphanumeric char replaced by "-">/<session-id>.jsonl`. The hook `transcript_path` gives the exact path. | verified | 2 |
| Usage in the session file | `assistant` records carry `message.usage`; `cost-state` records carry running totals per model. See "Usage accounting". | verified | 2 |
| Yolo, non-root | `--dangerously-skip-permissions` shows a "Bypass Permissions mode" accept dialog ("No, exit" is the default) on every start, unless `skipDangerousModePermissionPrompt: true` is in the settings. `--settings` is enough. | verified | 3 |
| Yolo, root | Refused: `--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons`. `IS_SANDBOX=1` removes the refusal. | verified | 3b |
| Onboarding | A new config dir shows a theme picker first. `"hasCompletedOnboarding": true` in `.claude.json` removes it. | verified | 3 |
| Workspace trust | An untrusted cwd shows "Accessing workspace ... Yes, I trust this folder". `projects["<path>"].hasTrustDialogAccepted = true` removes it. Trust on a parent directory covers the subdirectories. | verified | 3 |
| Minimal pre-seed | `{"hasCompletedOnboarding": true, "projects": {"<dir>": {"hasTrustDialogAccepted": true}}}`. Nothing else is needed. | verified | 3 |
| Quiet env | `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` stops the changelog fetch and the promotion banner. Also set `DISABLE_AUTOUPDATER=1`, `DISABLE_TELEMETRY=1`, `DISABLE_ERROR_REPORTING=1`. | partly verified | 3 |
| `--append-system-prompt` | Works in interactive mode. The text is added to the default system prompt. | verified | 4 |
| `--system-prompt` | Works in interactive mode. It replaces the default prompt, but Claude Code keeps the first block "You are Claude Code, Anthropic's official CLI for Claude." The tools stay. | verified | 4 |
| System prompt on resume | **The first prompt is frozen.** `--resume` with a new `--append-system-prompt` still sends the old prompt. Add `--system-prompt-snapshot off` to send the new one. | verified | 4 |
| User `CLAUDE.md` | `$CLAUDE_CONFIG_DIR/CLAUDE.md` loads by default. `--setting-sources ""` stops it. | verified | 4 |
| Prompt ready | About 1.9 to 2.7 s after spawn. Use the screen detector below. | verified | 1, 5 |
| Submit a turn | Bracketed paste, then CR. CR can follow the paste end at once. Newlines in the paste stay in the prompt. | verified | 5 |
| Large paste | 80 lines (5807 chars) show as `[Pasted text #1 +80 lines]` in the box. The hook `prompt` has the full text. | verified | 5 |
| Bare LF | LF (`\n`) outside a paste adds a new line. It does not submit. | verified | 5 |
| Type while busy | The text is queued ("Press up to edit queued messages"). `UserPromptSubmit` fires at once (0.27 s). The queued text joins the running turn. Only **one** `Stop` fires for both prompts. | verified | 5 |
| Type before ready | Text typed 0.3 s after spawn was kept and submitted. | verified (one run) | 5 |
| AskUserQuestion | `PreToolUse` and then `PermissionRequest` (tool `AskUserQuestion`) fire, also in bypass mode. The screen shows a numbered list with "Enter to select · ↑/↓ to navigate · Esc to cancel". Down arrow (`ESC [ B`) then CR picked option 2. | verified | 5 |
| Auth status | `claude auth status` prints JSON by default (`--json` is the same); `--text` prints lines. Exit code 0 when logged in, 1 when not. | verified | 7 |
| bwrap in Docker | Works as non-root with a custom seccomp profile (Docker default plus `clone`, `clone3`, `unshare`, `mount`, `umount2`, `pivot_root`) and `--security-opt systempaths=unconfined`. No `SYS_ADMIN`. | verified | 6 |
| Per-agent file overlay | A read-only file bind over a file inside a bind-mounted dir works. Each sandbox sees its own file. The shared file does not change. | verified | 6 |

### Hooks

Events in 2.1.310 (from the list in the binary): `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `PostToolBatch`, `Notification`, `UserPromptSubmit`,
`UserPromptExpansion`, `SessionStart`, `SessionEnd`, `Stop`, `StopFailure`,
`SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`,
`PreModelSwitch`, `PostModelSwitch`, `PermissionRequest`, `PermissionDenied`,
`Setup`, `TeammateIdle`, `TaskCreated`, `TaskCompleted`, `Elicitation`,
`ElicitationResult`, `ConfigChange`, `WorktreeCreate`, `WorktreeRemove`,
`InstructionsLoaded`, `CwdChanged`, `FileChanged`, `DirectoryAdded`,
`MessageDisplay`.

Do not register `WorktreeCreate` as a logger. A `WorktreeCreate` hook replaces
the git worktree step.

Sequence for one turn with one Bash call (spike 1, times from the Enter byte):

| t (s) | Hook | Useful fields |
|---|---|---|
| -2.44 | `SessionStart` | `source` (`startup` or `resume`), `model` |
| 0.30 | `UserPromptSubmit` | `prompt` (full text), `prompt_id`, `permission_mode` |
| 3.41 | `PreToolUse` | `tool_name`, `tool_input`, `tool_use_id`, `effort.level` |
| 3.70 | `PostToolUse` | `tool_response` (`stdout`, `stderr`, ...) |
| 3.72 | `PostToolBatch` | `tool_calls[]` |
| 5.05 | `Stop` | `last_assistant_message`, `stop_hook_active`, `background_tasks`, `session_crons` |
| 5.05 | `MessageDisplay` | `turn_id`, `message_id`, `delta`, `final` |
| 65.07 | `Notification` | `notification_type` = `idle_prompt`, `message` = "Claude is waiting for your input" |
| 76.60 | `SessionEnd` | `reason` |

Common fields in every payload: `session_id`, `transcript_path`, `cwd`,
`hook_event_name`, and `scratchpad_dir` (under `$TMPDIR/claude-<uid>/`).
The payloads are in
`spikes/claude/hooks-in-interactive/captured/hooks.jsonl`. The key types are
in `payload-shapes.json` in the same directory.

Notes:

- The hook event name is in the payload (`hook_event_name`). The
  `terrarium hook <event>` argument is not necessary, but it does no harm.
- `Notification` with `idle_prompt` comes 60 s after `Stop`. It is not a
  "blocked" signal. Use `Stop` for "idle".
- For "needs input", use `PermissionRequest`. It fires for `AskUserQuestion`
  in bypass mode.
- Settings sources (spike 1b, `SessionStart` from three places):

  | Flags | Hooks that fired |
  |---|---|
  | none | flag, user, project |
  | `--setting-sources user` | flag, user |
  | `--setting-sources project` | flag, project |
  | `--setting-sources ""` | flag only |
  | `--bare` | none |

  "User" is `$CLAUDE_CONFIG_DIR/settings.json`. "Project" is
  `<cwd>/.claude/settings.json`. Managed (policy) settings were not tested.

### Quiet start

To get to the idle prompt with no dialog:

1. Set `CLAUDE_CONFIG_DIR` to the harness home.
2. Put `"hasCompletedOnboarding": true` in `$CLAUDE_CONFIG_DIR/.claude.json`.
3. Put `projects["<workspace or a parent>"].hasTrustDialogAccepted = true` in
   the same file.
4. Pass `"skipDangerousModePermissionPrompt": true` in `--settings`.

First screen for each pre-seed (spike 3):

| Onboarding | Trust | Skip prompt | First screen |
|---|---|---|---|
| no | no | no | theme picker |
| yes | no | no | workspace trust dialog |
| yes | yes | no | bypass permissions warning |
| yes | yes | yes | idle prompt |
| no | yes | yes | theme picker |
| yes | no | yes | workspace trust dialog |
| yes | parent dir | yes | idle prompt |

The trust dialog and the bypass warning have "No, exit" as the default
choice. An Enter typed into one of them stops the agent.

Quiet environment variables:

| Variable | Effect | Status |
|---|---|---|
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | No `changelogLastFetched` written; no promotion banner | verified |
| `DISABLE_TELEMETRY=1` | `auth status` shows `analyticsDisabled: true` | verified (indirect) |
| `DISABLE_AUTOUPDATER=1`, `DISABLE_UPDATES=1` | Names exist in the binary | read |
| `DISABLE_ERROR_REPORTING=1` | Name exists in the binary | read |
| `DISABLE_LOGIN_COMMAND=1`, `DISABLE_LOGOUT_COMMAND=1` | Names exist. Useful so an agent cannot log out the shared account. | read |
| `IS_SANDBOX=1` | Lets yolo run as root | verified |

Claude Code writes `oauthAccount` (e-mail, name, organisation) into
`.claude.json` on the first interactive start.

### Session file and usage accounting

Path: `$CLAUDE_CONFIG_DIR/projects/<encoded cwd>/<session-id>.jsonl`. The
encoding replaces each character that is not a letter or a digit with `-`.

Record types in one two-turn session: `user`, `assistant`, `attachment`,
`system` (`stop_hook_summary`, `turn_duration`), `cost-state`,
`api-request`, `api-request-shape`, `api-request-blob`, `last-prompt`,
`ai-title`, `mode`, `permission-mode`, `atis-latch`, `file-history-snapshot`.

`assistant` record, `message.usage`:

```json
{"input_tokens": 2, "cache_creation_input_tokens": 36712, "cache_read_input_tokens": 0,
 "output_tokens": 45, "output_tokens_details": {"thinking_tokens": 41},
 "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
 "service_tier": "standard",
 "cache_creation": {"ephemeral_1h_input_tokens": 36712, "ephemeral_5m_input_tokens": 0},
 "inference_geo": "global", "iterations": [], "speed": "standard", "fallback_credit": null}
```

Also on the record: `message.model`, `message.id`, `requestId`, `timestamp`.

`cost-state` record (running totals for the session):

```json
{"type": "cost-state", "totalCostUSD": 0.00791902, "totalAPIDuration": 4060,
 "modelUsage": {"claude-haiku-5-5": {"inputTokens": 1188, "outputTokens": 73,
   "thinkingTokens": 41, "cacheReadInputTokens": 36712,
   "cacheCreationInputTokens": 36983, "webSearchRequests": 0, "costUSD": 0.00791902}}}
```

Rules for the tailer:

- One API response can make **several** `assistant` records (one per content
  block). They repeat the same `message.id`, `requestId`, and `usage`. Count
  each `message.id` once.
- `assistant` records cover the main thread only. Side calls (for example the
  `ai-title` call) are not there. `cost-state.modelUsage` includes them
  (`inputTokens` 1186 against 2 in the `assistant` record). Use the
  `cost-state` deltas for totals. Use `assistant` records for per-turn detail.
- `cost-state` repeats with the same values. Use the delta from the last seen
  record.
- `costUSD` is the API-price equivalent. It is not the subscription charge.
- `api-request-shape` holds the system prompt that was sent. It is a good
  place to check the prompt.

### Typing into the TUI

The idle screen ends like this (spike 1, `captured/screens.txt`):

```
────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
```

While it works, a spinner line ("· Sketching…", "✽ Effecting… (4s · ↓ 83
tokens)") is above the box. The footer adds "esc to interrupt".

Detector (`screen_state()` in `spikes/claude/lib/ptyharness.py`):

1. Find the last two rule lines (20 or more `─`) with a line that starts with
   `❯` between them. That is the input box. The text after `❯` is the input.
2. `busy` = the footer below the box has "esc to interrupt".
3. `dialog` = one of the last four non-empty lines has "Enter to select",
   "Enter to confirm", "Esc to cancel", or "to navigate".
4. `ready` = input box found, input empty, not busy, no dialog.

`typing-into-tui/detector_check.py` runs the detector on all 30 captured
screens. `ready` and `busy` were correct on all of them. The theme picker
gives `ready = false` but `dialog = None`, because it has no key-hint line.
Treat "no input box" as "not ready" too.

Send a turn:

1. Wait for `ready`.
2. Write `ESC [200~` + text + `ESC [201~`, then `\r`. No delay is necessary.
3. Expect `UserPromptSubmit` within 1 s. It came in 23 to 31 ms.
4. Expect `Stop` for the end of the turn.

Answer an `AskUserQuestion` dialog: the options are numbered. The cursor
starts on option 1. Send `ESC [B` (down) for each step, then `\r`. The
dialog also has "Type something." and "Chat about this" entries.

`pyte` shows small faults: a stray `u` on the first line (an unknown escape
sequence) and some mixed characters after partial redraws. The bottom lines
that the detector reads were correct in every capture.

### Auth status

`claude auth status` (JSON is the default; `--json` gives the same):

```json
{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
 "analyticsDisabled": true, "projectsDirectory": "<config>/projects",
 "configDirectory": "<config>", "email": "<email>", "orgId": "<uuid>",
 "orgName": "<name>", "subscriptionType": "max"}
```

- Logged out: `{"loggedIn": false, "authMethod": "none", ...}`, exit code 1.
- `--text`: `Login method: Claude Max account`, `Organization: ...`,
  `Email: ...`.
- A config dir with only `.credentials.json` gives `email`, `orgId`, and
  `orgName` as `null`. They come from `oauthAccount` in `.claude.json`,
  which the first interactive start writes.
- The plan tier is not in the output. `.credentials.json` has it:
  `claudeAiOauth.rateLimitTier` (for example `default_claude_max_20x`) and
  `claudeAiOauth.subscriptionType`.

### bwrap inside Docker

Image: `debian:bookworm-slim` with `bubblewrap` 0.8.0, user `uid=1000`.
Command: `bwrap --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64
/lib64 --ro-bind /bin /bin --proc /proc --dev /dev --unshare-all --share-net
true`. The host has no AppArmor.

| `docker run` options | Result |
|---|---|
| default | FAIL: "No permissions to create new namespace" |
| `--cap-add SYS_ADMIN` | FAIL: `pivot_root: Operation not permitted` |
| `seccomp=unconfined` | FAIL with `--proc`: "Can't mount proc"; OK with `--ro-bind /proc /proc` |
| `seccomp=unconfined` + `SYS_ADMIN` | same as above |
| `seccomp=unconfined` + `systempaths=unconfined` | OK |
| custom: + `clone`, `clone3`, `unshare` | FAIL: "Failed to make / slave" |
| custom: same + `SYS_ADMIN` | FAIL: `pivot_root` |
| custom: + `mount`, `umount2`, `pivot_root`, `setns` | FAIL with `--proc`; OK with `--ro-bind /proc /proc` |
| custom: same + `systempaths=unconfined` | OK |
| custom: `clone`, `clone3`, `unshare`, `mount`, `pivot_root` + `systempaths=unconfined` | FAIL: "unmount old root" |
| custom: that + `umount2` + `systempaths=unconfined` | **OK (minimal)** |
| `apparmor=unconfined` only | FAIL: no namespace |

Minimal working set: a seccomp profile that is Docker's default plus
unconditional `clone`, `clone3`, `unshare`, `mount`, `umount2`, `pivot_root`
(`bwrap-in-docker/seccomp-min-b.json`), and
`--security-opt systempaths=unconfined`. `SYS_ADMIN` does not help. It makes
things worse, because the default profile then allows the namespace calls but
the capability set still blocks `pivot_root`.

Without `systempaths=unconfined`, `--proc /proc` fails because Docker masks
paths under `/proc`. `--ro-bind /proc /proc` works instead, but the sandbox
then sees every process in the container.

File overlay (PLAN §8): `--bind /tmp/shared /home/agent --ro-bind
/tmp/promptA.md /home/agent/.copilot/SYSTEM.md` works. Two sandboxes saw their
own prompt. A write to the overlaid file failed with "Read-only file system".
The shared file did not change. If the target file does not exist, bwrap
**creates an empty file** in the shared directory as the mount point, and
leaves it there.

---

## Changes to the plan

These facts contradict or change assumptions in `docs/PLAN.md`.

1. **§11 seccomp.** "Allow `unshare`/`clone(CLONE_NEWUSER)`" is not enough.
   bwrap also needs `mount`, `umount2`, and `pivot_root`, and
   `--security-opt systempaths=unconfined` for `--proc`. Hosts with
   AppArmor (Ubuntu) can need `apparmor=unconfined` too. That is not tested.
2. **§8 and §11 shared home.** Without `CLAUDE_CONFIG_DIR`, Claude Code keeps
   its state file at `$HOME/.claude.json`, outside `$HOME/.claude`. A bind
   of `/auth/claude` at `$HOME/.claude` misses it. Set
   `CLAUDE_CONFIG_DIR=$HOME/.claude` (or `/auth/claude`) so that
   `.claude.json`, `.credentials.json`, and `projects/` are in one directory.
3. **§8 quiet defaults.** Trust is not the only dialog. The onboarding theme
   picker and the bypass-permissions warning also block the start. The
   pre-seed needs `hasCompletedOnboarding` and the settings key
   `skipDangerousModePermissionPrompt`. The trust and bypass dialogs default
   to "No, exit".
4. **§8 system prompt on restart.** Claude Code freezes the system prompt at
   the first request of a session. A restart with `--resume` and a new
   template keeps the old prompt. Pass `--system-prompt-snapshot off` when
   the rendered prompt changes, or always.
5. **§8 hooks and the user's own setup.** Pass `--setting-sources ""`.
   Otherwise the shared home's `settings.json`, its `CLAUDE.md`, and the
   repository's `.claude/settings.json` hooks all load into the agent.
6. **§7 sending a turn.** "Wait until idle" is not enough to make one turn
   per delivery. Text typed while busy joins the running turn: one `Stop`
   for two `UserPromptSubmit`. The runner must check `busy` on the screen
   just before it types, and must not count `Stop` events per delivery.
7. **§7 blocked detection.** `Notification` is only the 60 s idle reminder.
   The hook for "the agent asks a question" is `PermissionRequest`.
8. **§16.2 usage.** Do not sum `assistant` records as they are. Count each
   `message.id` once. Side calls are only in `cost-state`.
9. **§11 overlay on a missing file.** bwrap leaves an empty file in the
   shared home. Create the target file before the first start, or accept the
   placeholder.
10. **`--bare` is not an option** for subscription agents. It drops OAuth and
    all hooks.

## Surprises

- The bypass-permissions warning shows even when onboarding and trust are
  pre-seeded. It is a separate dialog with its own settings key.
- `--system-prompt` cannot remove the line "You are Claude Code, Anthropic's
  official CLI for Claude."
- `--resume <id>` works from any cwd, although the file is under the cwd's
  project directory.
- The hook payload has a `scratchpad_dir` under `$TMPDIR`. A private `/tmp`
  in the sandbox is enough for it.

## Not tested

- OAuth token refresh with many instances on one shared home. The spikes did
  not let the token expire, so they did not touch the host login. Two risks:
  refresh-token rotation if agents have separate copies of
  `.credentials.json`, and a rename-over write of a file that is a bind mount.
- Concurrent writes to one `.claude.json` from many instances.
- Project `CLAUDE.md` in the workspace with `--setting-sources ""`.
- Whether the hook process gets the launch environment (for the runner
  socket path). Claude Code runs hooks as child processes, so it is likely.
- Claude Code itself inside bwrap inside Docker (PTY, network, home binds).
- Managed (policy) settings.
- `DISABLE_AUTOUPDATER` and `DISABLE_UPDATES` by network capture.

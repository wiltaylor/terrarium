# Harness compatibility facts

This file records facts that the Phase 0 spikes measured (PLAN §20). Each fact
names the spike that shows it. The spikes are in `spikes/`. See
`spikes/README.md` to run them again.

Status words:

- **verified**: a spike ran it and the capture shows it.
- **read**: found in `--help` or in the binary, but no spike ran it.
- **not tested**: an open question.

The agent status `waiting` (PLAN §12d) is not a harness fact. The daemon
derives it from the runner's `idle` when the agent has an open wait. No
adapter and no compat scenario sees it, so a harness bump cannot break it.
The compat suite checks the runner statuses only.

## The tools dir and the version dirs

Each harness version is its own npm prefix in the tools dir:
`tools/versions/<name>/<version>/` (`crates/terrariumd/src/harness_tools.rs`,
[getting-started.md §7](getting-started.md#7-updating-a-harness)). The
links `tools/bin/<binary>` and `tools/current/<name>` point at the active
one. An agent launch runs the harness from the version dir that is active
at the launch, so an update does not change a running agent.

A move of a package dir to another prefix is safe for each pinned
package. Read from the installed packages on 2026-10-27 (no package has
an absolute path in a file or a link; each link is relative and stays in
the package dir; npm puts each dependency in the package's own
`node_modules/`):

| Package | Version | Command | What it loads | Status |
|---|---|---|---|---|
| `@anthropic-ai/claude-code` | 2.1.310 | `bin/claude` → `bin/claude.exe` | One native binary. | read |
| `@openai/codex` | 0.164.0 | `bin/codex` → `bin/codex.js` | A Node launcher that starts the native binary of `node_modules/@openai/codex-linux-x64`. | read |
| `@github/copilot` | 1.0.103 | `bin/copilot` → `npm-loader.js` | A Node loader that starts `node_modules/@github/copilot-linux-x64/copilot`. | read |
| `@earendil-works/pi-coding-agent` | 1.4.0 | `bin/pi` → `dist/bundle/cli.js` | A Node bundle, with its dependencies in `node_modules/` (relative `.bin` links). | read |
| `copilot-custom-system-prompt` | 0.1.2 | `dist/install.js` | Copies `extensions/copilot-custom-system-prompt` into `$COPILOT_HOME/extensions/`. The copy does not refer to the tools dir. | read |

So the migration of the flat layout (one prefix, `tools/lib/node_modules`)
moves each package with hard links, with no npm run.

## Version history

`harnesses.toml` at the repo root pins each harness (the version of the
first install). `just harness::latest --pin` and
`just harness::update <name> --pin` add a row here for each pin change. The "Range" cell
shows a change to the adapter's `tested` range. After
`just ci::compat <harness>`, write the result in the "Compat" cell. When a
bump changes a hook, a flag, or a config key, add a section for the new
version below, as for the versions below.

| Date | Harness | Version | Bump | Range | Compat |
|---|---|---|---|---|---|
| 2026-10-24 | claude | 2.1.310 | first pin | `>=2.1.310, <2.2.0` | pass (Phase 0 spikes, `ci::compat`) |
| 2026-10-24 | codex | 0.160.0 | first pin | `>=0.160.0, <0.161.0` | pass (Phase 0 spikes) |
| 2026-10-24 | copilot | 1.0.103 | first pin | `>=1.0.103, <1.1.0` | pass (Phase 0 spikes) |
| 2026-10-24 | pi | 0.85.1 | first pin | `>=0.85.1, <0.86.0` | pass (Phase 0 spikes) |
| 2026-10-24 | copilot-prompt | 0.1.2 | first pin | - | - |
| 2026-10-26 | codex | 0.160.0 → 0.164.0 | major | `>=0.160.0, <0.161.0` → `>=0.164.0, <0.165.0` | verified live in the container 2026-10-26 (codex: 1 turn; pi: 0 turns, start only) |
| 2026-10-26 | pi | 0.85.1 → 1.4.0 | major | `>=0.85.1, <0.86.0` → `>=1.4.0, <1.5.0` | verified live in the container 2026-10-26 (codex: 1 turn; pi: 0 turns, start only) |

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

### Claude inside bwrap

Tested on 2026-10-24, **outside Docker**: CachyOS host, kernel 7.2,
bubblewrap 0.12.0, Claude Code 2.1.310 (the native build in
`~/.local/share/claude/versions/`), Claude Max login, model `haiku`. The
daemon ran in-process with `RunnerSupervisor`, sandbox mode `bwrap`, and
the real Claude home bound as the shared home (`--harness-home
claude=$HOME/.claude`). Two model turns in total. The test is
`crates/terrariumd/tests/e2e_claude.rs` (`--ignored`).

| Topic | Fact | Status |
|---|---|---|
| Start in bwrap | The quiet start works in the sandbox. The idle prompt came about 0.3 s after the start call. No dialog. | verified |
| Hooks from the sandbox | `/opt/terrarium/bin/terrarium hook <Name>` (the CLI bound read-only) reached the runner socket that is bound at `/run/terrarium/agent.sock`. `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop` arrived. The hook process gets the launch env (`TERRARIUM_SOCKET`). | verified |
| Status | `starting` -> `idle` (prompt ready) -> `working` (prompt submitted) -> `idle` (turn ended) -> `stopped`. The turn took about 4 s. | verified |
| Session ID | `SessionStart` gave the UUID of `--session-id`. `transcript_path` is `/home/agent/.claude/projects/-home-agent-work/<id>.jsonl`: the sandbox path. On the host the file is in the shared home under `projects/-home-agent-work/`. | verified |
| Agent CLI | Without a prompt to do so, the agent ran `terrarium mail check` and `terrarium card ls --mine` (the default template asks for it). Both went through the runner socket `http` tunnel and answered. | verified |
| Clear the input box | `draft text` typed with no Enter, then Ctrl-U (`0x15`): the box was empty, the prompt read as ready, and the agent stayed `idle`. Ctrl-U is `ClaudeAdapter::clear_input_keys`. | verified |
| `--new-session` | With `--new-session`, a PTY resize from 120 to 150 columns did not redraw the screen: the harness has no controlling terminal, so it gets no `SIGWINCH`. Without the flag, it redrew at 150 columns. | verified |
| `sessions/<pid>.json` | Claude Code writes a live-process record to `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` (and a `<pid>.<hash>.key` file). In the sandbox PID namespace the pid is always `2`, so all agents would write one file, and each agent would see the operator's own sessions. | verified |
| Other writes to the shared home | `history.jsonl` (the typed prompt), `backups/.claude.json.backup.<ms>` (one each start), `session-env/<id>/`, `shell-snapshots/`, `usage-cache.json`, `projects/-home-agent-work/`. `.credentials.json` and the shared `.claude.json` did not change. | verified |
| Root of the sandbox | `/lib`, `/lib64`, `/bin`, `/sbin` are symlinks on this host (merged `/usr`). The sandbox makes the same symlinks with `--symlink`. | verified |
| External runner | The same test in runner process mode (`RunnerMode::Process`, the daemon default): `terrarium-runner` held the PTY, with the full `bwrap` argv in `runner.json`. The idle prompt came 0.3 s after the start call. The hooks (`SessionStart`, `UserPromptSubmit`, `Stop`) came in through the runner process. Ctrl-U, the resize to 150 columns, and one `haiku` turn (`pong`, 4.7 s) went through the control socket. The statuses were the same as in-process. The agent did not call the agent CLI in this run, so the agent RPC tunnel is verified only by `tests/runtime.rs` with `TERRARIUM_TEST_RUNNER_MODE=process`. One model turn. Run on 2026-10-24. | external runner: verified |

| Layout split (F1) | After the split of the agent dir (`docs/security-review.md`, F1): `harness/` bound read-only at its own path, the five Claude overlays (`.claude.json`, `sessions/`, `history.jsonl`, `backups/`, `projects/-home-agent-work/memory/`) read-write from `harness/claude/`, and `private/` not bound. Runner process mode (the daemon default), `bwrap`, `haiku`. The quiet start, the hooks, Ctrl-U, the resize to 150 columns, and one turn (`pong`, 4.2 s) worked as before. `sessions/2.json` and its key file landed in `harness/claude/sessions/`. `events.jsonl` was in `private/`, and the sandbox command line named no path in `private/`. The agent ran `terrarium mail check` through the RPC tunnel. One model turn. Run on 2026-10-24. | verified |

Changes that this run made to the sandbox (PLAN §11):

1. `--new-session` is off by default (`--sandbox-new-session` turns it
   on). It guards against `TIOCSTI` injection into a terminal that the
   sandbox shares with its parent. Each sandbox has its own PTY here, so an
   injection can reach only the agent's own input. Kernels from 6.2 also
   refuse `TIOCSTI` by default (`dev.tty.legacy_tiocsti = 0` on this host).
2. Each agent gets its own `sessions/` directory, bound over
   `$CLAUDE_CONFIG_DIR/sessions`, beside the per-agent `.claude.json`.
   After the change, the record went to the agent dir and the shared
   `sessions/` did not change.
3. A harness binary outside `/usr` (the native Claude build in
   `~/.local/share/claude/versions/`) runs by its real path, and its
   directory is bound read-only at the same path.

Not tested in this run: Docker (the seccomp profile of "bwrap inside
Docker" still applies), OAuth token refresh, and a question dialog. Two
agents at once and a real `--resume` were verified later; see "Two Claude
agents on one home" below.

---

### Two Claude agents on one home

Spike: `spikes/claude/concurrency/run.py` (4 haiku turns). Two sandboxed agents started at once on the shared home, each sent one turn, then each was stopped and resumed.

| Topic | Fact | Status |
|---|---|---|
| Two agents at once | Both started at the same time and came to `idle`. Both turns ended (`turn_end`). The first turn took about 6 s for both. | verified |
| Hook streams | Each agent got its own `session_started`, with its own UUID. The events of one agent do not contain the session UUID of the other agent. | verified |
| Real `--resume` | After stop and start, the status reason was `resuming session <id>`. `SessionStart` came with `source: "resume"` and the same UUID and `transcript_path`. The resumed turn answered `alpha-<nonce>` and `beta-<nonce>`: each agent remembered its own reply, not the reply of the other agent. The turn went into the same transcript file. | verified |
| Start after resume | The prompt was ready about 0.3 s after the start call. No dialog. | verified |
| Per-agent `history.jsonl` | Each agent's file had 2 lines (its two turns). Each had its own token and not the token of the other agent. The shared `history.jsonl` did not get the nonce. | verified |
| Per-agent `.claude.json` | `numStartups` was 2 in each per-agent copy. Claude Code writes to a state file that is a file bind mount, and the write lands in the per-agent copy. | verified |
| Shared `.claude.json` | Did not change (sha256 the same before and after). | verified |
| `.credentials.json` | Did not change (sha256 the same). No token refresh happened in the run. | verified |
| Per-agent `sessions/` | Each agent had `2.json` and `2.2628e0…d85.key`. The key file name is the same in both agents, and the same as a file that an earlier run (before the overlay) left in the shared `sessions/`. The name comes from the pid, so without the overlay the agents overwrite one file. | verified |
| Per-agent `backups/` | Each agent had one `.claude.json.backup.<ms>`, after two starts. Nothing new in the shared `backups/`. | verified |
| Per-agent auto memory | `projects/-home-agent-work/memory/` stayed empty in both agents. The haiku turns wrote no memory. | verified (no write) |
| Other writes to the shared home | `projects/-home-agent-work/<uuid>.jsonl` (one per agent), `session-env/<uuid>/` (one per agent, empty), and one `shell-snapshots/snapshot-bash-<ms>-<random>.sh`. Each is keyed by the session UUID or has a unique name. No other new file. | verified |
| Collisions not covered | None found. No new overlay was necessary. | verified |

Auto memory: the adapter now passes `autoMemoryEnabled: false` in the `--settings` JSON. The 2.1.310 settings schema says this stops reads and writes of the auto-memory directory. No model turn has proved it yet, so the `projects/<cwd>/memory/` overlay stays until the compatibility suite confirms no writes. The env var `CLAUDE_CODE_DISABLE_AUTO_MEMORY` also exists and is not used.

Per-agent overlays after this spike: `.claude.json`, `sessions/`, `history.jsonl`, `backups/`, `projects/<cwd>/memory/`. Shared: `.credentials.json`, transcripts, `session-env/`, `file-history/`, `todos/`, caches. Not tested: an OAuth token refresh with two agents, real writes to auto memory, more than two agents, Docker.

Per-agent overlays since 2026-10-25 (see "Per-agent session files" below): `.claude.json`, `sessions/`, `history.jsonl`, `backups/`, `projects/<cwd>/` (the transcripts, the subagent transcripts, and `memory/`), `file-history/`, `todos/`, `session-env/`. Shared: `.credentials.json`, `shell-snapshots/`, `security_warnings_state_<uuid>.json`, `paste-cache/`, caches. The old `harness/claude/memory/` moves to `harness/claude/projects/<cwd>/memory/` at the next start.

### Facts from the Docker smoke tests (2026-10-25)

- Claude: `/exit` makes one small model call (about 5 tokens) after `SessionEnd`.
- Claude: all agents share `projects/-home-agent-work/`, so an agent can read the transcripts of its peers. Codex (`sessions/`) and Copilot (`session-state/`) are the same. The live cross-harness run (2026-10-25) showed that this crosses projects too. Fixed: see "Per-agent session files" below.
- bwrap leaves empty mount-point stubs in the shared homes (`config/copilot`, `config/codex`) for the per-agent overlays.

### Per-agent session files (2026-10-25)

Each agent's session files are now read-write overlays from its own `harness/<harness>/` dir, bound over the session area of the shared home. No agent sees the transcripts of another agent, in its project or in another project. The shared home keeps the login, the settings, and the caches.

| Harness | Per-agent overlays (in `harness/<harness>/`, bound at the same path in the home) | How a resume finds the session |
|---|---|---|
| Claude Code | `projects/<encoded cwd>/`, `file-history/`, `todos/`, `session-env/` | `--resume <uuid>` reads `projects/<encoded cwd>/<uuid>.jsonl` ("Session file" above). It searches the project dirs of the home ("Resume from another cwd"); in the sandbox the cwd dir is the agent's own. Claude Code keeps no other index. |
| Codex | `sessions/`, `session_index.jsonl`, `shell_snapshots/`, and a SQLite home `harness/codex/sqlite/` (bound over itself; `-c sqlite_home="<dir>"`) | `codex resume <id>` looks the ID up in the `threads` table of `state_5.sqlite` (`rollout_path`), else in `sessions/`. A new SQLite home backfills `threads` from `$CODEX_HOME/sessions/` at the start (verified: a copied rollout got a `threads` row and a `backfill_state` of `complete`, no model turn). Both are the agent's own. `session_index.jsonl` (the thread names) is needed only for a resume by name and for the picker. |
| Copilot CLI | `session-state/`, `session-store.db`, `session-store.db-wal`, `session-store.db-shm`, `logs/` (the same dir as `--log-dir`; the extension subprocess writes `logs/process-<ts>-<pid>.log` in the home and ignores `--log-dir`) | `--resume <id>` reads `session-state/<id>/` ("Session file" in the Copilot section). The daemon finds the ID in `harness/copilot/session-id` and checks `harness/copilot/session-state/<id>/`. |
| pi | none new: `sessions/` was per agent already | `--session <id>` in the agent's own `sessions/` |

Facts behind the choice:

- Codex `state_5.sqlite` `threads` holds the `title` and the `first_user_message` of every session, and `thread_history_1.sqlite` `thread_items` holds every item of every turn, for all agents. A per-agent `sessions/` alone does not close the gap. `-c sqlite_home="<dir>"` is accepted with `--strict-config` on 0.160.0 and puts all six SQLite databases (`state_5`, `logs_2`, `memories_1`, `goals_1`, `queue_1`, and later `thread_history_1`) in that directory; `CODEX_HOME` gets none (an unauthenticated start, no model turn). The env var `CODEX_SQLITE_HOME` also exists; the config key wins over it.
- Codex `shell_snapshots/<id>.<ns>.sh` holds the exported env of the agent's shell (`declare -x`), with `TERRARIUM_AGENT` and any API key of the launch env.
- Copilot `session-store.db` has a `turns` table with the user message and the reply of every turn of every session, and Copilot has a `session_store_sql` tool ("Querying past sessions") that reads it. Its path is not configurable (no env var found in `cli-main.js` 1.0.102), so the three files are file binds. SQLite on a file bind: the close cannot delete the `-wal` file (EBUSY), SQLite ignores that, and the next open reads the data. Verified in `bwrap` with Python `sqlite3` and with `node:sqlite` (Node 26), two opens each. A Copilot start on these binds is not tested yet.
- Codex `session_index.jsonl` is appended to (one line per name, the newest wins). A rewrite by rename would fail with EBUSY on the file bind; the binary logs such a failure as "failed to index thread name". Not tested with a real rename of a thread.

Migration: nothing moves. The transcripts from before the change stay in the shared home, where the binds hide them from every agent. An agent that resumes a session from before the change does not find it in its own dir. Codex and Copilot exit at once for a missing ID ("No saved session found with ID", "No session, task, or name matched"); for Claude Code this is not tested. The supervisor then starts a new session with the recap turn (`new_session: "resume_failed"`). Not tested with a real resume.

The usage collector reads only the agent's own session dir in the sandbox (`docs/security-review.md`, F12). With sandbox mode `none`, no overlay is bound; the harness writes to the shared home, and the collector allows it there.

Not tested: a real resume of each harness on the new binds, a Copilot start with the `session-store.db` binds, and a Codex thread rename on the `session_index.jsonl` bind.

- Claude `--resume` of a session hidden by the per-agent `projects/` overlay exits 1 with "No conversation found with session ID"; the daemon records `resume_failed` and starts a new session with the recap turn (verified 2026-10-25). The `/exit` call recorded no usage sample in that run.

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
11. **§11 `--new-session`.** It stops PTY resizes from reaching the harness.
    The sandbox leaves it off by default ("Claude inside bwrap").
12. **§11 the shared Claude home.** `sessions/<pid>.json` collides in the
    sandbox PID namespace. Bind a per-agent `sessions/` directory over it,
    as with `.claude.json`.

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
- Claude Code itself inside bwrap inside Docker (PTY, network, home binds).
  It works outside Docker (see "Claude inside bwrap"). The hook process gets
  the launch environment: verified there.
- Managed (policy) settings.
- `DISABLE_AUTOUPDATER` and `DISABLE_UPDATES` by network capture.

---

## Plan accounts (PLAN §16.3)

The account design rests on these facts of the pinned versions. The sources
and the excerpts are in `docs/research/credential-sharing.md` (static
analysis, 2026-10-27). **[V]** is read in the code; **[I]** is inferred and
needs a live check.

| Harness | Fact | Status |
|---|---|---|
| Claude Code 2.1.310 | `CLAUDE_SECURESTORAGE_CONFIG_DIR` moves the credential dir away from `CLAUDE_CONFIG_DIR` (`O0()`). Not in the public docs. | [V] code |
| Claude Code 2.1.310 | It stats `.credentials.json` before each model request and reads it again on an mtime change; on a 401 it reads the file again and takes a newer token. | [V] code |
| Claude Code 2.1.310 | Locks: `proper-lockfile` dirs `<dir>/.oauth_refresh.lock` (`realpath: false`, `stale: 60000`, `update: 5000`), `<realpath(dir)>.lock` (legacy; a failure other than `ELOCKED` is ignored), and `<dir>/.storage-write.lock` (`stale: 15000`) around every save. The dead-holder takeover needs an owner file `.oauth_refresh.lock.owner`; with none it answers `no_owner_record` and waits. | [V] code |
| Claude Code 2.1.310 | Writes `.credentials.json` by a temp file in the same dir and a rename (an in-place fallback with `O_NOFOLLOW` on `EBUSY`). Through a **dir** symlink both stay in the target dir. | [V] code |
| Claude Code 2.1.310 | `oauthAccount` in `.claude.json`: the profile call, the org UUID of remote settings and some caches, and the display. The bootstrap at start updates it from the server. Model requests send the token only. | [V] code; the effect of a stale value after a live swap is [I] |
| pi 1.4.0 | `auth.json` is `<PI_CODING_AGENT_DIR>/auth.json`; no env var moves it alone. pi stats it (following links) before each read and reloads on a change of dev, inode, size, or times. It writes in place (following links). Its lock is `proper-lockfile` on the auth path with `realpath: false`: `<path>.lock`. | [V] code |
| Codex 0.164.0 | `auth.json` is fixed at `$CODEX_HOME/auth.json`; it writes in place and follows links; no cross-process lock; it refuses to load another account id while it runs ("account guard"). | [V] code |
| Copilot CLI 1.0.103 | `COPILOT_GITHUB_TOKEN` wins over the keychain and `config.json`. The `gho_` token of the device flow does not expire and is not refreshed. | [V] docs and strings; the closed runtime is [I] |

Live checks to do before the first second account (do them with a copied
test login, never with Wil's logins):

1. Claude Code honours `CLAUDE_SECURESTORAGE_CONFIG_DIR` for read, write,
   and `claude auth status` through the `cred/account` dir symlink.
2. A forced refresh (a copied, expired test login) writes through the dir
   symlink, takes `.oauth_refresh.lock` in the account, and makes
   `claude.lock` in the store.
3. A live swap reaches the next request (a turn after the swap is billed to
   the new account: check its usage window).
4. A stale `oauthAccount` after a live swap breaks nothing until the next
   start.
5. pi: a login link through `/run/terrarium/cred/account/auth.json` reads
   and refreshes; a swap reaches the next request.
6. Copilot: `COPILOT_GITHUB_TOKEN` with a `gho_` token works with no login
   in `config.json`, and Copilot does not write the token into the shared
   `config.json`.
7. Codex: `codex resume` after a swap keeps the conversation on the new
   account.

## Codex 0.160.0

Tested on 2026-10-24. Same host. ChatGPT Pro login (OAuth, `auth_mode`
`chatgpt`). Model for all turns: `gpt-6-luna` with
`model_reasoning_effort="low"`. Spikes are in `spikes/codex/`. Each spike
copies only `~/.codex/auth.json` into a temp `CODEX_HOME`.

### Summary

| Topic | Fact | Status | Spike |
|---|---|---|---|
| Recommended launch | `codex --no-daemon --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust -m <model> -c projects.<cwd>.trust_level="trusted" -c check_for_update_on_startup=false -c analytics.enabled=false -c feedback.enabled=false -c 'hooks.<Event>=[{hooks=[{type="command",command="..."}]}]' ...` with `CODEX_HOME` set | verified | 1, 2 |
| Hook injection | `-c hooks.<Event>=[...]` works in interactive mode. The binary calls this source `session_flags`. No file in `CODEX_HOME` changes. | verified | 1 |
| Hook trust | Hooks from `-c` show a "Hooks need review" dialog (default "Review hooks") unless `--dangerously-bypass-hook-trust` is given. The flag adds a start-up warning ("⚠ 1 warning · f2 to view"). | verified | 2 |
| Hook events | `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`. There is no `Notification` event. An unknown event name is ignored without a message. | verified (list from the binary; codex counts 11) | 1, 2 |
| Hooks that fire in one turn | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`; `SessionEnd` at exit | verified | 1 |
| `SessionStart` timing | It does **not** fire at start-up. It fires when the first prompt is submitted, 0.45 s after the Enter byte, just before `UserPromptSubmit`. A session file with only `session_meta` can still appear: an exit before any prompt writes one, and `SessionEnd` then fires with no `SessionStart` before it (Docker re-check, 2026-10-25). | verified | 1 |
| Hook payload IDs | Every payload has `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `model`, `permission_mode` (except `SessionEnd`). Turn hooks also have `turn_id`. `Stop` has `last_assistant_message`. | verified | 1 |
| "Turn ended" | `Stop` | verified | 1 |
| "Needs input" | `PermissionRequest` is the only candidate. It did not fire in yolo mode. The `request_user_input` tool was not tried. | not tested | — |
| `UserPromptSubmit` latency | 0.86 s for the first prompt (the session is created first), 0.04 s after that | verified | 5 |
| Yolo | `--dangerously-bypass-approvals-and-sandbox` and `-a never -s danger-full-access` both show "permissions: YOLO mode" in the header. No "are you sure" dialog. | verified | 2 |
| Trust dialog | Only in a git repository: "Folder access … Trust this folder?" (default "1. Trust and continue"). A cwd that is not a git repo has no dialog. | verified | 2 |
| Trust pre-seed | `-c projects.<cwd>.trust_level="trusted"` (no quotes round the path) removes the dialog. The path must not contain a dot, because `-c` splits the key on dots. A `[projects."<cwd>"]` table in `config.toml` also works. | verified | 2 |
| Update banner | Shows when `version.json` has a newer `latest_version`. `-c check_for_update_on_startup=false` removes it. | verified | 2 |
| App-server daemon | A start with **no** `-c` override starts a detached `codex app-server --managed-daemon` and a `pid-update-loop`. Both outlive the TUI. They copy 736 MB into `$CODEX_HOME/packages/`. Any `-c` or `--enable` override, or `--no-daemon`, runs the TUI without the daemon. | verified | 2 |
| Quiet keys | `check_for_update_on_startup`, `analytics.enabled`, `feedback.enabled`, `tui.resume_cwd`, `developer_instructions`, `model_instructions_file` are known keys (`--strict-config` accepts them; an unknown key fails). | verified | 2 |
| Prompt ready | 0.28 to 0.37 s after spawn. The composer shows for about 0.35 s **before** a start-up dialog replaces it. | verified | 2, 5 |
| Set session ID | Not possible. Read it from the `SessionStart` hook. `/status` also shows it before the first turn. | verified | 3, 8 |
| Resume | `codex resume <id>`. `SessionStart.source` is `resume`. The ID stays the same. | verified | 3 |
| Resume a missing ID | Exits at once: `ERROR: No saved session found with ID <id>.` | verified | 3 |
| Resume from another cwd | A "Working directory · resume" dialog asks which cwd to use. `-c tui.resume_cwd="session"` removes it. | verified | 3, 5 |
| Two resumes of one ID | The second shows "This conversation is open in another app … r retry f fork esc/ctrl+c/q exit" and waits. | verified | 3 |
| Clean exit | `/exit`, a pause of 0.3 s, then CR, in two writes. The process exits after 0.05 to 5.1 s. `SessionEnd.reason` = `other`. The exit text has the token usage and `codex resume <id>`. `/exit\r` in one write does not submit: `/exit` stays in the composer, and the stop falls back to the kill (the Codex smoke test in Docker, 2026-10-25). Every spike sent `/exit`, waited 0.3 s, then sent CR. The adapter sets `exit_key_pause` = 300 ms. | verified | 1, 3 |
| Session file | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<id>.jsonl`, plus `$CODEX_HOME/session_index.jsonl` (`id`, `thread_name`, `updated_at`). The hook `transcript_path` gives the exact path. | verified | 3 |
| Usage in the session file | `token_usage_record` and `event_msg/token_count` records. `token_count` also carries `rate_limits` with the plan. See "Usage accounting". | verified | 3 |
| `model_instructions_file` | Replaces the whole base prompt. `session_meta.base_instructions` = the file text, `provenance.type` = `custom`. A missing file stops the start: `failed to read model instructions file`. | verified | 4 |
| `developer_instructions` | Adds a `developer` message. The default prompt stays. | verified | 4 |
| `AGENTS.md` | Loaded from the workspace as a `user` message. Works beside `developer_instructions`. | verified | 4 |
| Prompt on resume | **Frozen.** `codex resume` with a changed `developer_instructions` sent the old text. The new text is not in the session file. | verified | 4 |
| Adapter default (2026-10-26) | An agent with no `system_prompt` gets `developer_instructions` (append; the Codex base prompt stays). An agent with a `system_prompt` gets `model_instructions_file` (replace). Before this date the adapter always replaced. An agent made before keeps its frozen session until the prompt hash changes. Replace verified live 2026-10-27 (gpt-5.6-luna: the reply followed the system prompt and named the Terrarium mail command). | verified | 4 |
| Submit a turn | Bracketed paste, then CR at once. Newlines stay in the prompt. | verified | 5 |
| Bare LF | Adds a new line. It does not submit. | verified | 5 |
| Type while busy | The text **steers** the running turn: the same `turn_id`, one `Stop` for both prompts. `UserPromptSubmit` for the second prompt came 4.5 s after its CR, at the next model request. | verified | 5 |
| Auth status | `codex login status`: stderr `Logged in using ChatGPT`, exit 0. No login: `Not logged in`, exit 1. **stdout is empty in both cases.** A reader of stdout alone sees no login. Seen again in the container (codex-cli 0.160.0, 2026-10-25) after `codex login --device-auth`, the same with and without `CODEX_HOME` set (fixture `crates/adapters/codex/tests/fixtures/auth/login-status.json`). | verified | 7 |
| Plan | `id_token` claim `https://api.openai.com/auth.chatgpt_plan_type` (`pro`). `/status` shows `Account: Pro 200`. `token_count.rate_limits.plan_type` = `pro`. | verified | 7, 8 |
| Quota | `/status` shows `Weekly limit: [███…] 100% left (resets 7:42 AM on 31 Oct)` with no turn sent. | verified | 8 |
| `exec --json` names | `thread.started`, `turn.started`, `turn.completed`, `turn.failed`, `item.started`, `item.completed`, `error` | read (binary) | — |

### Hooks

Sequence for one turn with one shell call (spike 1, times from the Enter
byte):

| t (s) | Hook | Useful fields |
|---|---|---|
| 0.45 | `SessionStart` | `source` (`startup` or `resume`), `model`, `permission_mode` (`bypassPermissions`) |
| 0.46 | `UserPromptSubmit` | `prompt`, `turn_id` |
| 2.43 | `PreToolUse` | `tool_name` (`Bash`), `tool_input.command`, `tool_use_id` |
| 2.50 | `PostToolUse` | `tool_response` (a string: the output) |
| 3.69 | `Stop` | `last_assistant_message`, `stop_hook_active` |
| 25.29 | `SessionEnd` | `reason` (`other` after `/exit`) |

The payloads are in `spikes/codex/hooks-in-interactive/captured/hooks.jsonl`.
The hook format is the Claude Code format (`hooks.<Event>` = a list of
`{matcher?, hooks: [{type: "command", command, timeout?}]}`).

Notes:

- The user's own `~/.codex/hooks.json` and `[hooks.state]` trust hashes
  are in the shared home. With a shared `CODEX_HOME`, those hooks also fire
  in each agent. The spikes used an empty `CODEX_HOME`, so this is not
  verified. No Codex equivalent of `--setting-sources ""` was found.
- `PermissionRequest` did not fire in yolo mode. A Codex agent in yolo mode
  has no hook for "needs input".

### Quiet start

To get to the idle composer with no dialog:

1. Set `CODEX_HOME` to the harness home.
2. Pass `--no-daemon`.
3. Pass `-c projects.<cwd>.trust_level="trusted"` (or put the table in
   `config.toml`). This matters only in a git repository.
4. Pass `--dangerously-bypass-hook-trust` with the `-c hooks...` overrides.
5. Pass `-c check_for_update_on_startup=false`.
6. On resume, pass `-c tui.resume_cwd="session"`.

First screen for each variant (spike 2):

| Variant | First screen |
|---|---|
| no flags, not a git repo | idle composer |
| no flags, git repo | "Folder access" trust dialog |
| yolo, git repo | "Folder access" trust dialog |
| `-a never -s danger-full-access` + trust by `-c` | idle composer, "permissions: YOLO mode" |
| yolo + trust + `-c` hooks | "Hooks need review" dialog |
| yolo + trust + hooks + `--dangerously-bypass-hook-trust` | idle composer, "⚠ 1 warning" |
| newer version in `version.json` | idle composer under an "Update available!" box |
| same + `check_for_update_on_startup=false` | idle composer, no box |

Codex writes `config.toml` on the first start (`[tui]
screen_reader_detection_done = true`, `[tui.model_availability_nux]`). It
also writes `installation_id`, `models_cache.json`, `version.json`, and
several SQLite files (`state_5`, `logs_2`, `memories_1`, `goals_1`,
`queue_1`) into `CODEX_HOME`. With a temp `CODEX_HOME` under `/tmp` it
prints "Refusing to create helper binaries under temporary dir".

### Session file and usage accounting

Record types in one two-turn session: `session_meta`, `turn_context`,
`response_item/message`, `world_state`, `token_usage_record`,
`event_msg/{task_started, item_completed, token_count, task_complete,
thread_settings_applied}`.

`token_usage_record` (one per model response):

```json
{"type": "token_usage_record", "payload": {"thread_id": "<id>", "turn_id": "<id>",
 "response_id": "resp_...", "usage": {"input_tokens": 14160, "cached_input_tokens": 11392,
 "cache_write_input_tokens": 0, "output_tokens": 5, "reasoning_output_tokens": 0,
 "total_tokens": 14165}, "turn_token_usage": {...}, "thread_token_usage": {...}}}
```

`event_msg/token_count` (running totals plus the rate limits):

```json
{"type": "token_count", "info": {"total_token_usage": {...}, "last_token_usage": {...},
 "model_context_window": 258400},
 "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 0.0,
 "window_minutes": 10080, "resets_at": 1793396550}, "secondary": null,
 "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
 "plan_type": "pro", "rate_limit_reached_type": null}}
```

Rules for the tailer:

- Use `token_usage_record.usage` per response. It has a `response_id` for
  de-duplication. `thread_token_usage` is the running total.
- The model name is in `turn_context.model` (one per turn), not in the
  usage record.
- `token_count.rate_limits` is a free quota probe: it comes with every
  turn. On this Pro plan only the 7-day primary window was present.
- `session_meta` holds `creator_user_id` and `creator_account_id`, and the
  whole base prompt in `base_instructions`.

### Typing into the TUI

The idle screen ends like this (spike 5):

```
› Ask Codex to do anything          <- placeholder, drawn dim (SGR 2)
  GPT-6-Luna low · /path · <title>
  ? for shortcuts                                       ⚠ 1 warning · f2 to view
```

While it works, a line `• Working (1s • esc to interrupt)` is above the
composer.

Detector (`screen_state()` in `spikes/codex/lib/codexlib.py`):

1. The composer is the last line that starts with `›` and is not a
   numbered option (`› 1. …`).
2. The input text is the text after `›` with dim characters blanked
   (`Session.dim_mask()`). The placeholder is dim.
3. `busy` = "esc to interrupt" or "Working (Ns" in the four lines above the
   composer or in the footer.
4. `dialog` = one of the last six non-empty lines has "enter continue",
   "enter confirm", "· esc quit/skip/cancel/use session", or
   "esc/ctrl+c/q exit".
5. `ready` = composer, empty input, not busy, no dialog, **and the same for
   1 s**. The composer flashes for about 0.35 s before a trust dialog.

`typing-into-tui/detector_check.py` runs the detector on 23 captured
screens (idle, busy, typed, trust, hook review, update banner, resume cwd,
"open in another app"). All were correct. The composer can hold more than
one line; the detector reads only the first.

---

## Codex: facts that contradict the plan

1. **§4 and §21 version pinning.** A start with no `-c` override starts a
   shared app-server daemon in `CODEX_HOME`, which outlives the TUI, copies
   736 MB into `packages/`, and runs an update loop. On this host the user's
   own daemon is already 0.164.0 while the CLI is 0.160.0. Always pass
   `--no-daemon`.
2. **§7 step 3 (prompt-submit hook timeout and retry).** For text typed
   while busy, `UserPromptSubmit` came 4.5 s later, at the next model
   request. A short timeout plus "retry once" would type the text twice.
   The first prompt of a session also takes 0.86 s.
3. **§7 "Set session ID".** There is no flag. `SessionStart` fires only at
   the first prompt, so the runner has no session ID until the first turn
   (except by reading `/status`).
4. **§7 blocked detection.** There is no `Notification` event, and
   `PermissionRequest` does not fire in yolo mode. Codex gives no hook for
   "needs input".
5. **§8 system prompt.** `model_instructions_file` **replaces** the base
   prompt. To append, use `-c developer_instructions=...`. Both are frozen
   at session creation: a changed template does not apply on
   `codex resume`.
6. **§4 trust.** `--skip-git-repo-check` is an `exec` flag. The interactive
   trust dialog shows only in a git repository. Use
   `-c projects.<cwd>.trust_level="trusted"`; the path must have no dot.
7. **§8 hooks.** Hooks passed by `-c` need `--dangerously-bypass-hook-trust`
   or a dialog blocks the start.
8. **§7 typing while busy.** The text steers the running turn (one `Stop`
   for two prompts), as with Claude Code.

## Codex: surprises

- `/status` shows the session ID, the plan ("Pro 200"), and the weekly
  limit before any turn.
- The composer is drawn before the trust dialog, so a fast "ready" check
  can type into a screen that is about to change.
- `/exit` took 5 s in some runs.

- In the Docker smoke test the order of `turn_start` and `idle -> working` changed from turn to turn. Clients must not depend on it.
- The Codex session file name carries the start-up time, but its content starts at the first prompt.

- Live isolation check (2026-10-25): with `-c sqlite_home=<per-agent dir>` all six SQLite databases land in the agent's own dir and the shared home gains no new file; only the shared `thread-writer-locks/` mtime changes (Codex makes and deletes an empty lock per thread there).

## Codex: not tested

- The user's `~/.codex/hooks.json` with a shared `CODEX_HOME` (it would
  likely fire in every agent).
- `request_user_input` (question tool) and how to answer it.
- A changed `model_instructions_file` on resume.
- Whether the `/status` session ID equals the later `SessionStart`
  `session_id`.
- OAuth token refresh with many instances on one home.

---

## Codex 0.164.0

Tested on 2026-10-25 and 2026-10-26 in the Docker stack (image with
0.164.0), through the daemon: agent `smoke/probe-codex`, ChatGPT Pro
subscription, model `gpt-6-luna`, 1 turn ("Reply with exactly: pong"). The
adapter did not change, except the table name (`CODEX_PINNED`).

| Topic | Fact | Status |
|---|---|---|
| Flags | `codex --help` adds one subcommand, `migrate-nfs-db`. No flag added or removed. `codex resume --help` is the same. | read |
| Quiet start | starting → idle ("prompt ready") 0.31 s after spawn. The screen shows the idle composer, "permissions: YOLO mode", "⚠ 1 warning · f2 to view". No dialog. | verified |
| Terminal modes | `?1049h`, `?1000h`/`?1003h`/`?1006h`, `?2004h`, `>7u`: the same counts per start as 0.160.0. | verified |
| Hooks | The 11 registered names fire as on 0.160.0: `SessionStart` at the first prompt (source `startup`), then `UserPromptSubmit` → `turn_start`, `Stop` → `turn_end`, `SessionEnd` (reason `other`) after `/exit`. Tool hooks were not exercised (no tool call). | verified |
| New hook `PreModelRequest` | In the hook enum of the binary. It runs before each model request, not once per turn. The adapter does **not** register it: the payload and the effect of the hook's output are not captured, and a hook process before each request adds delay. Register it (log only) after a spike. `Interrupt` is in the enum too (also in 0.160.0) and is not registered. | read |
| Usage | `usage.sample` from the session file: input 3837, cache_read 7936, output 5. | verified |
| Stop | `/exit` → `SessionEnd` → exit 0, no kill. | verified |
| Session file | Same layout (`sessions/YYYY/MM/DD/rollout-<local time>-<id>.jsonl`, `session_index.jsonl`, `sqlite/`). `turn_context` lost `current_date`, `timezone`, `workspace_roots` and gained `cyber_access_program`; `task_started` gained `turn_attribution`; `task_complete` gained `root_turn_id`; `response_item/message` gained `status`; new `response_item/additional_tools`. `token_usage_record`, `token_count.rate_limits`, `session_meta`: no change. The adapter reads none of the removed keys. | verified |
| Exit text | "Token usage: total=3,842 input=3,837 (+ 7,936 cached) output=5": input no longer includes the cache. The adapter does not parse it. | verified |

---

## Copilot CLI 1.0.103

Tested on 2026-10-24. Same host. GitHub Copilot subscription on the legacy
premium-request billing. Model for all turns: `claude-haiku-4.5` (0.33x).
`gpt-5-mini` (0x) shows as "Unavailable" in `/model`. Spikes are in
`spikes/copilot/`. Each spike uses an empty temp `COPILOT_HOME`: the token
is in the system keychain, so no file is copied.

### Summary

| Topic | Fact | Status | Spike |
|---|---|---|---|
| Recommended launch | `COPILOT_HOME=<home> COPILOT_ALLOW_ALL=true COPILOT_AUTO_UPDATE=false copilot --yolo --no-auto-update --disable-client-telemetry --model <m> --plugin-dir <terrarium plugin> [--experimental --plugin-dir <prompt extension>] [--session-id <uuid>]` | verified | 1, 2, 4 |
| `--experimental` in the sandbox | Do **not** pass it. With the flag, Copilot writes `"experimental": true` to `settings.json` at **each** start by a rename, also when the key is there. In the sandbox `settings.json` is a per-agent file bind, and a rename over a bind mount fails with `EBUSY` (`rename '/home/agent/.copilot/settings.json.t' -> '/home/agent/.copilot/settings.json'`: `EBUSY: resource busy or locked`, from Node in the same sandbox). Copilot then exits with code 1 about 0.5 s after spawn, with no PTY output, no stderr, and no log file. The adapter writes `"experimental": true` into the per-agent `settings.json` instead. With the key and no flag, Copilot does not write the file at start, the banner shows `/experimental`, and the log says "Installed 1 native extension(s)" (0 with no key and no flag). | verified (2026-10-25, container) | — |
| Rename-over writes | Copilot replaces `settings.json`, `command-history-state.json`, `open-sessions-state.json`, and `config.json` by a rename (the inode changes). On a file bind the history and open-sessions writes fail and Copilot goes on: the log has `[cli_persistence::open_sessions] Failed to record an open session: ... Device or resource busy (os error 16)`, and the per-agent history stays `[]` after `/help`. So those two overlays keep private data out of the shared home, but their content is never updated. A `settings.json` write after start (`/model`, `/theme`) is lost in the same way; whether Copilot survives it is not tested. | verified (2026-10-25, container) | — |
| Hook routes | Four routes fire: `$COPILOT_HOME/hooks/*.json`, `hooks` in `$COPILOT_HOME/settings.json`, `--plugin-dir <dir>` with `hooks.json` at the plugin root, and `<cwd>/.github/hooks/*.json` (trusted folder). A plugin's `hooks/hooks.json` did **not** fire. | verified | 1 |
| Hook injection by flag | `--plugin-dir <dir>` with `plugin.json` and `hooks.json`. Nothing in `COPILOT_HOME` changes. There is no `--additional-mcp-config`-style flag for hooks. | verified | 1 |
| Hook format | `{"version": 1, "hooks": {"<Event>": [{"type": "command", "bash": "<cmd>", "timeoutSec": 10}]}}` | verified | 1 |
| Two payload dialects | PascalCase names (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd`, `Notification`) get snake_case keys (`hook_event_name`, `session_id`, `tool_name`, ...). camelCase names (`sessionStart`, `userPromptSubmitted`, `preToolUse`, `postToolUse`, `agentStop`, `sessionEnd`) get camelCase keys (`sessionId`, `toolName`, ...). `PermissionRequest` has camelCase keys either way. | verified | 1 |
| Hooks that fire in one turn | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`; `SessionEnd` at exit | verified | 1 |
| `SessionStart` timing | Fires at the first prompt, not at start-up. It has `source` (`new`, `resume`) and `initial_prompt`. | verified | 1, 3 |
| Payload IDs | Every payload has `session_id` and `cwd`. Only `Stop` has `transcript_path`. `Stop` has `stop_reason` (`end_turn`) but no assistant text. | verified | 1 |
| `PermissionRequest` | Fires for **every** tool call, also with `--yolo` and Allow All. It is not a "needs input" signal. | verified | 1 |
| "Needs input" | `Notification` with `notification_type` = `elicitation_dialog` ("Information requested"), after `PreToolUse` with `tool_name` = `AskUserQuestion`. | verified | 5 |
| "Turn ended" | `Stop`. See the notes: a background shell can start another agent turn after it. | verified | 5 |
| Idle `Notification` | None in 20 s of idle. | verified | 1 |
| Yolo | `--yolo` alone: folder-trust dialog. `COPILOT_ALLOW_ALL=true` alone: no dialog, but the footer says "Manual Approval". Both: no dialog, "Allow All". `COPILOT_ALLOW_ALL=1` does not trust the folder. | verified | 2 |
| Trust dialog | "Confirm folder trust … ❯ 1. Yes / 2. Yes, and remember / 3. No (Esc)". The default is "Yes". `COPILOT_ALLOW_ALL=true` or `config.json` `trustedFolders: ["<cwd>"]` removes it. | verified | 2 |
| Prompt ready | 0.36 to 0.45 s after spawn; 2.6 s to the trust dialog | verified | 2, 5 |
| Set session ID | `--session-id <uuid>` works. The hook `session_id` is the same UUID. | verified | 3 |
| Reuse a session ID | `--session-id <used uuid>` **resumes** the session. No error. | verified | 3 |
| Resume | `--resume <id>`; `SessionStart.source` = `resume`. It works from another cwd with no dialog. | verified | 3 |
| Resume a missing ID | Exits: `Error: No session, task, or name matched '<id>'.` | verified | 3 |
| Two resumes of one ID | Both start. No lock and no warning. | verified | 3 |
| Clean exit | `/exit` then CR: exits in 1.0 to 1.5 s, `SessionEnd.reason` = `user_exit`. Ctrl-C shows "ctrl+c again to exit"; a second Ctrl-C exits in 0.1 s. The exit screen has the AI credits, tokens, and `copilot --resume=<id>`. | verified | 1, 2, 3 |
| Session file | `$COPILOT_HOME/session-state/<id>/events.jsonl` plus `workspace.yaml`, `checkpoints/`, `rewind-file-snapshots/`. Also `$COPILOT_HOME/session-store.db`. | verified | 3 |
| Usage in the session file | `session.usage_record` per model call, `session.usage_checkpoint` and `session.shutdown` with totals. See "Usage accounting". | verified | 3 |
| System prompt extension | `copilot-custom-system-prompt` 0.1.2 loads only in experimental mode: `--experimental`, or `"experimental": true` in `settings.json` (the sandbox uses the key, see above). It loads from `$COPILOT_HOME/extensions/` after `install`, **or** from `--plugin-dir <package dir>` with no install. | verified | 4 |
| `APPEND_SYSTEM.md` | Appends to the default prompt (31 475 chars, starts "You are GitHub Copilot"). Sentinel answered. | verified | 4 |
| `SYSTEM.md` | Replaces the whole prompt (111 chars, no default text). Sentinel answered. | verified | 4 |
| Prompt on resume | The extension reads the files at each start. `--resume` with a changed `SYSTEM.md` used the new text (a second `system.message` record). | verified | 4 |
| Both files, empty files | Read from the installed package 0.1.2 (`dist/prompts.js`, 2026-10-26); the replace path verified live 2026-10-27 (claude-haiku-4.5 followed `SYSTEM.md` and named the Terrarium mail command): a file that is missing or only white space counts as absent. With a non-empty `SYSTEM.md` the mode is `replace`; a non-empty `APPEND_SYSTEM.md` then goes after it, joined by a blank line. With only `APPEND_SYSTEM.md` the mode is `append`. With neither, the extension does not load. The adapter writes exactly one of the two files with text (the other is empty), so the mode follows the agent's `system_prompt` (PLAN §8). | verified | 4 |
| Submit a turn | Bracketed paste, then CR at once. Newlines stay. The screen showed the paste only after more than 0.2 s. | verified | 5 |
| Bare LF | Adds a new line. It does not submit. | verified | 5 |
| Type while busy | Enter **steers**: the text joins the running work, and **no** `UserPromptSubmit` fires for it. The footer offers "ctrl+q enqueue" for a queued message. | verified | 5 |
| Answer a question | `ask_user` shows a list: "↑/↓ select · enter accept · ctrl+d decline · esc cancel". Down (`ESC [B`) then CR picked option 2. | verified | 5 |
| Auth status | No `auth status` command. `/user` prints "Signed in to https://github.com as <login> (GitHub.com)". With no token the TUI prints "Please use /login to sign in to use Copilot". | verified | 7 |
| Plain-text login | `copilot login --device-code` in the container (no credential store) writes `$COPILOT_HOME/config.json`: two `//` comment lines, then JSON with `authTokens: {"https://github.com:<login>:github": {"token": "gho_…"}, "https://github.com:<login>": {"token": "gho_…"}}` (the same token twice), `loggedInUsers: [{host, login, kind: "githubDotCom"}]`, and `lastLoggedInUser: {host, login}`. A strict JSON parser refuses the file. The daemon strips the comment lines and counts a non-empty `authTokens.<key>.token` as a login (fixture `crates/adapters/copilot/tests/fixtures/auth/config-plaintext-login.json`). | verified (2026-10-25) | 7 |
| Quota | `/usage` shows "Requests 0 Premium" and "Weekly ■■■… 0% used • resets on Oct 26 at 10 AM", with no turn sent. | verified | 8 |

### Hooks

Sequence for one turn with one shell call (spike 1, PascalCase route, times
from `SessionStart`):

| t (s) | Hook | Useful fields |
|---|---|---|
| 0.00 | `SessionStart` | `source`, `initial_prompt` |
| 0.04 | `UserPromptSubmit` | `prompt` |
| 2.15 | `PreToolUse` | `tool_name` (`Bash`), `tool_input` (`command`, `description`) |
| 2.28 | `PermissionRequest` | `toolName`, `toolInput`, `permissionSuggestions` (camelCase) |
| 2.37 | `PostToolUse` | `tool_result.result_type`, `tool_result.text_result_for_llm` |
| 3.59 | `Stop` | `transcript_path`, `stop_reason`, `stop_hook_active` |
| exit | `SessionEnd` | `reason` (`user_exit`) |

`UserPromptSubmit` came 0.95 s after the CR for the first prompt and 0.07 s
for later prompts (spike 5).

Notes:

- Every route fires, and the hooks of all routes run. A shared
  `COPILOT_HOME` brings the user's `hooks/*.json` (on this host
  `~/.copilot/hooks/orca.json`) into every agent. There is no way to load
  only the flag route. `disableAllHooks` would stop all routes (not
  tested).
- `UserPromptSubmit` also fires for prompts that Copilot makes itself:
  `<system_notification> Shell command … has completed </system_notification>`.
  Do not count it as a delivery.
- A shell command can run in the background. Then `Stop` fires before the
  command ends, and a `system_notification` turn starts later, with its own
  `Stop`.

### Quiet start

1. Set `COPILOT_HOME`.
2. Set `COPILOT_ALLOW_ALL=true` (folder trust) and pass `--yolo` (Allow
   All mode). Or put the cwd in `config.json` `trustedFolders`.
3. Set `COPILOT_AUTO_UPDATE=false` and pass `--no-auto-update`.
4. Pass `--disable-client-telemetry`.

First screen for each variant (spike 2):

| Variant | First screen | Footer mode |
|---|---|---|
| no flags, no env | trust dialog | — |
| `--yolo` | trust dialog | — |
| `--yolo`, `COPILOT_ALLOW_ALL=1` | trust dialog | — |
| `COPILOT_ALLOW_ALL=true` | idle | Manual Approval |
| `--yolo`, `trustedFolders` in `config.json` | idle | Allow All |
| `--yolo`, `COPILOT_ALLOW_ALL=true`, not a git repo | idle | Allow All |

The start screen always has a welcome box, a "Discover Auto" box, and tips.
They do not block. With auto-update on, nothing was downloaded into
`COPILOT_HOME` in 15 s.

Files outside `COPILOT_HOME`:

- The OAuth token is in the system keychain (libsecret over D-Bus). With
  the D-Bus address set to a dead path, Copilot starts logged out. `copilot
  login` offers a plain-text fallback in the config. `COPILOT_GITHUB_TOKEN`,
  `GH_TOKEN`, `GITHUB_TOKEN` override stored credentials (read).
- `~/.cache/copilot/` (`COPILOT_CACHE_HOME` or `XDG_CACHE_HOME`, read from
  the code): `exp-cache.json`, `managed-settings/`, `copilot-user-cache.json`,
  and the unpacked runtime in `pkg/linux-x64/<version>/`. The spikes wrote
  to the real `~/.cache/copilot`.

### Session file and usage accounting

Record types in one two-turn session: `session.start`, `session.resume`,
`session.model_change`, `session.permissions_changed`, `hook.start`,
`hook.end`, `user.message`, `system.message`, `assistant.turn_start`,
`assistant.message`, `assistant.turn_end`, `model.*`,
`session.usage_record`, `session.usage_checkpoint`, `session.shutdown`.

`session.usage_record` (one per model call):

```json
{"usage": {"model": "claude-haiku-4.5", "provider": {"id": "copilot"},
 "inputTokens": 20331, "outputTokens": 50, "cacheReadTokens": 13637,
 "cacheWriteTokens": 6684, "reasoningTokens": 42, "cost": 0.33,
 "initiator": "user", "isByok": false, "apiCallId": "msg_...",
 "accounting": {"sourceSessionId": "<id>", "sequence": 1, "usageId": "msg_..."},
 "copilotUsage": {"totalNanoAiu": 0}, "aiCreditsStatus": "complete"}}
```

- `cost` is the premium-request multiplier for that call.
- `session.usage_checkpoint.totalPremiumRequests` is the running total.
  `session.shutdown.accountingSnapshot.modelMetrics` has per-model totals.
- The title side call (`gpt-4o-mini`) is only in `model.model_call_success`
  records, with OpenAI-style `usage`. It is not in `usage_record`.
- `system.message` holds the system prompt that was sent. The default
  prompt contains "The current user is @<login>".
- `workspace.yaml` has `mc_task_id` and `mc_session_id`. This suggests a
  link to the cloud session history (`--sync-session`). Not tested.

### Typing into the TUI

The idle screen ends like this (spike 5):

```
 /path [⎇ main]
────────────────────────────────────────────────
❯
────────────────────────────────────────────────
 ← open sidebar · Interactive · Allow All · / commands · ? help · tab next tab     Claude Haiku 4.5
```

While it works, the footer is ` ◉ Working esc esc edit prompt` or
` ◎ Working · 256 B esc esc interrupt · ctrl+q enqueue`.

Detector (`screen_state()` in `spikes/copilot/lib/copilotlib.py`): the
last two rule lines with a `❯` line between them are the input box;
`busy` = the footer starts with `◉/◎ Working` or has "esc esc interrupt";
`dialog` = "↑/↓ (to) navigate/select" or "enter (to) select/accept" in the
last six lines. `typing-into-tui/detector_check.py` checked 12 screens plus
the 8 start-up screens. All were correct. One capture (the paste 0.2 s
after it was sent) was not drawn yet; it is labelled `lag-`.

---

## Copilot CLI: facts that contradict the plan

1. **§4 hooks "loaded from the trusted workspace".** Hooks load from four
   places. Use `--plugin-dir` for Terrarium's hooks. The shared home's
   own hooks (`hooks/*.json`, `settings.json`) also run in every agent, and
   there is no switch that keeps them out.
2. **§8 prompt extension install.** The extension does not need to be
   installed into the shared home: `--plugin-dir <package dir>` loads it.
   Only `SYSTEM.md` / `APPEND_SYSTEM.md` must be at `$COPILOT_HOME/`, so the
   per-agent file bind of §8 is still needed for the prompt file.
3. **§4 yolo.** `--yolo` does not skip the trust dialog. It needs
   `COPILOT_ALLOW_ALL=true` (exactly `true`). `COPILOT_ALLOW_ALL=true` alone
   does not give Allow All mode.
4. **§7 "needs input".** `PermissionRequest` fires for every tool call even
   in Allow All mode. Use `Notification` with `elicitation_dialog`.
5. **§7 step 3.** Text typed while busy fires no `UserPromptSubmit`, and
   `UserPromptSubmit` also fires for Copilot's own system notifications.
   The prompt-submit hook is not a reliable delivery receipt.
6. **§7 "turn ended".** `Stop` can fire while a background shell still
   runs; a later system-notification turn follows.
7. **§8 session ID.** `--session-id <used uuid>` resumes instead of
   failing. Two processes can resume one session at the same time with no
   warning. The daemon must prevent a second start itself.
8. **§11 shared home.** Copilot also writes `~/.cache/copilot/`, and its
   token is in the D-Bus keychain, which a sandbox does not have. A sandbox
   needs `COPILOT_GITHUB_TOKEN` (or the plain-text fallback) and a cache
   directory.
9. **§16.2 usage.** The plan is on legacy premium requests, not AI credits.
   `session.usage_record.cost` is the multiplier. The cheapest available
   model is 0.33x.

## Copilot CLI: surprises

- The default system prompt names the GitHub login.
- A plugin's `hooks/hooks.json` (Claude plugin layout) does not load; the
  plugin-root `hooks.json` does.
- The paste is drawn late (more than 0.2 s), but the CR right after it
  still submits the full text.

- Docker re-check (2026-10-25): Copilot runs in the bwrap sandbox with `"experimental": true` in the per-agent `settings.json`; the agent prompt reaches the model through `APPEND_SYSTEM.md`, after Copilot's own prompt. Stop with `/exit\r` in one write exits 0 in 0.19 s with `SessionEnd` reason `user_exit`, so Copilot needs no `exit_key_pause`.
- At the first prompt Copilot makes one extra model call to `gpt-4o-mini-2024-07-18` for the session title. The exit screen showed "AI Credits 0", so it appears free.

- Live isolation check (2026-10-25): Copilot starts on the three `session-store.db*` file binds with no SQLite error; the `-wal` file stays after exit and the next open reads it. A hidden session on `--resume` exits 1 with "No session, task, or name matched", and the daemon starts a new session with the recap turn. The system-prompt extension subprocess writes its log to the shared `logs/`, not to `--log-dir`; it holds only the session ID. Fixed: the per-agent `logs/` (the `--log-dir` dir) is bound over `$COPILOT_HOME/logs/` (re-checked live on 2026-10-25: a start and stop added 0 files to the shared `logs/` and 18 to the per-agent one).

## Copilot CLI: not tested

- `disableAllHooks` with the `--plugin-dir` route.
- Hooks without folder trust.
- A model turn inside the container sandbox. The keychain-less start is
  verified (2026-10-25): with the sandbox argv of the daemon and no
  `--experimental`, Copilot reads the plain-text login, shows the prompt
  (`❯`, "Loading: 1 plugin, 13 hooks, 2 skills, 1 MCP server", the model
  `claude-haiku-4.5`), loads the extension, and exits with code 0 on
  `/exit`. That the model sees the prompt from `APPEND_SYSTEM.md` with the
  key instead of the flag is not tested.
- `--sync-session` and what `mc_task_id` sends to GitHub.
- Concurrent writes to one `session-state/<id>/` from two processes.

---

## pi 0.85.1

Tested on 2026-10-24. Same host. pi has **no** OAuth login on this host
(`~/.pi/agent/auth.json` is `{}`). It has three custom LAN providers with
API keys in `models.json`. Model for the completed turns:
`openai-codex/gpt-5.6-luna`, thinking off, with a copy of Codex's access
token (see "Auth"). Spikes are in `spikes/pi/`. Each spike uses a temp
`PI_CODING_AGENT_DIR` with a copy of `models.json` and a new `auth.json`.

### Summary

| Topic | Fact | Status | Spike |
|---|---|---|---|
| Recommended launch | `PI_CODING_AGENT_DIR=<dir> PI_OFFLINE=1 PI_TELEMETRY=0 TERRARIUM_SOCKET=<sock> pi --provider <p> --model <m> --thinking <t> --offline -e <terrarium.ts> --no-skills --session-id <uuid> [--system-prompt <text>] [--append-system-prompt <text or file>]` | verified | 1, 3, 4 |
| Event extension | `spikes/pi/terrarium-ext/terrarium.ts`, loaded with `-e`, sends one JSON line per event to `TERRARIUM_SOCKET` (Unix socket). It works in interactive mode. | verified | 1 |
| Events in one turn | `input`, `before_agent_start`, `agent_start`, `turn_start`, `message_end`, `tool_execution_start`, `tool_execution_end`, `turn_end` (repeat per model call), `agent_end`, `agent_settled` | verified | 1 |
| Start-up event | `session_start` (`reason` `startup`) fires **at start-up**, 0.35 s after spawn, before any prompt. | verified | 1, 5 |
| Payload IDs | The extension adds `session_id`, `session_file`, `cwd`, `model` (`<provider>/<id>`) to every line from `ctx.sessionManager` and `ctx.model`. | verified | 1 |
| "Prompt submitted" | `input`, 0.01 to 0.10 s after the CR. It has `prompt`, `source` (`interactive`), `streaming_behavior` (`null`, `steer`, `followUp`). | verified | 1, 5 |
| "Turn ended" | `agent_settled` (`ctx.isIdle()` = true). `agent_end` and `turn_end` are not enough: they repeat for each model call and for each automatic retry. | verified | 1 |
| "Needs input" | `ui_prompt_start` / `ui_prompt_end` (extension UI prompts). pi has no built-in question tool. | read (docs) | — |
| Tool approval | None. The bash tool ran with no prompt and no flag. `--approve` is about project trust, not tools. | verified | 1 |
| Project trust | A cwd with `.pi/settings.json` shows "Trust project folder?" (default "→ Trust", which saves to `trust.json`). `--approve`, `--no-approve`, or the extension's `project_trust` answer (`TERRARIUM_TRUST=yes`) remove it. A cwd without `.pi/` has no dialog. | verified | 2 |
| Update notice | Without `--offline`: "Update Available · New version 1.4.0 is available. Run pi update". `--offline` (or `PI_OFFLINE=1`) removes it. It does not block. | verified | 2 |
| Global resources | Skills load from `~/.agents/skills` (and other global paths) even with a temp `PI_CODING_AGENT_DIR`. `--no-skills` stops it. `--no-context-files` stops `AGENTS.md`/`CLAUDE.md`. | verified | 2 |
| Prompt ready | 0.16 to 0.35 s after spawn | verified | 2, 5 |
| Set session ID | `--session-id <uuid>` works. `session_start.session_id` is the same UUID. The file is created at the first message. | verified | 3 |
| Reuse a session ID | `--session-id <used uuid>` resumes the session (`reason` stays `startup`). No error. | verified | 3 |
| Resume | `--session-id <id>` or `--session <id>` in the same cwd | verified | 3 |
| Session ID from another cwd | `--session-id <id>` starts a **new** session with the same ID in the other project's directory. `--session <id>` asks "Session found in different project … Fork this session into current directory? [y/N]". | verified | 3 |
| Two starts with one ID | Both start. No lock. | verified | 3 |
| Clean exit | Ctrl-D on an empty editor or `/quit`: exits in 0.1 s. `session_shutdown.reason` = `quit`. Ctrl-C only clears the editor (two Ctrl-C 1 s apart did not exit). | verified | 1, 3 |
| Session file | `$PI_CODING_AGENT_DIR/sessions/--<cwd, "/" replaced by "-">--/<UTC time>_<id>.jsonl` (`PI_CODING_AGENT_SESSION_DIR` or `--session-dir` move it) | verified | 3 |
| Usage in the session file | Each assistant `message` has `provider`, `model`, `usage` with `input`, `output`, `cacheRead`, `cacheWrite`, `reasoning`, `totalTokens`, and `cost` (API-price dollars) | verified | 3 |
| `--system-prompt` | Replaces the whole default prompt. pi adds only "Current working directory: …". | verified | 4 |
| `--append-system-prompt` | Appends. The file form (a path) works. | verified | 4 |
| Prompt on resume | Not frozen. The prompt is built at each start, so a changed `--append-system-prompt` applied on the next start with the same `--session-id`. | verified | 4 |
| Submit a turn | Bracketed paste, then CR at once. Newlines stay. | verified | 5 |
| Bare LF | Adds a new line. It does not submit. | verified | 5 |
| Type while busy | Enter steers. `input` fires at once with `streaming_behavior` = `steer`. One `agent_settled` for both. | verified | 5 |
| Auth status | `pi auth check --provider <p> --json --no-refresh`: `{"status":"ready","provider":"…","authType":"oauth" or "api_key"}` exit 0, or `{"status":"not_ready",…,"reason":"credentials_not_configured"}` exit 1. Without `--json`: `ready`. | verified | 7 |
| Quota | None. The footer and `/session` show tokens and cost for the session only (`$0.000 (sub)` for a subscription). | verified | 8 |

### Events

Sequence for one turn with one bash call (spike 1, run 3, times from the
Enter byte):

| t (s) | Event | Useful fields |
|---|---|---|
| -1.04 | `session_start` | `reason` (`startup`), `session_file` |
| 0.10 | `input` | `prompt`, `source`, `streaming_behavior` |
| 0.10 | `before_agent_start` | `prompt`, system prompt length and head |
| 0.10 | `agent_start`, `turn_start` | `turn_index` |
| 3.19 | `message_end` | `usage`, `stop_reason` (`toolUse`) |
| 3.19 | `tool_execution_start` | `tool_name` (`bash`), `tool_input`, `tool_call_id` |
| 3.20 | `tool_execution_end` | `is_error` |
| 3.20 | `turn_end`, `turn_start` | |
| 4.08 | `message_end` | `last_assistant_message` (`done`), `stop_reason` (`stop`) |
| 4.08 | `turn_end`, `agent_end`, `agent_settled` | `idle: true` |
| 16.22 | `session_shutdown` | `reason` (`quit`, after Ctrl-D) |

With a provider error, pi retries by itself: `agent_start` … `agent_end`
ran four times (2, 6, 14 s back-off), each `message_end` with
`stop_reason` = `error`, then one `agent_settled`, and the screen showed
"Error: Retry failed after 3 attempts: Connection error." (spike 1, run 1
on the LAN provider; run 3 overwrote that capture, so the notes are in
`hooks-in-interactive/captured/run1-lan-retry-notes.txt`).

The extension keeps one socket connection, buffers up to 1000 lines while
it connects, and never blocks pi. `TERRARIUM_EVENT_LOG` adds a file copy.
`TERRARIUM_TRUST=yes|no` answers the project-trust question.
`TERRARIUM_FULL_PROMPT=1` adds the full system prompt to
`before_agent_start` (spikes only).

### Auth and token reuse

| Provider | Result | Status |
|---|---|---|
| `~/.pi/agent/auth.json` | `{}`: no OAuth or API-key entries | verified |
| LAN providers (`glm53-exl3`, `vllm`, `qwen38-helios`) | `auth check` says `ready` (`api_key` from `models.json`). `vllm` and `qwen38-helios` refused connections. `glm53-exl3` answered one `/v1/models` probe (it serves `DeepSeek-V4.1-Flash-1017`, not the configured `GLM-5.3-Flash-EXL3`) and then went down before the first turn. | verified |
| `anthropic` with `ANTHROPIC_OAUTH_TOKEN` = Claude Code's access token | `auth check` says `ready` (`api_key`). The API **refused** the call: `400 … "You're out of extra usage."` pi's docs say third-party harness use of Claude Pro/Max draws from extra usage, not the plan limits. | verified (1 request) |
| `openai-codex` with a copy of `~/.codex/auth.json` | Works. The `auth.json` entry is `{"type": "oauth", "access": <access_token>, "refresh": "", "expires": <JWT exp ms>, "accountId": <chatgpt_account_id>}`. The refresh token is left empty, so pi can never rotate the host's Codex refresh token; the entry fails when the access token expires (73 h left at the test). | verified |
| `github-copilot`, `google`, `xai`, `openrouter` | `not_ready`, `credentials_not_configured` | verified |

`auth check` reports only that a credential is present. It said `ready`
for a LAN server that was down and for the Anthropic token that the API
refused. `pi auth print-bearer-token` was not run (it prints the token).

---

## pi: facts that contradict the plan

1. **§4 and §16 "pi is the escape hatch, one harness for many
   subscriptions".** On this host pi has no OAuth login. A Claude Pro/Max
   OAuth token through pi is billed as extra usage, not the plan: pi cannot
   spend the Claude Max subscription. The Codex (ChatGPT Pro) plan works
   through pi. A copied credential with a shared refresh token can rotate
   the host's token; the safe copy has no refresh token and expires.
2. **§4 "`--session <id>` or `--session-id <id>`".** `--session-id` creates
   or resumes and never fails. It is scoped to the cwd: the same ID in
   another cwd is a different session. `--session <id>` from another cwd
   blocks on a `[y/N]` fork question.
3. **§7 "turn ended".** Use `agent_settled`. `agent_end` fires once per
   attempt, also for each automatic retry after an error.
4. **§8 quiet start.** `--approve` is project trust, not tool approval. pi
   never asks before a tool. Global skills from `~/.agents/skills` load
   from outside `PI_CODING_AGENT_DIR`; pass `--no-skills` or give the
   sandbox its own `$HOME`.
5. **§16.2 `pi auth print-bearer-token` for probes.** Not checked. `pi auth
   check` cannot tell a usable credential from a refused one.
6. **§7 exit.** Ctrl-C does not exit pi. Use Ctrl-D on an empty editor or
   `/quit`.

## pi: surprises

- `--system-prompt` also drops pi's tool guidance. Only the cwd line is
  added.
- The session file name has the UTC start time and the ID; the extension
  can report the path before the file exists.

## pi: not tested

- `ui_prompt_start` with a real extension prompt.
- Compaction events.
- A Copilot subscription through pi (`github-copilot` provider).
- `pi auth print-bearer-token` and the provider usage endpoints.

---

## pi 1.4.0

Tested on 2026-10-25 and 2026-10-26 in the Docker stack (image with 1.4.0).
0 model turns. The daemon check started `lab/pi-check` with no subscription
on the old adapter. The adapter changes below were then checked by hand in
the container: the exact argv of `launch::build`, run in a pty with a temp
`PI_CODING_AGENT_DIR` (a copy of `/auth/pi` without `auth.json`). The
daemon path with the changed adapter is not proven until the next image
build.

| Topic | Fact | Status |
|---|---|---|
| `--provider` | Now "Provider to search for --model (requires --model)". `--provider` alone is fatal: `Error: --provider requires --model (for example: --provider openai-codex --model <pattern>)`, and pi does not start. 0.85.1 started with the same argv. | verified |
| Model without `--model` | pi takes `defaultProvider`/`defaultModel` from `settings.json` when that provider has a login, else the first logged-in provider in its own order (`anthropic` before `openai-codex`), from `defaultModelPerProvider` (`core/model-resolver.js`). Both can spend a different subscription than the agent's. | read (source) |
| Adapter change | A subscription with no agent `model` gets `--provider <p> --model <cheap default>` (`flags::DEFAULT_MODELS`: `openai-codex`/`openai`/`github-copilot` → `gpt-6-luna`, `anthropic` → `claude-haiku-5-5`, `google` → `gemini-3.1-flash-lite`, `openrouter` → `openai/gpt-6-luna`, `xai` → `grok-4.3`, `groq` → `openai/gpt-oss-20b`, `deepseek` → `deepseek-flash`, `mistral` → `mistral-small-latest`). The agent's `model` overrides it. A provider with no row (a custom provider in `models.json`) and no `model`: the daemon refuses the start (`start_refusal`). `--provider` never goes alone. | verified (start only: the footer shows `gpt-6-luna`, the extension reports `openai-codex/gpt-6-luna`) |
| `--tui-mode` | The default changed from `regular` to `fullscreen` (alternate screen and mouse tracking in `pty.log`). The adapter passes `--tui-mode regular`: no `?1049h`, no `?1000h`/`?1003h`/`?1006h`, one `?2004h`. The detector says `PromptReady` on that screen. Idle detection also worked in fullscreen; busy and dialog screens in fullscreen are not checked. | verified |
| `--no-mcp` | New: built-in MCP. Without the flag, pi reads `mcp.json` in `PI_CODING_AGENT_DIR` (the shared `/auth/pi`). The adapter passes `--no-mcp`. | read (help); started with it |
| `--no-extensions` | Now also turns off pi's built-in extensions. `-e` still loads an explicit path (the screen lists only `terrarium.ts`) and also takes `builtin:<name>`. | verified |
| Other flags | `--model`, `--offline`, `PI_OFFLINE`, `PI_TELEMETRY`, `--no-skills`, `--no-prompt-templates`, `--no-context-files`, `--no-approve`, `--session-dir`, `--session-id`, `--append-system-prompt`, `--system-prompt`, `PI_CODING_AGENT_DIR`: help text unchanged. | read |
| Changelog screen | At the first 1.4.0 start, pi shows the "What's new" entries above the editor, because the shared `settings.json` has `lastChangelogVersion` `0.85.1`. The adapter now sets `lastChangelogVersion` to the pinned version in the agent's `settings.json` copy at every start. With it, no changelog shows. | verified |
| Extension events | All 17 events that `terrarium.ts` subscribes to are in `types.d.ts`. Additive changes only: `agent_settled.aborted`, `turn_end.messageEntryId`/`toolResultEntryIds`, `tool_execution_*.parentToolCallId`, `tool_execution_end.durationMs`. `before_agent_start` is now a union (the custom variant has `message`, no `prompt`). `on()` returns an unsubscribe function. `getSessionId`/`getSessionFile`, `isIdle`: no change. | read (types) |
| Start and stop | starting → idle 0.41 s "prompt ready" (daemon, old adapter). By hand with the new argv: `session_start` with the `--session-id` UUID and a session file in `--session-dir`; `/quit` → `session_shutdown` → exit 0 in 0.06 s. | verified |
| Session file | No file before the first message, as on 0.85.1. The start prints "Warning: No project session found with id '<id>'; creating a new session with that id." above the banner. | verified |
| `pi auth check` | Same format: `{"status":"not_ready","provider":"openai-codex","reason":"credentials_not_configured"}` exit 1. New flag `--credentials` prints the secret: Terrarium must never pass it. | verified |

### pi 1.4.0: not tested

- A turn with the new argv (busy, tool, and `agent_settled` screens in
  `regular` mode on 1.4.0).
- The default models of `DEFAULT_MODELS` in a real turn. The ids come from
  the 1.4.0 model catalog (`pi-ai/dist/providers/*.models.js`).
- `google-gemini-cli` (in `provider.rs`) is not a provider in the 1.4.0
  catalog.

---

## Harness hardening (all harnesses)

The source is `docs/research/harness-hardening.md` (2026-10-28: static
analysis of the pinned versions, no model turn). Each setting below is
**read**, not verified with a live run, unless it says so. The goal: no
feature waits for a human, no plan mode, and no extra tool or
integration, but the Codex image tool stays.

Two classes:

- **Forced**: Terrarium sets it at each launch (a flag, an env var, or a
  settings key after the user file). A user file cannot change it. It is
  forced when an unattended agent needs it: an ask-user tool, a plan mode,
  a dialog that waits for a key, a trust prompt, or data that leaves the
  agent (another client can steer it, a session export).
- **Default**: the packaged file in `defaults/harness/<harness>/`, which
  the config repo seeds (PLAN §8.3). The operator can change it.

There is no option that turns an ask-user tool or a plan mode on again.
An operator who needs one must change the adapter (`flags.rs`,
`config.rs`) and its tests.

| Harness | Forced | Default (tweakable) |
|---|---|---|
| Claude Code | `permissions.deny` always holds `AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode`, `ProposeGoal`, `EnterWorktree`, `ExitWorktree` (added to the user's deny list); `modelProposedGoals: "disabled"`; `disableAutoMode: "disable"` (top level and in `permissions`); `askUserQuestionTimeout: "60s"`; `crossSessionInbound: "refuse"`; `autoContinueAtUsageLimit`; `disableClaudeAiConnectors`, `disableRemoteControl`, `remoteControlAtStartup: false`, `autoUploadSessions: false`; `skipDangerousModePermissionPrompt`; `hooks`; skills ("Skills" below): `disableBundledSkills: true`, `enabledPlugins` removed, `Skill` in `permissions.deny` for an agent with no skill. Flags `--no-chrome`, `--strict-mcp-config`, and `--plugin-dir` for an agent with skills. Env `DISABLE_UPDATES`, `DISABLE_LOGIN_COMMAND`, `DISABLE_FEEDBACK_COMMAND`, `CLAUDE_CODE_SEND_FEEDBACK=0`, `CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY`, `ENABLE_CLAUDEAI_MCP_SERVERS=false`, `CLAUDE_CODE_DISABLE_CRON`, `CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL`, `DISABLE_INSTALL_GITHUB_APP_COMMAND`, `DISABLE_UPGRADE_COMMAND`, `DISABLE_EXTRA_USAGE_COMMAND`, `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`, `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=false`, `CLAUDE_CODE_DISABLE_POLICY_SKILLS=1`. `.claude.json`: `autoInstallIdeExtension`, `autoConnectIde`, `lspRecommendationDisabled`, `claudeInChromeDefaultEnabled`, `remoteControlAtStartup`, `autoUploadSessions`. **Never** `CLAUDE_CODE_DISABLE_DANGEROUS_RM_TIMEOUT`: the `rm` prompt would wait forever. | The deny list of the other extra tools (`Workflow`, `Artifact`, `ReportFindings`, `SendFeedback`, `PushNotification`, `RemoteTrigger`, `Cron*`, `ScheduleWakeup`, `ClaudeDesign`, `Projects`, `ReadNotifications`, `ShareOnboardingGuide`, `SendUserFile`, `OfferChromeSetup`, `ShowOnboardingRolePicker`); `autoMemoryEnabled`, `autoDreamEnabled`, skill and plugin sync, `enableArtifact`, workflows, agent view, `isolatePeerMachines`, surveys and feedback drafts, prompt suggestions, away summary, notifications, voice, `switchModelsOnFlag: true`, spinner tips, `fileCheckpointingEnabled: false`, `includeGitInstructions`, `fastMode`. |
| Codex | `tools.experimental_request_user_input.enabled = false` (removes `request_user_input` and `request_user_input_async`); `features.default_mode_request_user_input = false`; `include_collaboration_mode_instructions = false`; `tools.update_plan.enabled = false`; `features.goals = false`; `notice.model_migrations` (one inline table, one entry per catalog `upgrade` row); `notice.hide_rate_limit_model_nudge`, `hide_full_access_warning`, `hide_gpt5_1_migration_prompt`; `tui.show_tooltips = false`; `features.apps`, `plugins`, `remote_plugin`, `plugin_sharing`, `tool_suggest`, `skill_mcp_dependency_install`, `memories`, `daemon_auto_start` = false; `features.hooks = true`; `skills.bundled.enabled = false`, `skills.include_instructions` (true only for an agent with skills), and the agent's skills dir read-only over `$CODEX_HOME/skills`; with the yolo, trust, hook, update, analytics, feedback, SQLite, and prompt keys of before. Empty `AGENTS.md` and `AGENTS.override.md` over `$CODEX_HOME`. `$CODEX_HOME/generated_images/` is the agent's `<workspace>/generated_images/` (D4). | `features.image_generation = true` (the `image_gen.imagegen` tool stays; it needs a ChatGPT plan login), `web_search = "disabled"`, `agents.enabled = false`, `features.sleep_tool = false`, `features.skill_search = false`, `tui.notifications = false`, `otel.metrics_exporter = "none"`. |
| Copilot CLI | Flags `--no-ask-user`, `--mode interactive`, `--disable-builtin-mcps` (no `github-mcp-server`, `githubiq`), `--sync-session=off`, with the yolo, update, and telemetry flags of before. Settings `askUser: false`, `defaultMode: "interactive"`, `stayInAutopilot: false`, `defaultPermissionMode: "allow-all"`, `permissions.disableAssistedPermissionsMode`, `shellShortcut: false`, `ide.autoConnect` and `ide.openDiffOnEdit` false, `autoUpdate: false`, `remoteExport: false`, `remoteSessions: false`, `memory: false`, `connectors: false`, `experimental: true`; skills: `skillDirectories`, `ignoredSkillsLocations`, and the built-in skills in `disabledSkills`, and `--excluded-tools=skill` for an agent with no skill. Env `COPILOT_DISABLE_DESKTOP_NOTIFICATIONS=1`. An empty `copilot-instructions.md` over the shared one. | `banner: "never"`, tips, notifications, beep, terminal title and progress, voice, `continueOnAutoMode: false`, `imageGeneration: false`, the built-in agents and sub-agents, `customAgents.defaultLocalOnly`, `contextManagementTools: false`, `enabledMcpServers`, `enabledPlugins`. |
| pi | `lastChangelogVersion`; `doubleEscapeAction: "none"`, `branchSummary.skipPrompt`, `defaultProjectTrust: "never"`, `tuiMode: "regular"`; the flags of before (`--offline`, `--no-extensions`, `--no-mcp`, `--no-skills`, `--no-prompt-templates`, `--no-context-files`, `--no-approve`), and one `--skill` for each skill of the agent. B1: empty `SYSTEM.md` and `APPEND_SYSTEM.md` over the shared ones, and the other prompt flag with an empty value. | `enableInstallTelemetry`, `enableAnalytics`, `enableSkillCommands` false, `warnings.anthropicExtraUsage: false`, `cacheWarming: "off"`. |

Not done, because the syntax is not known: the Copilot tool allowlist
(`--available-tools`). Open question 1 of the Copilot section of the
research. `--excluded-tools=skill` is used for the skill tool only (in
`--help` of 1.0.103; a start with it is not tested).

**Project instructions (D1).** Terrarium uses `AGENTS.md` only. The
protocol template tells each agent to read `AGENTS.md` in its workspace
and in each repo. Nothing loads `CLAUDE.md`. Per harness:

| Harness | What loads by itself |
|---|---|
| Claude Code | Nothing: `--setting-sources ""` turns off the project source (read in the bundle). Claude Code does not know `AGENTS.md`. |
| Codex | The `AGENTS.md` of the workspace root, as a user message (also in replace mode). That is the same file that the prompt names, so Terrarium lets it load. The global files of the shared home are empty for agents. A repo's `AGENTS.md` is in a subdirectory of the cwd, so Codex does not load it: the agent reads it. |
| Copilot CLI | Very likely nothing in replace mode (the custom instructions are a section of the replaced system message; not tested). The global `copilot-instructions.md` is empty for agents. |
| pi | Nothing: `--no-context-files` (it would also load the shared home's `AGENTS.md`). |

**Turns that start with `/` or `!` (D3).** Each adapter puts U+00A0 (no-break
space) in front of a turn whose text starts with `/` or `!` after
white space; Copilot also for a lone `$`. Verified live on 2026-10-28
with `/help reply with ok`, sent with Enter: with U+00A0, each of the four
harnesses ran a normal turn. U+200B (zero width space) and a plain space
did not work in Claude Code: it ran `/help`, and a `/plan` turn put the
agent in plan mode. The model reads the same words. The delivery log
ignores the character when it matches a transcript block. A leader runs a command on purpose with
a raw send (`terrarium agent send --raw`) or keys (`terrarium agent keys`).

**Dialogs (B2).** The screen detectors also know these dialogs from the
research. Each string comes from the bundle or the source; no screen was
captured.

| Harness | Dialog | Marker |
|---|---|---|
| Claude Code | usage-limit menu, spend limit, `.mcp.json` approval, Consumer Terms | `Stop and wait for limit to reset`, `Could not update your spend limit`, `Press Enter to retry`, `New MCP server found in this project`, `An update to our Consumer Terms` |
| Codex | model migration at start, rate-limit popup, Plan-mode question | `enter/esc continue`, `enter/esc confirm`, `· ⌃c quit`, `press enter to confirm`, `Press enter to confirm or esc to go back`, `enter to submit answer` and `tab to add notes` anywhere in a line |
| Copilot CLI | tool approval, auto-mode switch, folder trust, form | `approve once`, `approve for this session`, `needs approval`, `confirm folder trust`, `ctrl+d decline` |
| pi | model, session, and tree selectors | `tab scope`, `enter save`, `Delete session?` |

**A question in plain text (D5).** In all four harnesses, a question in
plain text ends the turn, and the runner sets `idle`. The question guard
of the daemon (PLAN §8.4) sets `blocked` and mails the lead.

## Skills (all harnesses)

Terrarium-managed skills (PLAN §8.5): each start copies the agent's
skills into `<harness dir>/skills/` (read-only in the sandbox), and each
adapter points its harness there. Read on 2026-10-28 in the pinned
versions (the Claude Code bundle, the Codex source at `rust-v0.164.0`,
the Copilot 1.0.103 runtime, the pi 1.4.0 package). **V** is read in the
code; **I** is inferred. No model turn ran: the live check is open.

| Harness | Mechanism | Shared-home and other skill dirs | The skill tool | Source |
|---|---|---|---|---|
| Claude Code 2.1.310 | `--plugin-dir <harness dir>/claude/plugin`: `.claude-plugin/plugin.json` `{"name":"terrarium"}` and a copy of the skills in `skills/<name>/`. An inline plugin is on unless `enabledPlugins` turns it off, so `--setting-sources ""` does not block it (V). The model sees `terrarium:<name>` (V). The dir is only read (I). | `--setting-sources ""` blocks `$CLAUDE_CONFIG_DIR/skills` (user source) and the workspace `.claude/skills` (project source) (V). `CLAUDE_CODE_DISABLE_POLICY_SKILLS=1` stops the managed-policy skills (V key). `disableBundledSkills: true` hides only the bundled skills (V). `enabledPlugins` is removed from the user file. | `Skill`. A bare deny in `permissions.deny` removes the tool and the skill list (V). | `--help`; bundle markers `lt.enabled=Aqt(lt.source,...)`, `ft=`${n}:${Ee.name...}``, `Qt("userSettings")&&!w?SL(s,"userSettings","skills",n)`, `e.source==="builtin"`, `if(!e.options.tools.some((we)=>Rt(we,Ao)))return[]` |
| Codex 0.164.0 | The agent's skills dir, bound read-only over `$CODEX_HOME/skills`, also when empty: the User root (V; the code calls it deprecated, kept for compatibility). `-c skills.include_instructions=true` only for an agent with skills; `false` empties the catalog, and a `$name` mention finds nothing (V). A read-only root does not stop the start: a walk error is a warning (V). | The bind hides the shared `$CODEX_HOME/skills`. `skills.bundled.enabled = false`: no `.system` install and no System root (V). `$HOME/.agents/skills` is on the empty tmpfs `$HOME`. **Open:** `<workspace>/.agents/skills` and a project `.codex/skills` still load; no key turns a repo root off (V). | No tool: the "## Skills" list names each `SKILL.md`, and the model reads it (V). | `ext/skills/src/host_roots.rs:48-119,137-185`, `host_service.rs:140,234-236,412-416`, `loader/discovery.rs:87-95`, `preparation.rs:118-119`, `extension.rs:199-200,370`, `config/src/skills_config.rs:19-63` |
| Copilot CLI 1.0.103 | `skillDirectories` = [the skills dir] in the agent's `settings.json` (V: an absolute root of `<name>/SKILL.md`, I: the layout). In replace mode (`SYSTEM.md`), Copilot's `<available_skills>` list is gone with the system message (V), so the adapter writes the list into `SYSTEM.md`. | `ignoredSkillsLocations` (paths, and the dirs under them; V) = `$COPILOT_HOME/skills`, `~/.agents/skills`, `~/.claude/skills`, and the workspace's `.github/skills`, `.claude/skills`, `.agents/skills` (the paths of the project roots are I). `disabledSkills` holds the built-ins `customize-cloud-agent`, `github-pr-media`, `discover-resources` (V). `COPILOT_SKILLS_DIRS` is not in the clean env. | `skill`. Its description lists no skill (V). An agent with no skill gets `--excluded-tools=skill` (`--help`; a start with it is not tested). | the runtime `cli-main.js` (`skillDirectories` catalog entry, `canonicalIgnoredSkillLocations`), `runtime.node` strings, `builtin-skills/` |
| pi 1.4.0 | `--skill <harness dir>/skills/<name>` for each skill. It loads also with `--no-skills`, which stops only the discovery (V). The `<skills>` prompt section lists each `SKILL.md` path, also with `--system-prompt` (V). | `--no-skills` stops the pi home's `skills/`, `~/.agents/skills`, the project `.pi/skills` and `.agents/skills`, and the settings paths (V). | No tool: the model reads the `SKILL.md` with `read` (V). | `dist/cli/args.js:166`, `dist/core/resource-loader.js:424`, `dist/core/system-prompt.js:102-108`, `dist/core/skills.js` |

**The live check (not run).** A skill `fern-check` ("When asked for the
deployment checklist code, reply with FERN-42"), given to one agent per
harness and not to a second agent. The first must answer FERN-42 through
the skill; the second must not know the code. Until it passes, each
mechanism above is read, not verified.

## Usage probes and login flows

Tested on 2026-10-24 on the same host, against the operator's real logins:
Claude Max (OAuth), ChatGPT Pro (Codex OAuth), GitHub Copilot Individual
(through a GitHub CLI OAuth token). Each probe is one read-only `GET`. No
token was printed or stored, and no login or logout ran against a real
home. The code is in `crates/terrariumd/src/probes/`; the sanitised answers
are in `crates/terrariumd/tests/fixtures/probes/`. Run them again with
`cargo test -p terrariumd --test subscriptions -- --ignored live_probes`
(set `GH_TOKEN` for Copilot). All three endpoints are undocumented. Treat
each as best-effort (PLAN §21).

### Summary

| Probe | Request | Status |
|---|---|---|
| `anthropic-oauth-usage` | `GET https://api.anthropic.com/api/oauth/usage`, `Authorization: Bearer <claudeAiOauth.accessToken>`, `anthropic-beta: oauth-2025-04-20` | verified (200) |
| `openai-codex-usage` | `GET https://chatgpt.com/backend-api/wham/usage`, `Authorization: Bearer <tokens.access_token>`, `ChatGPT-Account-Id: <tokens.account_id>` (both from `$CODEX_HOME/auth.json`) | verified (200) |
| `github-copilot-quota` | `GET https://api.github.com/copilot_internal/user`, `Authorization: token <GitHub OAuth token or PAT>` | verified (200) with a `gh` OAuth token |
| User-Agent | All three answered with `User-Agent: terrarium/0.1.0`. No harness UA is needed. | verified |
| Token refresh | The probes never refresh. An expired Claude token (`expiresAt`, ms) fails before the request. | by design |

### Anthropic

`$CLAUDE_CONFIG_DIR/.credentials.json`: `claudeAiOauth.{accessToken,
refreshToken, expiresAt, refreshTokenExpiresAt, scopes, subscriptionType,
subscriptionTypeSource, rateLimitTier}`.

Answer (sanitised; many other keys are `null`):

```json
{"five_hour": {"utilization": 16.0, "resets_at": "2026-10-24T10:59:59.514909+00:00",
               "limit_dollars": null, "used_dollars": null, "remaining_dollars": null, "locked_reason": null},
 "seven_day": {"utilization": 10.0, "resets_at": "2026-10-31T01:59:59.514930+00:00", ...},
 "seven_day_opus": null, "seven_day_sonnet": null, "seven_day_oauth_apps": null,
 "extra_usage": {"is_enabled": false, ...},
 "limits": [{"kind": "session", "group": "session", "percent": 16, "resets_at": "...", "is_active": true},
            {"kind": "weekly_all", "group": "weekly", "percent": 10, ...},
            {"kind": "weekly_scoped", "percent": 3, "scope": {"model": {"display_name": "<model>"}}, ...}],
 "spend": {...}, "seven_day_breakdown": {"rows": [{"key": "claude_code", "percent": 100}, ...]}}
```

- `utilization` is a percent (0 to 100). The probe reads `five_hour` (`5h`),
  `seven_day` (`7d`), and, when not `null`, `seven_day_opus`,
  `seven_day_sonnet`, `seven_day_oauth_apps`.
- The answer has code-named objects with dollar limits (for example a
  promotional credit). The probe ignores them.
- `limits[]` repeats the windows as integer percents, and adds per-model
  weekly limits. The probe does not read it yet.

### OpenAI Codex

`$CODEX_HOME/auth.json`: `{auth_mode, OPENAI_API_KEY, tokens: {id_token,
access_token, refresh_token, account_id}, last_refresh}`.

Answer (sanitised):

```json
{"user_id": "<id>", "account_id": "<id>", "email": "<email>", "plan_type": "pro",
 "rate_limit": {"allowed": true, "limit_reached": false,
   "primary_window": {"used_percent": 0, "limit_window_seconds": 604800,
                      "reset_after_seconds": 570148, "reset_at": 1793396550},
   "secondary_window": null},
 "code_review_rate_limit": null, "additional_rate_limits": null,
 "credits": {"has_credits": false, "unlimited": false, "balance": "0", ...},
 "rate_limit_reached_type": null}
```

- On this Pro plan only the primary window is present, and it is 7 days
  (`604800` s). Plans with a 5-hour window put it in `primary_window` and
  the weekly one in `secondary_window`; that shape is a fixture built from
  this schema (`openai_wham_usage_two_windows.json`), not a capture.
- The probe names a window by its length: `5h`, `7d`. `reset_at` is Unix
  seconds.
- The plan is also in the `id_token` claim
  `https://api.openai.com/auth`.`chatgpt_plan_type`. The login wizard reads
  only that claim.

### GitHub Copilot

Answer (sanitised):

```json
{"login": "<user>", "access_type_sku": "yearly_subscriber_quota", "copilot_plan": "individual",
 "quota_reset_date": "2026-11-01", "quota_reset_date_utc": "2026-11-01T00:00:00.000Z",
 "quota_snapshots": {
   "chat": {"unlimited": true, "percent_remaining": 100.0, "entitlement": 0, "remaining": 0, ...},
   "completions": {"unlimited": true, ...},
   "premium_interactions": {"unlimited": false, "entitlement": 300, "remaining": 232,
     "quota_remaining": 232.1, "percent_remaining": 77.3, "credits_used": 67,
     "overage_count": 0, "overage_permitted": false, "timestamp_utc": "...", ...}}}
```

- The probe reports `premium_interactions` as `monthly` (`used_pct = 100 -
  percent_remaining`), and `chat` and `completions` only when they are not
  unlimited. The reset is `quota_reset_date_utc`.
- **The token.** Copilot keeps its login in the system credential store.
  On this host `~/.copilot/config.json` holds no token (it is JSON with
  `//` comment lines at the top: strip them before parsing). In a
  container without a credential store, Copilot writes the token to
  `config.json` as `authTokens.<host>:<login>[:github].token` (verified
  2026-10-25, see "Plain-text login"). The probe reads that, then the
  older guesses `copilot_tokens`, `github_token`, `oauth_token`, `token`. The probe
  also takes a stored PAT (`POST /api/subscriptions/{name}/api-key` with
  `provider = "github"`) and `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`,
  `GITHUB_TOKEN` from the daemon env.

### Status commands

| Harness | Command | Output | Status |
|---|---|---|---|
| Claude | `claude auth status` | JSON (see "Auth status"). Exit 0 logged in. | verified |
| Codex | `codex login status` | stderr `Logged in using ChatGPT`, exit 0; stdout is empty. The daemon reads both streams (the adapter's `parse_auth_status`: exit 0 and a line that starts with `Logged in`). Logged out: `Not logged in`, exit 1 (spike). | verified (logged in, 2026-10-25) |
| Copilot | none | There is no status command. The wizard reads the plain-text fallback in `config.json` with the adapter's `auth_status`: a non-empty `authTokens.<key>.token` is a login. A login that names a user but has no token (keychain) is not. | verified (2026-10-25) |
| pi | `pi auth check --json --no-refresh --provider <p>` | `{"status":"not_ready","provider":"anthropic","reason":"credentials_not_configured"}` with no login. The ready value is not seen; the daemon accepts `ready`, `ok`, `valid`. | partly verified |

- `pi auth check` without `--provider` or `--model` prints `Error: Auth
  checks require --provider <provider> or --model <model>`.
- `pi auth check` **refreshes expired OAuth credentials by default**. The
  daemon always passes `--no-refresh`, so a check cannot rotate a token.

### Login flows

Each command ran in a PTY (120 x 40) with an **empty temp home**
(`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `COPILOT_HOME`, `PI_CODING_AGENT_DIR`),
and got Ctrl-C once the URL or code was on screen. No login finished. The
raw output, with the PKCE values and the codes replaced, is in
`crates/terrariumd/tests/fixtures/login/`.

| Kind | Output | Pattern |
|---|---|---|
| `claude auth login` | `Opening browser to sign in…`, then `If the browser didn't open, visit: <url>` (the URL twice: an OSC 8 link target and the visible text), then `Paste code here if prompted > `. URL: `https://claude.com/cai/oauth/authorize?code=true&client_id=...&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=...&code_challenge=...&code_challenge_method=S256&state=...` | `https://[^\s]*/oauth/authorize\?[^\s]+`, the OSC 8 target first |
| `codex login --device-auth` | `1. Open this link ... https://auth.openai.com/codex/device`, `2. Enter this one-time code (expires in 15 minutes)`, then the code on its own line, for example `ABCD-EF12G` (4, dash, 5). It warns about PATH aliases under `/tmp`. | URL `https://auth\.openai\.com/codex/device\S*`; code `one-time code.*?\b([A-Z0-9]{4}-[A-Z0-9]{4,6})\b` |
| `copilot login --device-code` | `To authenticate, visit https://github.com/login/device and enter code WXYZ-1234`, `Waiting for authorization...`. It also writes the code to the clipboard with OSC 52. | `visit\s+(https://\S+/login/device)\s+and enter code\s+([A-Z0-9]{4}-[A-Z0-9]{4})` |
| `pi`, `/login` | A menu (`Sign in with an account` / `with an API key`), then a provider list (`Anthropic`, `GitHub Copilot`, `Kimi For Coding`, `OpenAI Codex`, `OpenRouter`, `Radius`, `xAI`). Anthropic shows `https://claude.ai/oauth/authorize?...redirect_uri=http%3A%2F%2Flocalhost%3A53692%2Fcallback...` wrapped over four screen rows, and `paste the authorization code / redirect URL here`. | the OSC 8 link target (the screen text is wrapped) |

- The Claude and pi URLs carry a fresh PKCE challenge and `state` on each
  run. A device code expires in 15 minutes (Codex).
- pi's Anthropic login uses a localhost callback. In a container the
  operator pastes the final redirect URL into the prompt, as pi says.
- A login PTY must not inherit `ANTHROPIC_*`, `CLAUDE_*`, `GH_TOKEN`,
  `GITHUB_TOKEN`, `COPILOT_GITHUB_TOKEN`, or `OPENAI_API_KEY`: each makes the
  harness use another credential. The daemon removes them.

### Not tested

- A login that completes (by design: no real login ran).
- Claude `claude setup-token`.
- An expired or refused token on each endpoint (the probes map a non-2xx
  answer to `failed`).
- Rate limits of the usage endpoints at the 5-minute interval.
- `pi auth print-bearer-token` as the credential for a probe.

### Z.ai usage

**Undocumented, verified live 2026-10-26** with the stored `zai-key` key
(GLM Coding Plan, level `max`). One read-only `GET`; it answered `200` on
the first request. The key was not printed or stored. The code is
`crates/terrariumd/src/probes/zai.rs`; the sanitised answer is
`crates/terrariumd/tests/fixtures/probes/zai_quota_limit.json`.

| Fact | Value | Source |
|---|---|---|
| Request | `GET https://api.z.ai/api/monitor/usage/quota/limit` | verified |
| Auth | `Authorization: Bearer <Z.ai API key>` (the key from `POST /api/subscriptions/{name}/api-key`). One plugin sends the bare key with no `Bearer`; Terrarium did not try that form. | verified; https://github.com/guyinwonder168/opencode-glm-quota |
| Host | `api.z.ai`. Tools for mainland China use `open.bigmodel.cn`; not tried. | https://github.com/guyinwonder168/opencode-glm-quota |
| Shape | `{code: 200, msg, success: true, data: {level, limits: [...]}}`. Each limit: `type`, `unit`, `number`, `percentage`; when the period has started, also `usage` (the limit), `currentValue` (used), `remaining`, `nextResetTime` (epoch ms). | verified |
| Period | `number` x `unit`: unit 3 = hours, 5 = months, 6 = weeks. | verified (3, 5); https://github.com/getpaseo/paseo/issues/5057 (6), 2026-09-18 |
| Windows on a Max plan | `TOKENS_LIMIT` unit 3 number 5: the 5-hour prompt window. It had only `percentage` (0) and no reset time, because no prompt ran in the window. `TIME_LIMIT` unit 5 number 1: the monthly MCP tool budget (4,000; `usageDetails` per tool: `search-prime`, `web-reader`, `zread`). No weekly window came back. | verified |
| Other plans | Pro answers `CREDIT_LIMIT` entries (5-hour and weekly) in place of `TOKENS_LIMIT`. | https://github.com/getpaseo/paseo/issues/5057, 2026-09-18 |
| Errors | An auth error can come back as `success: false` with a `code` and `msg`. | from tools, not verified |

The probe maps each `TOKENS_LIMIT` or `CREDIT_LIMIT` entry to a window:
`5h`, `7d`, `14d`, `monthly`, `<n>mo`, or `unit<u>x<n>` for an unknown
unit. `used_pct` is `percentage`, and `resets_at` is `nextResetTime`. `raw`
keeps the entry and `level`. A `TIME_LIMIT` entry is not a window: a used
up tool budget does not stop chat, and the quota policy would hold the
agents. The probe keeps it in the `raw` of the first window, under
`other_limits`. An answer with `success: false` fails the probe with its
`code` and `msg`; `code` 401 counts as an HTTP 401 (the login shows as
expired). A failed probe sets `probe_status = failed`; local accounting
continues.

A Z.ai subscription with plan `coding` (or no plan) and no `probe` gets
`zai-usage` when the row is read, so existing rows probe with no
migration. The `api` plan (pay as you go) gets no probe.

### Custom system prompt (all harnesses)

**Verified live 2026-10-27.** One agent per harness had the `system_prompt`
"begin every reply with the single word PINEAPPLE", plus a control agent
with none. Each got one turn: "which terrarium CLI command lists your
mail?".

| Harness, model | Launch | Reply |
|---|---|---|
| Claude Code, claude-haiku-5-5 | `--system-prompt` | `PINEAPPLE. Run terrarium mail check ...` |
| Codex, gpt-5.6-luna | `model_instructions_file` | `PINEAPPLE terrarium mail check ...` |
| Copilot, claude-haiku-4.5 | `SYSTEM.md` (5,499 bytes), `APPEND_SYSTEM.md` empty | `PINEAPPLE terrarium mail check ...` |
| pi, glm-5.3-flash (Z.ai) | `--system-prompt` | `PINEAPPLE terrarium mail check.` |
| Claude Code, no `system_prompt` | `--append-system-prompt` | `terrarium mail check lists your mail.` |

Each replaced prompt kept the Terrarium protocol prompt after the custom
text: every agent named the mail command. `{{agent.name}}` was filled in.

### OpenRouter credits

**Verified live 2026-10-26** with the stored `openrouter-key` key (a normal
key, not a management key). Read-only `GET`s; the key was not printed. The
code is `crates/terrariumd/src/probes/openrouter.rs`.

| Fact | Value | Source |
|---|---|---|
| Request | `GET https://openrouter.ai/api/v1/credits` | verified |
| Auth | `Authorization: Bearer <OpenRouter key>`. A normal key may read it. | verified |
| Shape | `{data: {total_credits, total_usage}}`, US dollars, for the account. | verified |
| The key | `GET https://openrouter.ai/api/v1/key` answers the spend of the key: `usage`, `usage_daily`, `usage_weekly`, `usage_monthly`, `limit`, `limit_remaining`, `is_free_tier`, and BYOK totals. The probe does not read it. | verified |

The probe `openrouter-credits` makes one window, `credits`: `used_pct` is
`total_usage / total_credits` (100 when no credits and some usage; 0 when
neither), with no reset time. `raw` has `total_credits`, `total_usage`,
`remaining`, and `currency`. An `openrouter` subscription with no `probe`
gets it when the row is read.

### Z.ai (GLM): from docs, not verified live

Read on 2026-10-26. No Z.ai key was available, so every fact below is
**from docs, not verified live**, unless the line says "verified offline".
The code is `crates/terrarium-core/src/zai.rs`; the API is docs/api.md
§16.5.

| Fact | Value | Source |
|---|---|---|
| Plans | GLM Coding Plan: Lite, Pro, Max. Each has a 5-hour and a weekly credit limit (Lite 2,000 / 10,000; Pro 12,000 / 60,000; Max 28,000 / 140,000). Usage shows only on a web page (`z.ai/manage-apikey/billing`). | https://docs.z.ai/devpack/overview |
| Models of the plan | GLM-5.3 and GLM-5.3-Flash. GLM-5.2 and GLM-5.1 requests go to GLM-5.3; GLM-4.7 requests go to GLM-5.3-Flash. | https://docs.z.ai/devpack/overview |
| Claude Code base URL | `ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic` | https://docs.z.ai/devpack/tool/claude |
| Claude Code key | `ANTHROPIC_AUTH_TOKEN=<Z.ai API key>` (not `ANTHROPIC_API_KEY`) | https://docs.z.ai/devpack/tool/claude |
| Claude Code timeout | `API_TIMEOUT_MS=3000000` | https://docs.z.ai/devpack/tool/claude |
| Claude Code models | `ANTHROPIC_DEFAULT_OPUS_MODEL` and `..._SONNET_MODEL` = `glm-5.3[1m]`, `..._HAIKU_MODEL` = `glm-5.3-flash[1m]` in the manual block; the page's default list says GLM-5.3-Flash for all three. Terrarium sets the ids without `[1m]`: `glm-5.3`, `glm-5.3-flash`. | https://docs.z.ai/devpack/tool/claude |
| Claude Code version | "verified compatibility with Claude Code 2.0.14 and other versions" | https://docs.z.ai/devpack/tool/claude |
| OpenAI-compatible, Coding Plan | `https://api.z.ai/api/coding/paas/v4` (Chat Completions). Another URL does not spend the plan quota. A Responses-protocol URL `https://api.z.ai/api/v1` also exists (not used). | https://docs.z.ai/devpack/tool/others |
| OpenAI-compatible, pay as you go | `https://api.z.ai/api/paas/v4` (`.../chat/completions`) | https://docs.z.ai/guides/overview/quick-start |
| Key header | `Authorization: Bearer <key>` | https://docs.z.ai/guides/overview/quick-start |
| Where to get a key | `https://z.ai/manage-apikey/apikey-list` | https://docs.z.ai/guides/overview/quick-start |
| Prices (USD per 1M tokens, in / cached in / out) | GLM-5.3 1.4 / 0.26 / 4.4; GLM-5.3-Flash 0.15 / 0.03 / 0.50; GLM-4.6 0.6 / 0.11 / 2.2; GLM-4.5-Air 0.2 / 0.03 / 1.1 | https://docs.z.ai/guides/overview/pricing |
| Usage endpoint | **Not documented.** `GET https://api.z.ai/api/monitor/usage/quota/limit`. Verified live on 2026-10-26: see "Z.ai usage" below. The probe `zai-usage` reads it. | https://github.com/getpaseo/paseo/issues/5057, https://github.com/guyinwonder168/opencode-glm-quota |

pi 1.4.0 (the installed `pi-ai`), **verified offline** on 2026-10-26 with
`pi --offline --list-models zai` in an empty `PI_CODING_AGENT_DIR` (no
network, no model turn):

- pi has a built-in provider `zai`: base URL
  `https://api.z.ai/api/coding/paas/v4`, key from `ZAI_API_KEY`, API
  `openai-completions` (`pi-ai/dist/providers/zai.js`). Its catalog has
  `glm-4.7`, `glm-5-turbo`, `glm-5.2`, `glm-5.3`, `glm-5.3-flash`, and the
  `-highspeed` models.
- The custom provider `zai-api` that Terrarium writes into the agent's
  `models.json` (base URL `https://api.z.ai/api/paas/v4`, `apiKey`
  `"$ZAI_API_KEY"`) loads: `--list-models` shows `zai-api glm-5.3` and
  `zai-api glm-5.3-flash`.
- pi turns on its Z.ai request format from the base URL
  (`detectCompat`: `baseUrl.includes("api.z.ai")`), so the custom provider
  needs no `compat` block.

Not tested: a turn on either endpoint, through either harness; whether
the Anthropic-compatible endpoint takes a pay-as-you-go key; whether
Claude Code shows an auth-conflict notice when `ANTHROPIC_AUTH_TOKEN` is
set next to the shared Claude login; the `[1m]` model suffix. The live
test is `cargo test -p terrarium-adapter-pi --test zai_live -- --ignored`
with `ZAI_API_KEY` set (docs/getting-started.md §6.3).

### Z.ai models

**Verified live** on 2026-10-26 with the stored `zai-key` (GLM Coding
Plan): one `GET https://api.z.ai/api/coding/paas/v4/models` with
`Authorization: Bearer <key>`, no model turn. The answer is `200` in the
OpenAI list shape (sanitised; the full body is the fixture
`crates/terrariumd/tests/fixtures/models/zai_coding.json`):

```json
{"object":"list","data":[{"id":"glm-4.5","object":"model","created":1753632000,"owned_by":"z-ai"},
  ...,{"id":"glm-5.3","object":"model","created":1786636800,"owned_by":"z-ai"},
  {"id":"glm-5.3-flash","object":"model","created":1786636800,"owned_by":"z-ai"},
  {"id":"glm-5.3-flashx","object":"model","created":1786636800,"owned_by":"z-ai"}]}
```

The ids: `glm-4.5`, `glm-4.5-air`, `glm-4.6`, `glm-4.7`, `glm-5`,
`glm-5-turbo`, `glm-5.1`, `glm-5.2`, `glm-5.3`, `glm-5.3-flash`,
`glm-5.3-flashx`. No name and no context window. The list differs from
pi 1.4.0's `zai` catalogue (pi has the `-highspeed` models, Z.ai lists
`glm-5.3-flashx` and the 4.5/4.6 models), so `GET
/api/subscriptions/{name}/models` (docs/api.md §16.3) asks the provider
first and pi second. The `api` plan endpoint
(`https://api.z.ai/api/paas/v4/models`) is not tested.

`pi --offline --list-models zai` (pi 1.4.0, in the container, with
`ZAI_API_KEY` set to a placeholder in an empty `PI_CODING_AGENT_DIR`) prints
a table `provider model context max-out thinking images`; the fixture is
`crates/terrariumd/tests/fixtures/models/pi_list_zai.txt`. Without the key
var it prints "No models available".

### Model lists (plan logins)

**Verified live** on 2026-10-26 against the operator's real logins: Claude
Max (OAuth), ChatGPT Pro (Codex OAuth), and GitHub Copilot Individual (the
token in `config.json`). Each request is one read-only `GET`, with no model
turn. No token was printed or stored. The code is
`crates/terrariumd/src/models/plan.rs` (the `plan` source of `GET
/api/subscriptions/{name}/models`, docs/api.md §16.3). The sanitised
answers are the fixtures `anthropic_models.json`, `codex_models.json`, and
`copilot_models.json` in `crates/terrariumd/tests/fixtures/models/`. The
credentials come from the same files as the usage probes (above).

| Harness | Request | Filter and order |
|---|---|---|
| Claude Code | `GET https://api.anthropic.com/v1/models?limit=1000`, `Authorization: Bearer <claudeAiOauth.accessToken>`, `anthropic-version: 2023-06-01`, `anthropic-beta: oauth-2025-04-20`. A stored Anthropic key sends `x-api-key: <key>` and no beta header. | All rows, in the API order. `has_more: true` reads the next page with `after_id=<last_id>`. |
| Codex | `GET https://chatgpt.com/backend-api/codex/models?client_version=<codex version>`, `Authorization: Bearer <tokens.access_token>`, `ChatGPT-Account-Id: <tokens.account_id>` | Only `visibility == "list"`, sorted by `priority` (low first). |
| Copilot | `GET https://api.github.com/copilot_internal/user`, `Authorization: token <GitHub token>`, for `endpoints.api`. Then `GET <endpoints.api>/models`, `Authorization: Bearer <GitHub token>`, `Copilot-Integration-Id: copilot-developer-cli` | Only `model_picker_enabled == true`, `capabilities.type == "chat"`, and `policy.state` not `disabled`. In the API order. |

**Claude Code.** The answer is `200` in Anthropic's list shape: `{data:
[{type, id, display_name, created_at, max_input_tokens, max_tokens,
capabilities, ...}], has_more, first_id, last_id}`. It had 14 models, from
`claude-haiku-5-5`, `claude-sonnet-5-5`, `claude-opus-5-5`, and
`claude-fable-5-1` to `claude-sonnet-4-5-20250929`, with `has_more: false`.
The name is `display_name`. The context is `max_input_tokens`.

**Codex.** The answer is `200` `{models: [{slug, display_name,
visibility, supported_in_api, context_window, priority, ...}]}` with 9 rows.
Two rows have `visibility: "hide"` (`gpt-reserve` and
`codex-auto-review`), so the list has 7 models, from `gpt-6.1-sol`
(priority 1) to `gpt-5.6-luna` (priority 9). The id is `slug`. The context
is `context_window`. `client_version` is the installed Codex version from
`GET /api/harnesses`, else the pin in `harnesses.toml`. A Codex
subscription with an OpenAI API key does not use this request. It uses the
`provider` source (`https://api.openai.com/v1/models`).

**Copilot.** `endpoints.api` was `https://api.individual.githubcopilot.com`.
When the user endpoint names no `https://` API, the daemon uses
`https://api.githubcopilot.com`. The models answer is `200` `{data: [{id,
name, model_picker_enabled, policy: {state}, capabilities: {type, limits:
{max_context_window_tokens}}}], object}` with 24 rows. The filter keeps 4:
`claude-sonnet-4.6`, `gpt-5.3-codex`, `mai-code-1.1-flash`, and
`claude-haiku-4.5`. It drops `claude-opus-4.8`, because its
`policy.state` is `disabled`. It also drops the models that are not in the
picker and the embedding models. The ids have dots. Copilot's `--model`
takes these ids, so the daemon does not change them.

The token is not in an error note: the daemon replaces each token and the
account id with `<redacted>`. An expired Claude token (`expiresAt`) fails
before the request, as for the probe. The daemon never refreshes a token.

## vmlab workspace attach

vmlab 0.33.3-alpha, 2026-10-25. The question: how does a host agent put an
agent workspace (a host directory) into a vmlab machine at
`/work/<project>/<agent>` (PLAN §11, "mode 1")? The reference is the
`vmlab` skill (`containers.md`, `vm.md`, `dev-machines.md`,
`shares-media.md`, `lab-file.md`).

### The options

| Mechanism | From outside the lab file | Needs | Result |
|---|---|---|---|
| `container { volume { host = ... } }` | No: a lab file attribute. | The `host` path must be a directory under the lab root, and the container needs a NIC (the volume mounts from the segment gateway, over virtiofs or SMB). | Not used. The test lab has no NIC, and a path under the lab root cannot be the work root. |
| `vm { share { host = ... } }` | No. | A NIC unless `transport = "virtiofs"`. VMs only. | Not used: containers are the cheap case. |
| `@dev(workspace = ..., workspace_guest = ...)` with the workspace syncer | No: a decorator in the lab file. | The guest agent's `watch` and `fileops` (a container's agent has them). No NIC: the syncer uses the agent channel. | **Used.** |
| `vmlab dev sync ...` | The verbs act on a declared `@dev` machine only. | A `@dev` machine. | Status and flush only; it does not attach a new dir. |

vmlab has no overlay or include file, and no CLI flag that adds a mount
to a lab. So the host agent writes the lab file of each instance itself:
`vmlab.prepare` copies the checked definition to
`<instances_dir>/<instance>/lab/vmlab.wcl` and adds
`@dev(workspace = "<work_dir>/<project>/<agent>", workspace_guest = "/work/<project>/<agent>")`
to the target's machine (docs/host-protocol.md, "`vmlab.prepare`"). The
agent's own `vmlab.wcl` (in a worktree or in its workspace) does not
change.

### What the tests showed

All with a scratch lab (`lab "mode1-exp"`, one `container "box"`,
`alpine:3.22`, `mode = :idle`, no NIC):

| Check | Result |
|---|---|
| `vmlab validate` with `@dev(workspace = "/abs/path")`, `"./link"` (a symbolic link out of the lab), and `"../hostwork"` | `ok` for all three. vmlab does not keep a `@dev` workspace in the lab root. So the host agent checks the paths of an agent-written lab itself (the allowlist). |
| `vmlab up` with an absolute `@dev(workspace)` | Up in 2.3 s. `vmlab status -v` shows a `DEV` table with the host dir and `/work/p/a`. The first pass seeded the guest: `ls /work/p/a` showed the host file, owned by root (the container floor). |
| A new file on the host | In the guest after about 400 ms. |
| A file written in the guest (`vmlab exec box -- sh -c 'echo built > /work/p/a/out.txt'`) | On the host after about 400 ms, owned by the host user. `vmlab dev sync status`: `in step`. |
| A changed `@dev` in the lab file of a running lab, then `vmlab up` | Refused, exit 1: `vmlab.wcl has changed since the lab daemon loaded it, and box is still running ... Run vmlab down`. So a workspace cannot join a running lab. |
| The same lab file in two directories, `vmlab up` in each | The second is refused, exit 5: ``lab `mode1-twin` is already registered from <first dir>``. vmlab keys a lab by its name. |

### Limits

- One workspace for each machine: vmlab refuses a second `@dev` on one
  machine. A `lab_repo` target attaches its first holder; the others get
  "a vmlab machine syncs one workspace". A `lab_from_workspace` target
  gives each holder its own instance, so each has its own.
- A workspace cannot join or leave a running lab (the changed-file
  refusal). The daemon makes a new instance when the target is ephemeral
  and no exec session is live; else "attach needs a restart of the
  target".
- The syncer copies, it does not mount: there is a delay of about 400 ms
  each way, a halt on a conflict (`.vmlab-sync-halt` in the guest; resolve
  with `vmlab dev sync resolve` on the host), and a guard on a mass delete
  from the guest. `.gitignore` and `.vmlabignore` paths are guest-owned and
  do not sync: a `target/` in `.gitignore` stays in the guest. That is good
  for build output, but a test report under an ignored path does not reach
  the host.
- `workspace_guest` is `/work/<project>/<agent>`, a Linux path. A Windows
  guest needs another mapping (not done).
- **Awaiting Wil's vmlab change:** two instances of one lab definition at
  the same time (two holders of a `lab_from_workspace` target with the same
  lab name, or a `lab_repo` target and a copy of its lab) are refused with
  exit 5 until vmlab keys a lab instance by its directory. The code makes
  one instance dir for each holder already (`<target>.<agent ULID>`), and
  does not rename the lab.

## Webhooks (GitHub, Forgejo)

The ingest listener (docs/api.md §20) reads webhooks from GitHub and
Forgejo. `crates/terrarium-core/src/ci.rs` turns them into events. The
facts below come from the source and the docs on 2026-10-26, not from a
live delivery. The fixtures in `crates/terrarium-core/tests/fixtures/ci/`
are not recorded deliveries. We wrote them from the payload structs and
the documented examples, and we trimmed them. A field that we could not
confirm is not in them.

### Sources

| What | Version | Where |
|---|---|---|
| Forgejo source | `forgejo` branch at `627f15a` (2026-10-26); tag `v17.0.0` (2026-10-15); branches `v16.0/forgejo` and `v15.0/forgejo` | <https://codeberg.org/forgejo/forgejo>: `services/webhook/shared/payloader.go`, `modules/webhook/type.go`, `modules/structs/hook.go`, `modules/structs/action.go`, `services/convert/action.go`, `services/actions/run.go`, `models/actions/run.go`, `models/actions/status.go`, `services/actions/commit_status.go`, `routers/web/repo/setting/webhook.go` |
| Forgejo release notes | 17.0.0 | `release-notes-published/17.0.0.md`: PR #14334 (webhooks for workflow jobs), #14351 (fine-grained workflow run events), #14356 (the `action_run_*` events are deprecated), #14504 (no `repository` in the job payload) |
| GitHub docs | 2026-10-26 | <https://docs.github.com/en/webhooks/webhook-events-and-payloads>, <https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries>, and the `webhook-*` schemas in `github/rest-api-description` |
| Gitea source, for comparison | `release/v1.23`, `v1.24`, `v1.25` | `modules/webhook/type.go` |

### Headers

| Header | GitHub | Forgejo |
|---|---|---|
| Event | `X-GitHub-Event` | `X-Forgejo-Event`, `X-Gitea-Event`, `X-Gogs-Event`, and also `X-GitHub-Event`. The value is the event, for example `pull_request`. |
| Event type | none | `X-Forgejo-Event-Type` (and the Gitea, Gogs, and GitHub names): the raw type, for example `pull_request_sync`. Terrarium reads the event, not the type. |
| Delivery ID | `X-GitHub-Delivery`, a GUID | `X-Forgejo-Delivery` (and the other names), a UUID (`models/webhook/hooktask.go`) |
| Signature | `X-Hub-Signature-256: sha256=<hex>`, only when a secret is set | `X-Forgejo-Signature`, `X-Gitea-Signature`, `X-Gogs-Signature`: the hex with no prefix. Forgejo also sends `X-Hub-Signature-256: sha256=<hex>` and `X-Hub-Signature: sha1=<hex>`. With an empty secret, Forgejo still sends the headers: an empty string, and `sha256=`. |

The signature is the HMAC-SHA256 of the body with the secret. The unit
test `ingest::tests::signatures_per_provider` checks the GitHub example
of "validating webhook deliveries" (secret `It's a Secret to Everybody`,
body `Hello, World!`).

**Content type.** Both forges can post `application/json` or
`application/x-www-form-urlencoded` with the JSON in the form field
`payload`. Forgejo signs the JSON string, not the form body
(`services/webhook/default.go`). For GitHub we did not confirm which bytes
it signs for a form body. Terrarium takes `application/json` only, and
answers `415` to a form body.

### Run and job events

| Forge | Events | Payload |
|---|---|---|
| GitHub | `workflow_run`: `action` is `requested`, `in_progress`, or `completed`. `workflow_job`: `queued`, `in_progress`, `completed`, or `waiting`. | `workflow_run.{id, name, head_branch, head_sha, run_attempt, html_url, status, conclusion, display_title, event, pull_requests}`; `workflow_job.{id, run_id, name, workflow_name, head_branch, head_sha, run_attempt, html_url, conclusion}`. The run conclusions: `action_required`, `cancelled`, `failure`, `neutral`, `skipped`, `stale`, `success`, `timed_out`, `startup_failure`. |
| Forgejo 17 | One event for each status, the same in the event header and the type: `workflow_run_{blocked,cancelled,failure,running,skipped,success,waiting}` and `workflow_job_{...}` with the same statuses. There is no plain `workflow_run` or `workflow_job` event. | `WorkflowRunPayload {action, run}`: `action` is `new_run_attempt`, `run_status_changed`, or `run_completed`. `WorkflowJobPayload {action, job, run}`: `new_job_attempt`, `job_status_changed`, or `job_completed`. `run` is an `ActionRun`: `id`, `title`, `repository`, `workflow_id` (the file name, for example `ci.yml`), `index_in_repo`, `trigger_user`, `prettyref`, `commit_sha`, `event`, `event_payload` (the whole trigger payload as a string), `trigger_event`, `status`, `started`, `stopped`, `created`, `updated`, `duration`, `html_url`. `job` is an `ActionRunJob`: `id`, `run_id`, `attempt`, `name`, `html_url`, `needs`, `runs_on`, `task_id`, `status`. |
| Forgejo 12 to 16 (17 too, deprecated, to be removed after 19) | `action_run_failure`, `action_run_success`, and on 12 to 16 `action_run_recover` | `ActionPayload {action, run, prior_status}`: `action` is `failure`, `recover`, or `success`. |

Facts that change the mapping:

- **Forgejo has no conclusion field.** The status is in the event name and
  in `run.status`: `unknown`, `waiting`, `running`, `success`, `failure`,
  `cancelled`, `skipped`, `blocked`.
- **Forgejo sends one run event for each change, not two.**
  `services/actions/run.go` calls `WorkflowRunStatusChanged` while the run
  is not done and `WorkflowRunCompleted` when it is. So the event name
  alone decides the kind.
- **`prettyref` is not always a branch.** For a pull request run it is
  `#<number>` (`ActionRun.PrettyRef` in `models/actions/run.go`). Terrarium
  then sets `branch` to `null` and `pr.number` to the number.
- **Forgejo has no `head_branch`, `head_sha`, `run_attempt`, or
  `display_title`.** Terrarium uses `prettyref`, `commit_sha`, the job's
  `attempt`, and `title`.
- **Not confirmed:** how `started` and `stopped` serialise (they are
  `optional.Option[time.Time]`). Terrarium does not read them.

### Commit status

GitHub sends `status` (`state`: `pending`, `success`, `failure`,
`error`; fields `sha`, `context`, `description`, `target_url`,
`branches`, `commit`). Gitea sends `status` from 1.23.

**Forgejo does not send a `status` webhook.** There is no such event in
`HookEventType` on the `forgejo` branch, `v17.0.0`, v16, or v15. Forgejo
Actions does set commit statuses (`services/actions/commit_status.go`,
context `<workflow> / <job> (<event>)`, states `pending`, `success`,
`error`, `failure`, `warning`, `skipped`), but they fire no webhook. Use
the workflow run and job events instead.

### Push and pull request

| Field | GitHub | Forgejo |
|---|---|---|
| Compare link | `compare` | `compare_url` |
| Pusher | `pusher {name, email}` | `pusher`, a full user (`login`) |
| Commit count | `commits` (the list) | `commits` and `total_commits` |
| Push to a pull request | `action: synchronize` | `action: synchronized`, event `pull_request`, type `pull_request_sync` |
| Merge | `action: closed` with `pull_request.merged: true` | the same (`services/webhook/notifier.go`) |

### Ping and test delivery

GitHub sends `ping` (`zen`, `hook_id`, `hook`) when you make a webhook.
Forgejo has no ping. Its "Test delivery" sends a `push` event: a
`PushPayload` of the head of the default branch with `before` equal to
`after` (`WebhookTest` in `routers/web/repo/setting/webhook.go`). An empty
repository gets a fake commit with the message "This is a fake commit".

### Not tested

- No live delivery from GitHub or Forgejo. The next step is one
  delivery of each kind to a running stack, and the recorded bodies as
  new fixtures.
- Gitea's own `workflow_run` and `workflow_job` events (1.24 and 1.25)
  follow the GitHub shape. A `forgejo` source that is really a Gitea
  server would send them with `X-Gitea-Event: workflow_run`; Terrarium
  makes `ext.forgejo.workflow_run` from them today.

## Target displays: VNC and RDP

Live checks of the host agent's VNC and RDP drivers (PLAN §19b, step 2)
on 2026-10-27. Each server ran in a throwaway container from
`examples/display/` on a random loopback port. The driver ran in the
`display_live` example of `terrarium-host`
(`crates/terrarium-host/examples/display_live.rs`), which runs the same
`dispatch` as the host agent, with one kept connection for all the
operations of a run. OCR was `tesseract` 5.5.3 on the host.

### VNC: x11vnc on Xvfb

Debian bookworm, x11vnc 0.9.16-9, Xvfb 1280 x 800 x 24, openbox, xterm
(`vnc.Dockerfile`).

| Item | Result |
|---|---|
| Version | RFB 3.8 |
| Security | Type 2, VNC authentication, with a random 8-character password. A wrong password: `failed`, "authentication failed", with no password in the text. |
| Encoding | x11vnc answered with ZRLE (the driver's trace log: one rectangle, encoding 16). A full screen was 22 to 35 KB of PNG; a screenshot took 140 to 180 ms on loopback. |
| `info` | `width` 1280, `height` 800, `ops` with `scroll` and `ocr` (`tesseract`). |
| `click`, `type`, `keys` | A click put the focus in the xterm. `type` "echo hello-from-vnc\n" and `printf "%s\n" tErRa_123` ran: the screen and the OCR showed the commands and their output, with the capitals and the symbols right. `keys ctrl-l` cleared bash's screen. |
| `drag` | A drag of the title bar moved the window. |
| `scroll`, double click | The server took them (no error); not checked on the screen. |
| `screenshot` with `region` and `scale` | Right sizes. |
| Text outside ASCII | `é` and `€` did not show in the xterm, with or without x11vnc `-add_keysyms`. The keysyms leave the client (the unit tests check them); the C locale of the xterm, or the X keymap, drops them. Not solved. |

### RDP: xrdp

Debian bookworm, xrdp 0.9.21.1, xorgxrdp 0.9.19, openbox, xterm
(`rdp.Dockerfile`).

| Item | Result |
|---|---|
| Security | xrdp offers SSL and RDP security only (its log: "configured [SSL\|RDP], requested [SSL\|HYBRID\|HYBRID_EX\|RDP], selected [SSL]"). So the driver used TLS (TLS 1.3, `TLS_AES_256_GCM_SHA384`) with the password in the Client Info PDU and auto logon. NLA was not checked: xrdp has none. |
| Wrong password | With TLS alone, the connection does not fail: the screen shows xrdp's "login failed for user lab" dialog. The NLA path fails at the login instead. |
| Pin | The first connection wrote the line `127.0.0.1:<port> B4:80:...` to the pin file. With a changed line, the next connection ended with `refused`, both fingerprints, and the `terrarium-host rdp-forget` hint; xrdp logged no login. |
| Codecs | IronRDP offered RemoteFX, and xrdp used it. The screen was right, with faint marks at the 64-pixel tile corners (RemoteFX is lossy). With no RemoteFX (plain bitmaps, 32-bit colour), IronRDP drew xrdp's updates wrong (stripes and smeared text). So the driver keeps IronRDP's default codecs. |
| Screen | 1280 x 800, the size the client asks for. A screenshot took 340 to 460 ms: it waits until the screen has no update for 250 ms. |
| Input after login | xrdp dropped the input of the first moment after the login: the first click of a new connection did not arrive (checked with `xinput test-xi2` in the container). The driver now waits for the first screen updates after the login; then 12 key chords sent at once all arrived. At the first login of a new container, the desktop starts after the login: a click in the first second can come before the xterm is there. |
| `click`, `type`, `keys` | A click put the focus in the xterm. `type` (Unicode key events) "seq 1 100\n" and `printf "%s\n" tErRa_RDP-9 "a\|b&c"` ran, with the capitals and the symbols right. F1 to F12 arrived as the right X keycodes (67 to 76, 95, 96 in `xinput test-xi2`). |
| `scroll` | 10 wheel steps up scrolled the xterm back (the OCR read the old lines); over the root window, openbox changed the desktop. |
| `drag`, right click | A drag of the title bar moved the window. A right click on the root window opened the openbox menu. |
| A server that drops the session | Not checked live. A DeactivateAll (a resize) ends the connection, and the next operation connects again. |

### Not tested

- RDP with NLA (CredSSP) against Windows. The code path is IronRDP's
  `connect_finalize` with NTLM; the unit tests do not reach it.
- VeNCrypt or TLS for VNC: the client does not speak them.
- A VNC server that changes the screen size (DesktopSize): the unit
  tests do not cover it, and x11vnc on Xvfb does not resize.

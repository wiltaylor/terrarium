# Grok Build hook fixtures

`captured/`: the payloads of the signed-in grok 1.0.77 check in ask mode
(TER-1312, 2026-11-14), in order, written by a personal hook that saved
stdin. One new session (a read-only command, a `touch`, a write that
asked for permission and was allowed once, a turn cancelled with Ctrl+C,
`/quit`), then `--resume` with one turn and `/quit`. They hold no
credential: no token, key or account field is in a hook payload. The
paths are the check's private scratch folder.

What the capture shows:

- Every field comes twice: camelCase and the Claude Code snake_case name.
  The event name differs: `hookEventName` is snake_case
  (`pre_tool_use`), `hook_event_name` is PascalCase (`PreToolUse`).
- `SessionStart.source` is `new` for a new session and `load` for
  `--resume`. `SessionStart` has no `transcriptPath`; later events do.
- `Stop.reason` is `end_turn`. A Ctrl+C cancel sends no `Stop`. `/quit`
  sends `SessionEnd` (`reason` `shutdown`) and then `Stop` (`reason`
  `shutdown`).
- `Notification.notificationType` is `permission_prompt` when the
  permission question shows, and `idle_prompt` about a minute after a
  cancelled turn.
- The shell tool is `run_terminal_command`; `toolUseId` is
  `call-<uuid>-<n>`; `PostToolUse` adds `toolResult`, `durationMs`.

The other files are hand-written from the documented payload (docs.x.ai,
"Hooks"), from before the capture. They keep the PascalCase
`hookEventName` and the camelCase-only bodies that the mapper still
accepts.

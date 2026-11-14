# Grok Build hook fixtures

Hand-written from the documented payload (docs.x.ai, "Hooks"):
`hookEventName`, `sessionId`, `cwd`, `workspaceRoot`, and `toolName` and
`toolInput` on the tool events. `prompt`, `source`, `error`, `reason`, and
`message` are not documented; they follow the Claude Code names. These
are NOT captured from a signed-in grok. Replace them with the captured
payloads of the signed-in check (TER-1192 release gate).

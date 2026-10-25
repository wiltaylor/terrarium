# The adapter compatibility suite

The suite proves that each adapter works with the pinned harness binary
(PLAN §23.5, "Compatibility"). It starts the real harnesses through the
adapters and the in-process `Runner`. It does not use the daemon. The crate
is `crates/terrarium-compat`. The facts that the suite checks come from
[`compat.md`](compat.md).

The suite spends real subscription turns. It is not in `just ci::check`.

## Run it

```sh
just ci::compat            # every harness (pi only when you enable it)
just ci::compat claude     # one harness
just ci::compat codex,copilot
```

The recipe builds the `terrarium` CLI (the hooks call it), then runs the
ignored test `real_harnesses`:

```sh
cargo build -p terrarium
TERRARIUM_COMPAT_HARNESSES=claude \
  cargo test -p terrarium-compat --test compat -- --ignored --exact real_harnesses --nocapture
```

The report goes to stdout as a Markdown table and to
`target/compat-report.md`. The test fails when a step fails. A skipped step
does not fail the test.

`cargo test -p terrarium-compat` without `--ignored` runs only the dry run
(see below). It starts no harness and takes less than a second.

## Safety rules

- Every real-harness test is `#[ignore]`. Only `--ignored` runs it.
- Each harness gets at most 3 model turns in one run. The runner types each
  turn one time only (`submit_attempts = 1`), so a retry cannot type a turn
  two times.
- The suite uses the cheapest model of each harness: Claude `haiku`, Codex
  `gpt-6-luna` with `model_reasoning_effort="low"`, Copilot
  `claude-haiku-4.5` (0.33x). The prompts are short, for example "Reply with
  exactly: pong".
- The suite never runs a login or a logout. A harness that is not logged in
  is skipped with the reason.
- pi does not run by default. This host has no pi login, and pi cannot
  spend the Claude plan (`compat.md`, "pi"). To run pi, set
  `TERRARIUM_COMPAT_PI=1`, `TERRARIUM_COMPAT_PI_PROVIDER` (an API-key
  provider, for example `openai`), `TERRARIUM_COMPAT_PI_MODEL`, and the API
  key in the env var that the adapter names for that provider
  (`OPENAI_API_KEY` for `openai`). The suite refuses a provider that the
  adapter refuses.
- A harness whose shared home holds the operator's own hooks is skipped
  (step `user_hooks`). In sandbox mode `none`, those hooks fire in the test
  agent, and they can act outside the home, where the leftover check cannot
  see. Set `TERRARIUM_COMPAT_ALLOW_USER_HOOKS=1` to run that harness anyway.
  On this host, `~/.codex/hooks.json` and `~/.copilot/hooks/orca.json` hold
  such hooks.

## The deployment model, and the differences

The suite uses the operator's real harness homes read-write, as
`crates/terrariumd/tests/e2e_claude.rs` does. The sandbox mode is `none`
(the bwrap tests are elsewhere). Each run gets a temp root
`/tmp/tcompat-<random>` with the state dir and the agent workspace.

The child env is the daemon's clean env for mode `none`: `LANG`, `LC_*`,
`TZ`, `USER`, `LOGNAME`, `HOME`, `PATH` (with the CLI directory first),
`TERM`, and the adapter's env. No `CLAUDE*`, `ANTHROPIC*`, `OPENAI*`,
`COPILOT*`, `GH_TOKEN`, `GITHUB_TOKEN`, or `PI_*` var passes from the parent.

Sandbox mode `none` cannot bind the per-agent overlays of `materialize`
over the shared home. Thus:

| Harness | Difference | What the suite does |
|---|---|---|
| Claude | `$CLAUDE_CONFIG_DIR/.claude.json` has no onboarding key and no trust key, so the start shows the theme picker. | It copies the per-agent `claude.json` over the shared file for the run. It keeps the original bytes and puts them back at the end (also after a panic). A copy of the original is in `target/compat-claude-state-backup.json` until the restore is done. |
| Claude | The other overlays (`sessions/`, `history.jsonl`, `backups/`, the auto memory) are not bound. | The harness writes to the shared files. The leftover check reports them. |
| Codex | The per-agent empty `hooks.json` is not bound, so the operator's `hooks.json` fires. | The `user_hooks` gate (see above). |
| Copilot | `SYSTEM.md` and `APPEND_SYSTEM.md` reach the harness only through the overlays. | `system_prompt_applied` is skipped. The resume turn asks for the first reply only. |
| Copilot | The token is in the system keychain (D-Bus). | The suite adds `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR` to the child env. |

## The scenario

Each harness runs these steps. Each step records pass, fail, or skip, with
evidence.

| Step | What it checks |
|---|---|
| `version` | `detect_version` is in `tested_version`. |
| `auth` | The adapter's auth check says "logged in". If not, the other steps are skipped. |
| `user_hooks` | Only when the shared home has the operator's hooks (see "Safety rules"). |
| `fresh_start` | `materialize` into the temp agent dir, `launch`, `Runner::spawn`. The status goes from `starting` to `idle` within 30 s. The adapter's screen hint never says `Question` (a dialog). No error line is on the screen. The time to the ready prompt is in the report. |
| `turn_with_hooks` | Turn 1, "Reply with exactly: pong". The `Delivery` resolves `Ok`. `TurnStart` and `TurnEnd` come. The status goes back to `idle`. The session ID comes from `SessionStarted` (or `find_session_id`). The submit and stop hook payloads have the fields that `compat.md` names, `cwd` is the workspace, and `session_id` is the session. The reply has "pong". |
| `system_prompt_applied` | The rendered prompt has "The secret word is MARMALADE". Turn 2 asks for the secret word. The reply has MARMALADE. |
| `resume` | The suite types the adapter's exit keys (`/exit`, `/quit`) and waits 15 s, else it kills. It types them as the runner's `stop` does: with an `exit_key_pause` (Codex: 300 ms), it types the text, waits the pause, and then types the CR. Then it spawns again with `resume = Some(session id)`. `SessionStarted` comes with `source = resume` (not for pi, whose `reason` stays `startup`) and the same session ID. Turn 3 asks for the secret word and for the first reply. The reply has MARMALADE and "pong", so the history came back. |
| `usage_parsed` | The adapter's usage parser reads one or more samples with a model name from the session file. |
| `leftovers` | See below. |

The reply text comes from the stop hook (`last_assistant_message`) when the
harness gives it (Claude, Codex, pi). Else it comes from the screen, without
the lines that hold the typed text.

### Sub-checks for open items in `compat.md`

| Harness | Step | What it checks |
|---|---|---|
| Codex | `empty_hooks_file` | Is an empty `{"hooks": {}}` file accepted at start? The dry part checks that the per-agent file is that JSON. The live part starts Codex two times on a throw-away `CODEX_HOME` under `target/` with no login: one time with the empty file, and one time with a broken file as the control. It types nothing and reads no credential. When neither screen has a hook message, the result is "inconclusive" (skip). |
| Copilot | `empty_prompt_files` | Is a start with both prompt files empty free of errors? The dry part checks that `materialize` with no prompt writes both files empty. The live part needs the prompt extension: set `TERRARIUM_COMPAT_COPILOT_PROMPT_EXT` to the `copilot-custom-system-prompt` package directory. Then the fresh start loads it with both shared files empty. |

## Leftovers

The suite lists the shared home before the first probe. After the run, it
lists the home again and compares:

- A new path that names a run key is removed. The run keys are the session
  IDs, the agent ULID, and the name of the temp root. The temp root name is
  also in the workspace path, so Claude's `projects/<encoded workspace>/`
  directory matches.
- A new small file that has a run key in its content is removed (for
  example `sessions/<pid>.json`).
- A new mount point that `materialize` made in the home is removed when it
  still has the bytes that `materialize` wrote.
- A new directory that becomes empty after these removals is removed.
- Other new paths are reported, not removed. The operator's own sessions
  write to the same home at the same time.
- A file that was there before is never removed or rewritten. When it
  changed and has run lines (for example `history.jsonl`), the report gives
  the number of lines.

## The dry run

`TERRARIUM_COMPAT_DRY_RUN=1` (and the test `dry_run`, which always runs)
does these steps for every harness, pi too:

- It checks the version (`<harness> --version`; a missing binary is a skip).
- It materializes into a temp home, not the real home.
- It builds the fresh and the resume `LaunchSpec`, and checks the program,
  the cwd, the model, the session ID on resume, and the clean env.
- It does the dry parts of the two sub-checks.
- It reads the real homes for the `user_hooks` gate. It does not write to
  them.

The dry report goes to `target/compat-report-dry.md`.

## Settings

| Var | Default |
|---|---|
| `TERRARIUM_COMPAT_HARNESSES` | `all`, or a comma list |
| `TERRARIUM_COMPAT_DRY_RUN` | unset; `1` = dry run |
| `TERRARIUM_COMPAT_CLI` | `target/debug/terrarium` |
| `TERRARIUM_COMPAT_<H>_HOME` | `~/.claude`, `~/.codex`, `~/.copilot`, `~/.pi/agent` |
| `TERRARIUM_COMPAT_<H>_MODEL` | `haiku`, `gpt-6-luna`, `claude-haiku-4.5` |
| `TERRARIUM_COMPAT_PI`, `TERRARIUM_COMPAT_PI_PROVIDER`, `TERRARIUM_COMPAT_PI_MODEL` | unset (pi does not run) |
| `TERRARIUM_COMPAT_ALLOW_USER_HOOKS` | unset |
| `TERRARIUM_COMPAT_COPILOT_PROMPT_EXT` | unset |

`<H>` is `CLAUDE`, `CODEX`, `COPILOT`, or `PI`.

## Findings on this host (2026-10-24)

- Claude Code 2.1.310: all steps pass with 3 haiku turns. The start reached
  `idle` 0.4 s after the spawn, with no dialog. The resume gave
  `SessionStarted.source = resume` with the same session ID.
- `claude auth status` with `CLAUDE_CONFIG_DIR` set migrates
  `$CLAUDE_CONFIG_DIR/.claude.json` (`migrationVersion` 13 to 14) and writes
  a backup of the old file to `backups/`. The daemon's auth probe does the
  same in the shared home.
- In mode `none`, Claude appends each typed turn and each `/exit` to the
  shared `history.jsonl`. The overlay of the bwrap sandbox prevents it.
- Codex 0.160.0 and Copilot 1.0.103: the `user_hooks` gate skipped the live
  steps. The Codex `empty_hooks_file` check is inconclusive: with no login,
  Codex shows the sign-in screen and no hook message for either file.
- Copilot: `config.json` in `~/.copilot` names no login and has no token,
  so the file-based auth check says "not logged in".

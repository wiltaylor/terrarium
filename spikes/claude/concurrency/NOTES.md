# Spike: two Claude Code agents on one shared home

Tested on 2026-10-24, **outside Docker**: CachyOS host, kernel 7.2,
bubblewrap 0.12.0, Claude Code 2.1.310 (the native build), Claude Max login,
model `haiku`. The daemon ran in-process with `RunnerSupervisor`, sandbox
mode `bwrap`, and the real Claude home (`~/.claude`) bound read-write as the
shared home. This is the same set-up as `claude_in_bwrap_answers_one_turn`.

Four model turns in total. The test is `two_claude_agents_share_one_home`
in `crates/terrariumd/tests/e2e_claude.rs` (`--ignored`). `run.py` runs it,
takes a snapshot of the shared home before and after, and writes
`captured/report.json`.

```sh
uv run spikes/claude/concurrency/run.py            # 4 model turns
uv run spikes/claude/concurrency/run.py --cleanup  # delete what the run made
```

## Steps

1. Make two agents, `alpha` and `beta`, in one project. Start both at the
   same time.
2. Send each agent one turn: `Reply with exactly: <name>-<nonce>`.
3. Stop both. Start both again: the start resumes the last session
   (`--resume <id>`).
4. Send each agent one turn: "What exact text did you reply with to my
   previous message? Reply with only that text."
5. Read the events, the transcripts, and the per-agent files. Compare the
   shared home with the snapshot.

## Facts

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

## Decisions in the code

`crates/adapters/claude/src/config.rs` now binds these per-agent paths over
the shared home (the module doc comment gives the reasons for each path in
the home):

| Target in `$CLAUDE_CONFIG_DIR` | Source in the agent dir | Kind |
|---|---|---|
| `.claude.json` | `claude/claude.json` | file (before this spike) |
| `sessions/` | `claude/sessions/` | directory (before this spike) |
| `history.jsonl` | `claude/history.jsonl` | file, made empty when missing |
| `backups/` | `claude/backups/` | directory |
| `projects/<encoded cwd>/memory/` | `claude/memory/` | directory, bound on top of the shared `projects/` tree |

The adapter makes each mount point in the shared home before the start, so
bwrap does not leave empty files or directories behind.

## Changes for docs/compat.md

1. "Claude inside bwrap", "Not tested": remove "two agents at the same time
   on one home" and "`--resume` of a real session". Add a reference to this
   spike.
2. "Other writes to the shared home": `history.jsonl`, `backups/`, and
   `projects/<cwd>/memory/` are now per-agent.
3. PLAN §21: the risk row about the shared Claude home can say that
   `history.jsonl` is done.

## Not tested

- OAuth token refresh with two agents. The token did not expire in the run.
- Writes to the auto memory. Haiku did not write memory in two turns. The
  overlay is there because the path is keyed by the cwd, which all agents
  share. Another option is to turn auto memory off in the `--settings` JSON;
  Terrarium has its own memory (PLAN §14).
- More than two agents, and long runs (transcript clean-up by
  `cleanupPeriodDays`, which any agent can run on the shared `projects/`).
- Docker (bwrap inside Docker).
- Whether skills in the home `skills/` load with `--setting-sources ""`.

## Clean-up

`--cleanup` deletes only the paths in `captured/new-paths.txt`: the new
paths that name one of the two session UUIDs, the new paths under
`projects/-home-agent-work/`, and new shell snapshots that contain
`/home/agent`. Other new paths (the operator's own Claude sessions can write
at the same time) go to `unattributed_new_paths` in the report and stay.

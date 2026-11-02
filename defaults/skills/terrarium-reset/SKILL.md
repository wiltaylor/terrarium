---
name: terrarium-reset
description: Compact your session or start a new, clean session with `terrarium agent compact --self` and `terrarium agent new-session --self`, and send yourself a self-contained `--then` prompt to go on after the reset. Use it when your context is long, when you switch to unrelated work, or when you are in a confused state. Leads can reset their workers the same way.
---

# Compact or start a new session

Your harness session keeps the whole conversation. A long conversation
costs more on each turn and can confuse you. Terrarium gives you two
resets:

- **Compact**: the harness writes a summary of the conversation and goes
  on from the summary. You keep the same session and the main facts.
- **New session**: Terrarium ends your harness session and starts a new
  one. The new session does not remember the old one. It gets a short
  recap turn (your open cards, open waits, and unread mail), then your
  prompt.

Each reset can carry a prompt (`--then`). Terrarium sends it as your next
turn after the reset, with the tag `[System:reset requested by self]`.

## When to compact and when to start a new session

Compact when:

- you go on with the same work, and the context is long;
- the facts of the conversation are still correct and useful.

Start a new session when:

- you switch to work that is not related to the old work (a new card, a
  new repo);
- you are in a confused state: you repeat the same failed step, you
  remember facts that are not true, or old instructions get in the way;
- a compaction did not help.

Do not reset in the middle of a step that has state only in your
conversation. First write the state down: commit or push your work, add a
card comment, or write a file.

## Write a self-contained `--then` prompt

After a new session, you remember nothing. After a compaction, details can
be lost. So the `--then` prompt must be complete by itself:

1. The goal in one sentence.
2. The card ID and the branch.
3. The files and the commands that matter.
4. What is done already, and what is not.
5. The next steps, in order.

Example:

```sh
terrarium agent new-session --self --then "Card TER-12: add retries to the
HTTP client. Branch ter-12-retries in the project repo. Done: the retry
loop in src/client.rs (commit 4f2a1c). Not done: the tests. Next: 1) add a
test for 3 failures then success in tests/client.rs, 2) run cargo test
-p client, 3) move TER-12 to review."
```

For a long prompt, use a file or stdin:

```sh
terrarium agent compact --self --then-file next.md
terrarium agent compact --self --then-file - < next.md
```

## End your turn after the request

A request for yourself waits until your current turn ends. The command
returns at once with `queued: ... runs when your turn ends`. End your turn
immediately after the command. Do not start other work in the same turn:
the reset runs after your turn, before any other mail or turn.

## The commands

```sh
terrarium agent compact --self [--then "<prompt>" | --then-file <file>|-]
terrarium agent new-session --self [--then "<prompt>" | --then-file <file>|-] [--no-recap]
```

- Only one reset can wait for each agent. A new request replaces the one
  that waits (the output says `replaced`).
- A compaction that runs is not interrupted.
- If the compaction fails, your prompt still comes. Its first line is
  `Note: the compaction did not complete: <reason>.`
- `--no-recap` leaves out the recap turn of a new session.
- The `then` prompt is at most 32 KiB.
- A new session keeps your cards, mail, waits, workspace, and (for an
  ephemeral agent) your task deadline. Only the conversation is new.

## Reset another agent (leads)

A lead (project_admin) can reset the agents of its project. The operator
can reset any agent:

```sh
terrarium agent compact <agent> --then "<prompt>"
terrarium agent new-session <agent> --then "<prompt>" [--no-recap] [--defer]
```

- An agent at rest resets at once. A busy agent resets when its current
  turn ends. `--defer` also waits for the end of the turn.
- A stopped agent with `--then` starts at once in a new session. Without
  `--then`, its next wake starts a new session.
- The tag of the prompt names you: `[System:reset requested by
  <project>/<agent>]`.
- Write the prompt for the other agent: restate its card, its branch, and
  its next step.

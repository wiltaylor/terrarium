# Ready-card dispatcher

The dispatcher is off by default. Set `card_dispatcher = "lead"` in a
project definition, or PATCH `/api/projects/<project>` with
`{"card_dispatcher":"lead"}` for an API-managed project. Set it to `"off"`
to disable. The named agent must be a non-ephemeral project admin.
Removing the setting from a project file or fleet disables dispatch too.

The lifecycle timer fills free places in the column with role `working`.
That column must have an explicit WIP limit. It takes unassigned cards in
`ready` (or the column with that role), by position, skipping unfinished
dependencies (neither accepted nor done), held cards, groups, and cards with an existing bound agent.
Exactly one supported type label is required:

| Label | Template |
| --- | --- |
| `type:small` | `card-small` |
| `type:impl` | `card-impl` |
| `type:hard` or `type:impl-hard` | `card-impl-hard` |

Templates resolve in the project first, then globally. The owner's spawn
policy must allow the template and bound agents. Daily and concurrent spawn
caps still apply. Pausing the owner pauses dispatch. Targets come only from
the type template, just as for a manual template spawn, not the owner's full target list. Subscription pools
and template secret grants use the existing spawn inheritance checks.

The agent is named after the lower-case card ID and bound to that card. The
spawn transaction rechecks eligibility, WIP, and owner configuration before
creating the agent and claiming the card. A failed transaction leaves the
card ready. Card-specific spawn failures get a persistent per-card cooldown: 1 minute,
doubling to at most 30 minutes. One card comment and one mail to the owner
report the failure, not a new notice on every retry. Other eligible cards
proceed while a failed card cools down. A successful claim clears its retry
state. Leaving ready or archiving also clears a card's retry state.

Spawn caps and missing owner-wide spawn rights defer the whole project without card
comments or card cooldowns. One lead mail reports each deferral reason, not
one per ready card or timer tick. A misconfigured owner likewise gets one
project-inbox notice. The timer checks the project once per pass and resumes
when the budget resets or the configuration is repaired. The existing bound
agent lifecycle parks agents for review and tears them down after merge.
Denials specific to a template (allowlist, targets, or grants) use per-card
backoff instead. A selection invalidated by a concurrent edit or claim is
not a failure and does not create a comment, notice, or cooldown.

This switch does not automate reviews, verdicts, or merges. Workers still
validate their own pushed commits before moving cards to review.

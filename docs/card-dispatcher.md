# Ready-card dispatcher

The dispatcher is off by default. Set `card_dispatcher = "lead"` in a
project definition, or PATCH `/api/projects/<project>` with
`{"card_dispatcher":"lead"}` for an API-managed project. Set it to `"off"`
to disable. The named agent must be a non-ephemeral project admin.

The lifecycle timer fills free places in the column with role `working`.
That column must have an explicit WIP limit. It takes unassigned cards in
`ready` (or the column with that role), by position, skipping unfinished
dependencies, held cards, groups, and cards with an existing bound agent.
Exactly one supported type label is required:

| Label | Template |
| --- | --- |
| `type:small` | `card-small` |
| `type:impl` | `card-impl` |
| `type:hard` or `type:impl-hard` | `card-impl-hard` |

Templates resolve in the project first, then globally. The owner's spawn
policy must allow the template and bound agents. Daily and concurrent spawn
caps still apply. Pausing the owner pauses dispatch. Target rights are passed
from the owner's policy through the normal spawn checks. Subscription pools
and template secret grants use the existing spawn inheritance checks.

The agent is named after the lower-case card ID and bound to that card. The
spawn transaction rechecks eligibility, WIP, and owner configuration before
creating the agent and claiming the card. A failed transaction leaves the
card ready. Missing owners, missing templates, name conflicts and exhausted
spawn budgets defer dispatch until a later timer pass. The existing bound
agent lifecycle parks agents for review and tears them down after merge.

This switch does not automate reviews, verdicts, or merges. Workers still
validate their own pushed commits before moving cards to review.

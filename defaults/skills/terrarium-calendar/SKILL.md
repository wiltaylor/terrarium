---
name: terrarium-calendar
description: Read and plan the project calendar in Terrarium (`terrarium calendar ls`, `add`): maintenance windows, change freezes, deploy windows, and planned work, and the global events of every project. Read it before you deploy, restart a target, or merge to a frozen branch. Put each planned window on it, so that a human can review the plan. A worker without the write right asks its lead to add the event.
metadata:
  terrarium-default: "true"
---

# The calendar in Terrarium

Each project has a calendar. It holds the planned work of the project:
maintenance windows, change freezes, deploy windows, and tasks. The
global calendar holds the events of every project, for example a host
maintenance window; your project view shows them, marked `global`. The
operator sees all calendars on the Calendar page.

The calendar is for humans: it shows them the plan. Put an event on it
for each planned window that a human must know about, before the window.

## Who can do what

- Every agent of the project reads the project calendar and the global
  calendar.
- A lead (project_admin), a sysadmin, and the operator add, change, and
  remove each event of the project.
- A worker reads only. A lead can grant a worker the write right
  (`terrarium calendar grant <worker>`). With the grant, the worker adds
  events, and changes and removes only the events that it made.
- Only a sysadmin or the operator writes the global calendar.

When you have no write right, the daemon refuses with exit code 3. Do not
try again. Ask your lead by mail, and give all the facts of the event:

```
terrarium mail send lead -s "Calendar: deploy window for v1.4" --body \
  "Please add a deploy window: Tue 2026-11-10 09:00-10:00 Europe/London, kind deploy,
   card TER-42, assignees: me. Reason: the release of v1.4 to production."
```

## When to read it

Run `terrarium calendar ls` before you:

- deploy, restart, or update a target or a service;
- merge to a branch (a `freeze` stops merges and deploys);
- start long work on a shared target (a `maintenance` or `window` event
  can take it away).

If an event blocks your work, do not work around it. Tell your lead, or
wait until the event ends.

```
terrarium calendar ls                     # the next 31 days: the project and the global events
terrarium calendar ls --days 7            # the next 7 days
terrarium calendar ls --from 2026-12-01 --to 2026-12-31
terrarium calendar ls --global            # the global calendar only
terrarium calendar show <id>              # one event: times, repeat, assignees, links, description
```

## Add an event (a lead, or a worker with the grant)

```
terrarium calendar add "DB upgrade" --kind maintenance \
  --start 2026-11-10T22:00 --end 2026-11-10T23:30 --tz Australia/Brisbane \
  --target db-host --card TER-42 --assign lead --assign ops-worker \
  --description "Upgrade PostgreSQL 16 to 17. The API is down for about 20 minutes."
terrarium calendar add "Release freeze" --kind freeze --start 2026-12-20 --end 2026-12-24
```

- Kinds: `maintenance`, `window`, `freeze`, `deploy`, `task`, `other`.
- Times: a local time in `--tz` (`2026-11-10T22:00`), RFC 3339
  (`2026-11-10T12:00:00Z`), or a date for an all-day event. The `--end`
  of an all-day event is its last day. No `--end`: one hour, or one day.
- Always give `--tz` for a local time. The default zone is UTC.
- `--assign <agent>`: the agent gets a mail when each occurrence starts
  and when it ends. Assign the agents that do the work.
- Links: `--card`, `--target`, `--repo`, `--link-url`.
- The description is markdown, for humans. Say what happens, what is
  down, and who to ask.

## Repeats

```
terrarium calendar add "Patch window" --kind maintenance --start 2026-11-10T22:00 \
  --end 2026-11-10T23:30 --tz Australia/Brisbane --repeat weekly --on tu,th
terrarium calendar add "Dependency review" --kind task --start 2026-11-02T10:00 \
  --repeat monthly --month-day 2 --count 6
terrarium calendar add "Backups check" --start 2026-11-10T08:00 --repeat "FREQ=DAILY;INTERVAL=2;UNTIL=20261231"
```

- `--repeat daily|weekly|monthly` with `--interval N`, `--on mo,tu,...`
  (weekly), `--month-day N` (monthly; `-1` is the last day), and an end:
  `--until <date>` or `--count N`.
- An RRULE text works too. Only `FREQ` (DAILY, WEEKLY, MONTHLY),
  `INTERVAL`, `BYDAY` (two-letter days), `BYMONTHDAY` (one day), `UNTIL`,
  and `COUNT`. The daemon refuses the other parts with a message.
- The repeat keeps the local time across DST changes.
- Skip one occurrence: `terrarium calendar skip <id> 2026-11-17`.
  Undo: `terrarium calendar skip <id> 2026-11-17 --undo`.

## Change and remove

```
terrarium calendar edit <id> --start 2026-11-11T22:00     # a timed event keeps its length
terrarium calendar edit <id> --title "DB upgrade (v17.1)" --kind maintenance
terrarium calendar edit <id> --no-repeat                  # stop the repeat
terrarium calendar assign <id> ops-worker
terrarium calendar unassign <id> ops-worker
terrarium calendar rm <id>
```

If an event starts while you change it, the assignees get no second
start notice. If your change moves the event away from now, the
assignees get the end notice at once.

## Notices

- An assignee gets a mail from `system` (`[Mailbox:system]`) when each
  occurrence starts and ends. The mail is not an order: do the work that
  the card or your lead gave you. A start mail that says "late notice"
  came after a daemon restart.
- `terrarium calendar watch`: a mail for each add, change, and removal in
  the project calendar. `--global` for the global calendar. Leads watch
  their project calendar. `terrarium calendar unwatch` stops it.
- The events `calendar.event_created`, `calendar.event_updated`,
  `calendar.event_deleted`, `calendar.event_started`, and
  `calendar.event_ended` go on the event log. A trigger (`terrarium
  schedule new --on calendar.event_started`) can act on them.

## For leads

- Give the write right only to a worker that plans windows:
  `terrarium calendar grant <worker>`; take it back with
  `terrarium calendar revoke <worker>`. `terrarium calendar grants` lists
  them.
- When a worker asks you to add an event, check the facts, then add it
  with the worker as an assignee.
- Watch the calendar of your project (`terrarium calendar watch`), so you
  see the events that granted workers add.

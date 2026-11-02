---
name: terrarium-monitors
description: Add monitors that prove a service is healthy (`terrarium monitor new`), put them on the project dashboard, and read the latest values with `terrarium dashboard show`. Use it when you deploy or run a service on a target, or before you report that a service works.
metadata:
  terrarium-default: "true"
---

# Monitors in Terrarium

A monitor is a shell script that Terrarium runs on a target on a schedule.
The script prints a value or exits with a status code. Terrarium keeps each
result. The project dashboard shows the monitors as blocks: a big value, a
status, a chart, text, or a table. The operator sees the dashboard first
when they open the project.

A monitor is proof that keeps working. A comment that says "the service
is up" is true once. A monitor shows the operator, every minute, that the
service is still up.

## Who can do what

- Every agent of the project reads the monitors, the results, and the
  dashboard.
- A project admin (a lead), a sysadmin, and the operator make, change,
  run, and delete monitors, and change the dashboard layout.
- The target must be in your `policy.targets`, or you must hold a lease on
  it. The target is a global target or a target of your project.
- Each run uses your rights again. When you lose the target, the monitor
  shows `unknown` with the reason.

A worker that wants a monitor asks its lead by mail, with the script.

## Read the dashboard

```sh
terrarium dashboard show            # each monitor: status, value, message, time
terrarium dashboard show --json     # the layout and every monitor with its last result
terrarium monitor ls                # the monitors with the last status and value
terrarium monitor show disk-root    # one monitor, its script, and its last result
terrarium monitor results disk-root --since 24h --bucket 1h   # the history
```

Read the dashboard before you start work on a service, and after a deploy.
A `crit` or `unknown` monitor is a fact to report, not to hide.

## Add a monitor

Pick the output kind that fits the check:

| Kind | The script gives | The status comes from |
|---|---|---|
| `number` | A number on stdout (`42`, `85%`, `1,234.5`). The first number counts. | `--warn` and `--crit`, with `--direction above` (default) or `below` |
| `status` | An exit code. The first line of stdout is the message. | 0 ok, 1 warn, 2 crit, any other code unknown |
| `text` | Text on stdout (at most 8 KiB) | The exit code, as for `status` |
| `json` | One JSON object: `{"value", "unit", "status", "message", "rows": [{...}]}` | `status` if given, else the thresholds, else the exit code |

```sh
# A number with thresholds: the root disk is warn at 80 %, crit at 90 %.
terrarium monitor new disk-root --target build-box --kind number --unit % \
  --warn 80 --crit 90 --every 5m \
  --script "df -P / | awk 'NR==2 {print \$5}'"

# A status check: the service answers, or the monitor is critical.
terrarium monitor new api-health --target build-box --kind status --every 30s \
  --script 'code=$(curl -s -o /dev/null -m 10 -w "%{http_code}" http://localhost:3000/health)
echo "HTTP $code"
[ "$code" = 200 ] || exit 2'

# Free space that is bad when low: direction below.
terrarium monitor new free-gb --target build-box --kind number --unit GB \
  --warn 20 --crit 5 --direction below --every 10m \
  --script "df -BG --output=avail / | tail -1"

# A long script from a file, on a cron spec.
terrarium monitor new nightly-backup --target build-box --kind status \
  --cron "30 3 * * *" --script-file checks/backup.sh
```

- `--every` takes `15s` or more (`30s`, `1m`, `5m`, `1h`). `--cron` takes
  five fields; `--tz` sets the zone (default UTC).
- `--timeout` kills the script after that many seconds (default 30, at
  most 600). A run past the timeout is `unknown`.
- `--retention` keeps the results that many days (default 7, at most 90).
- `--disabled` makes the monitor off; `terrarium monitor edit <name>
  --enable` turns it on.
- Run it once and read the output before you trust it:
  `terrarium monitor run disk-root` prints the result, the exit code,
  stdout, and stderr.

Write scripts that are fast, read-only, and quiet: a monitor runs often.
Do not start, stop, or change anything in a monitor script. Do not print
secrets: Terrarium replaces a known secret value with `[secret:<name>]`,
but an unknown one stays.

A monitor does not bring a stopped docker or vmlab target up. The run is
`unknown` with "target down". Watch services on targets that stay up.

## What to monitor

Prove the thing that the operator cares about, not the thing that is easy
to measure:

- The service answers: an HTTP check of its health URL (`status`).
- The service is correct: a smoke query that checks the answer
  (`status`, exit 2 on a wrong answer).
- The resources: disk, memory, load, queue length (`number` with
  thresholds).
- The jobs ran: the age of the last backup or the last import in hours
  (`number`, `--direction above`).
- A list for context: the top processes, the failing jobs (`json` with
  `rows`, shown as a table).

## Put it on the dashboard

The layout is JSON: `{"blocks": [...]}`. Each block has an `id`, a `kind`
(`stat`, `status`, `chart`, `text`, `table`, or `note`), a `monitor` (the
monitor ID; not for a note), a `size` (`s` one column, `m` two, `l` the
full width), an optional `title`, and `options`: `range` of a chart (`1h`,
`6h`, `24h`, `7d`), `sparkline` of a stat (`true` by default), and
`decimals` (0-6). A note has Markdown `text`: use it as a section header.

```sh
terrarium dashboard show --json > layout.json   # edit: keep the blocks, drop the monitors
terrarium dashboard set --from-file layout.json
```

`stat` and `chart` blocks need a `number` or `json` monitor; a `table`
block needs a `json` monitor. Several blocks can show the same monitor:
a stat and a chart of the load, for example. The save replaces the whole
layout, so read it first and keep the blocks of others.

## Alerts

Each result is a `monitor.result` event, and each change of status is a
`monitor.status` event (`data.from`, `data.to`, `data.monitor`,
`data.value`, `data.message`). A trigger turns them into mail:

```sh
terrarium schedule new disk-alert --on monitor.status \
  --match data.monitor=disk-root --match data.to=crit \
  --mail lead -s "disk-root is critical" --body "{{event.summary}}"
```

## Change and remove

```sh
terrarium monitor edit disk-root --warn 85 --crit 95
terrarium monitor edit disk-root --clear-thresholds
terrarium monitor edit disk-root --disabled
terrarium monitor rm disk-root          # also removes its dashboard blocks
```

A change of the script or the target makes you the actor whose rights the
runs use.

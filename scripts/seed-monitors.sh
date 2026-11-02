#!/usr/bin/env bash
# Make example monitors and a dashboard for a project (docs/api.md §30):
# the load average, the memory used, the root disk used, the uptime, the
# docker service, the running containers, an HTTP check of the dashboard,
# and the top 5 processes by CPU. Then a dashboard layout with note
# headers and a block of each kind (stat, status, chart, text, table).
#
# Usage: scripts/seed-monitors.sh [project] [target]
#
#   project  default `demo`
#   target   default: the first `host` target that the project may use
#            (a global one, or one of the project)
#
# Set `DASH_URL` for the URL of the HTTP check (default
# http://localhost:8080/, as the host target sees it).
#
# The script uses only the `terrarium` CLI. By default it runs the CLI in
# the Docker stack: `docker compose exec -T terrarium terrarium`. Set `T`
# to use another command, for example `T=terrarium` for a local daemon.
#
# The script is idempotent: it skips a monitor when the project has a
# monitor with the same name. It writes the dashboard layout only when
# the dashboard has no blocks (set `FORCE_LAYOUT=1` to replace it). It
# makes the project when it is missing. It needs python3 to read the JSON
# answers.

set -euo pipefail

P="${1:-demo}"
TARGET="${2:-}"
DASH_URL="${DASH_URL:-http://localhost:8080/}"
T="${T:-docker compose exec -T terrarium terrarium}"
read -r -a TCMD <<<"$T"

# `docker compose` needs the dir of compose.yaml.
cd "$(dirname "$0")/.."

t() { "${TCMD[@]}" -p "$P" "$@"; }
tj() { "${TCMD[@]}" -p "$P" --json "$@"; }
quiet() { "$@" >/dev/null 2>&1; }

# json <python expression of `d`>: read JSON on stdin, print the result.
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

if ! quiet "${TCMD[@]}" project show "$P"; then
  echo "making project $P"
  "${TCMD[@]}" project new "$P" -d "Demo project" --prefix DEMO >/dev/null
fi

# ---- the target ----

if [ -z "$TARGET" ]; then
  TARGET=$(tj targets ls | json "next((x['name'] for x in (d if isinstance(d, list) else []) if x.get('kind') == 'host' and x.get('project') in (None, '$P')), '')")
  if [ -z "$TARGET" ]; then
    echo "no host target that project $P may use; give one: $0 $P <target>" >&2
    exit 1
  fi
  echo "picked target $TARGET (the first host target of project $P or global)"
else
  echo "using target $TARGET"
fi

# ---- the monitors ----

EXISTING=$(tj monitor ls | json "'\n'.join(m['name'] for m in d)")

# monitor <name> <flags...>: make the monitor unless one has the name.
monitor() {
  local name="$1"
  shift
  if grep -qx -- "$name" <<<"$EXISTING"; then
    echo "skip $name: it exists"
    return
  fi
  t monitor new "$name" --target "$TARGET" "$@"
}

monitor load --kind number --every 30s --warn 4 --crit 8 \
  --description "The 1-minute load average." \
  --script "cut -d' ' -f1 /proc/loadavg"

monitor memory --kind number --unit % --every 1m --warn 80 --crit 90 \
  --description "Memory in use (total less available)." \
  --script "awk '/^MemTotal:/ {t=\$2} /^MemAvailable:/ {a=\$2} END {printf \"%.1f\\n\", (t-a)/t*100}' /proc/meminfo"

monitor disk-root --kind number --unit % --every 5m --warn 80 --crit 90 \
  --description "Space used on the root file system." \
  --script "df -P / | awk 'NR==2 {print \$5}'"

monitor uptime --kind text --every 5m \
  --description "The output of uptime." \
  --script "uptime"

monitor docker --kind status --every 1m \
  --description "The docker service runs (exit 0), or not (exit 2)." \
  --script 'if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet docker; then
  echo "docker is active"
elif docker info >/dev/null 2>&1; then
  echo "docker answers"
else
  echo "docker is not running"
  exit 2
fi'

monitor containers --kind number --every 1m --warn 30 --crit 50 \
  --description "Running docker containers." \
  --script "docker ps -q 2>/dev/null | wc -l"

monitor dashboard-http --kind status --every 30s \
  --description "An HTTP check of the dashboard: 2xx or 3xx is ok, no answer is critical." \
  --script "code=\$(curl -s -o /dev/null -m 10 -w '%{http_code}' '$DASH_URL')
echo \"HTTP \$code from $DASH_URL\"
case \"\$code\" in
  2*|3*) exit 0 ;;
  000) exit 2 ;;
  *) exit 1 ;;
esac"

monitor top-cpu --kind json --every 1m \
  --description "The top 5 processes by CPU, as table rows." \
  --script "ps -eo pid=,pcpu=,pmem=,comm= --sort=-pcpu | head -5 | awk '
BEGIN { printf \"{\\\"status\\\":\\\"ok\\\",\\\"rows\\\":[\" }
{ gsub(/[\"\\\\\\\\]/, \"\", \$4); if (NR > 1) printf \",\"; printf \"{\\\"pid\\\":%s,\\\"cpu\\\":%s,\\\"mem\\\":%s,\\\"cmd\\\":\\\"%s\\\"}\", \$1, \$2, \$3, \$4 }
END { print \"]}\" }'"

# ---- the dashboard ----

BLOCKS=$(tj dashboard show | json "len(d['blocks'])")
if [ "$BLOCKS" != "0" ] && [ "${FORCE_LAYOUT:-0}" != "1" ]; then
  echo "skip the layout: the dashboard has $BLOCKS blocks (FORCE_LAYOUT=1 replaces it)"
  exit 0
fi

LAYOUT=$(tj monitor ls | TARGET="$TARGET" python3 -c '
import json, os, sys
ids = {m["name"]: m["id"] for m in json.load(sys.stdin)}
target = os.environ["TARGET"]
def block(bid, kind, name, size="s", title=None, **options):
    if name not in ids:
        return None
    b = {"id": bid, "kind": kind, "monitor": ids[name], "size": size, "options": options}
    if title:
        b["title"] = title
    return b
def note(bid, text):
    return {"id": bid, "kind": "note", "size": "l", "text": text, "options": {}}
blocks = [
    note("head-host", "## Host health\nThe host target `%s`. Load warns at 4, memory and disk at 80 %%." % target),
    block("load", "stat", "load", decimals=2),
    block("memory", "stat", "memory", decimals=1),
    block("disk", "stat", "disk-root", title="Root disk used", decimals=0),
    block("docker", "status", "docker", title="Docker service"),
    block("load-chart", "chart", "load", "m", "Load average", range="24h"),
    block("memory-chart", "chart", "memory", "m", "Memory used", range="6h"),
    note("head-svc", "## Services\nA status check exits with 0 (ok), 1 (warning), or 2 (critical)."),
    block("http", "status", "dashboard-http", title="Dashboard HTTP"),
    block("containers", "stat", "containers", title="Running containers", decimals=0),
    block("procs", "table", "top-cpu", "m", "Top 5 processes by CPU"),
    block("uptime", "text", "uptime", "l"),
]
print(json.dumps({"blocks": [b for b in blocks if b]}))
')
printf '%s\n' "$LAYOUT" | t dashboard set --from-file -
echo "done: open the Dashboard of project $P, or run: terrarium -p $P dashboard show"

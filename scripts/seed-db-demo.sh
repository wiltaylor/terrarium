#!/usr/bin/env bash
# Seed the DB of a project with example data (PLAN §15.4), and write the
# wiki page `db-examples` that shows each view of a `db` block.
#
# Usage: scripts/seed-db-demo.sh [project]        (default project: demo)
#
# The script uses only the `terrarium` CLI. By default it runs the CLI in
# the Docker stack: `docker compose exec -T terrarium terrarium`. Set `T`
# to use another command, for example `T=terrarium` for a local daemon.
#
# The script is idempotent: it skips a type that exists (with its
# records), and the wiki page when it exists. It makes the project when
# it is missing. The data is the same at each run (a fixed random seed).

set -euo pipefail

P="${1:-demo}"
T="${T:-docker compose exec -T terrarium terrarium}"
read -r -a TCMD <<<"$T"

# `docker compose` needs the dir of compose.yaml.
cd "$(dirname "$0")/.."

t() { "${TCMD[@]}" -p "$P" "$@"; }
quiet() { "$@" >/dev/null 2>&1; }

# A fixed seed: the same "random" values at each run.
RANDOM=4242

now=$(date -u +%s)
day() { date -u -d "@$((now - $1 * 86400))" +%F; }
at() { date -u -d "@$((now - $1))" +%Y-%m-%dT%H:%M:%SZ; }
# pick <values...>: one of the values in $PICKED. No subshell, so the
# seed sequence of RANDOM stays the same at each run.
pick() {
  local a=("$@")
  PICKED=${a[RANDOM % ${#a[@]}]}
}

if ! quiet "${TCMD[@]}" project show "$P"; then
  echo "making project $P"
  "${TCMD[@]}" project new "$P" -d "Demo project" --prefix DEMO >/dev/null
fi

# new_type <name> <args...>: make the type, or say that it exists.
# Returns 1 when the type existed, so the caller skips its records.
new_type() {
  local name=$1
  shift
  if quiet t db type show "$name"; then
    echo "type $name exists: skip it and its records"
    return 1
  fi
  t db type new "$name" "$@" >/dev/null
  echo "made type $name"
}

rec() { t db rec new "$@" >/dev/null; }

# ---- service ----
SERVICES=(
  "auth-service|identity|rust|8080|true|https://auth.internal.example.com|auth,core"
  "billing-api|payments|go|8081|true|https://billing.internal.example.com|payments,core"
  "search-indexer|search|rust|9200|true|https://search.internal.example.com|search"
  "web-frontend|web|typescript|3000|true|https://app.example.com|ui"
  "notification-worker|platform|python|8090|false|https://notify.internal.example.com|queue,email"
  "payments-gateway|payments|go|8443|true|https://pay.internal.example.com|payments,pci"
  "user-profile|identity|typescript|8082|true|https://profile.internal.example.com|users"
  "media-transcoder|media|rust|8095|true|https://media.internal.example.com|video,batch"
  "analytics-pipeline|data|python|8100|false|https://analytics.internal.example.com|data,batch"
  "api-gateway|platform|go|443|true|https://api.example.com|edge,core"
  "feature-flags|platform|typescript|8084|true|https://flags.internal.example.com|config"
  "audit-log|security|rust|8086|true|https://audit.internal.example.com|security,compliance"
)
if new_type service -d "The services that we run" \
  --field 'name:string!' --field 'owner:string' \
  --field 'language:enum(rust,go,python,typescript,java)' \
  --field 'port:integer' --field 'healthy:bool=true' --field 'url:string' \
  --field 'tags:list(string)' --unique name; then
  for s in "${SERVICES[@]}"; do
    IFS='|' read -r name owner lang port healthy url tags <<<"$s"
    rec service --set "name=$name" --set "owner=$owner" --set "language=$lang" \
      --set "port=$port" --set "healthy=$healthy" --set "url=$url" --set "tags=$tags"
  done
  echo "  ${#SERVICES[@]} services"
fi

# ---- contact ----
CONTACTS=(
  "Ana Silva|identity|Tech lead|ana.silva@example.com|Europe/Lisbon"
  "Ben Okafor|payments|Engineer|ben.okafor@example.com|Africa/Lagos"
  "Chen Wei|search|Staff engineer|chen.wei@example.com|Asia/Shanghai"
  "Dana Kowalski|web|Engineer|dana.kowalski@example.com|Europe/Warsaw"
  "Eli Cohen|platform|SRE|eli.cohen@example.com|Asia/Jerusalem"
  "Fatima Haddad|payments|Engineering manager|fatima.haddad@example.com|Asia/Dubai"
  "Gus Andersen|media|Engineer|gus.andersen@example.com|Europe/Copenhagen"
  "Hana Sato|data|Data engineer|hana.sato@example.com|Asia/Tokyo"
  "Ivan Petrov|platform|SRE|ivan.petrov@example.com|Europe/Sofia"
  "Jade Moreau|security|Security engineer|jade.moreau@example.com|Europe/Paris"
  "Kofi Mensah|identity|Engineer|kofi.mensah@example.com|Africa/Accra"
  "Lena Fischer|web|Designer|lena.fischer@example.com|Europe/Berlin"
  "Marco Rossi|data|Analyst|marco.rossi@example.com|Europe/Rome"
  "Nina Larsen|platform|Tech lead|nina.larsen@example.com|Europe/Oslo"
  "Omar Farouk|payments|Engineer|omar.farouk@example.com|Africa/Cairo"
  "Priya Nair|search|Engineer|priya.nair@example.com|Asia/Kolkata"
  "Quinn Murphy|web|Engineer|quinn.murphy@example.com|Europe/Dublin"
  "Rosa Martinez|security|Engineering manager|rosa.martinez@example.com|America/Mexico_City"
  "Sam Taylor|media|Product manager|sam.taylor@example.com|Australia/Sydney"
  "Tomas Novak|data|Engineer|tomas.novak@example.com|Europe/Prague"
)
if new_type contact -d "The people to ask about a service or an area" \
  --field 'name:string!' --field 'team:string' --field 'role:string' \
  --field 'email:string' --field 'timezone:string'; then
  for c in "${CONTACTS[@]}"; do
    IFS='|' read -r name team role email tz <<<"$c"
    rec contact --set "name=$name" --set "team=$team" --set "role=$role" \
      --set "email=$email" --set "timezone=$tz"
  done
  echo "  ${#CONTACTS[@]} contacts"
fi

# ---- bug ----
# title|component|service
BUGS=(
  "Login fails on Safari after a password reset|login|auth-service"
  "Session cookie has no SameSite flag|session|auth-service"
  "Token refresh races with logout|session|auth-service"
  "MFA code input drops the first digit on Android|login|auth-service"
  "Invoice PDF shows the wrong currency symbol|invoices|billing-api"
  "Proration is off by one day at month end|invoices|billing-api"
  "Duplicate invoice when the webhook retries|webhooks|billing-api"
  "Tax rate cache never expires|tax|billing-api"
  "Reindex job stalls at 98 percent|indexer|search-indexer"
  "Search returns deleted documents for 5 minutes|indexer|search-indexer"
  "Accented characters do not match in queries|query|search-indexer"
  "Facet counts are wrong after a partial update|query|search-indexer"
  "Dark mode toggle does not persist|settings|web-frontend"
  "Table header overlaps rows on narrow screens|layout|web-frontend"
  "File upload progress bar jumps to 100 percent|upload|web-frontend"
  "Date picker shows UTC instead of local time|forms|web-frontend"
  "Keyboard focus is lost after closing a dialog|a11y|web-frontend"
  "Email digest is sent twice on DST change|scheduler|notification-worker"
  "Push notifications fail for long titles|push|notification-worker"
  "Unsubscribe link returns 500|email|notification-worker"
  "Retry queue grows without bound when SMTP is down|queue|notification-worker"
  "3-D Secure challenge times out on slow networks|checkout|payments-gateway"
  "Refund webhook is not idempotent|webhooks|payments-gateway"
  "Card brand icon is missing for UnionPay|checkout|payments-gateway"
  "Decline reason is not shown to the customer|checkout|payments-gateway"
  "Avatar upload accepts SVG with scripts|avatars|user-profile"
  "Profile page loads all activity at once|profile|user-profile"
  "Display name allows zero-width characters|profile|user-profile"
  "Transcoding fails for HDR input|transcode|media-transcoder"
  "Thumbnails are generated twice|thumbnails|media-transcoder"
  "Audio drifts out of sync after 40 minutes|transcode|media-transcoder"
  "Worker leaks temp files on cancel|workers|media-transcoder"
  "Daily rollup double counts late events|rollups|analytics-pipeline"
  "Schema drift breaks the export job|export|analytics-pipeline"
  "Backfill ignores the time zone of the project|rollups|analytics-pipeline"
  "Dashboard query times out for 90-day ranges|queries|analytics-pipeline"
  "Rate limiter counts OPTIONS requests|rate-limit|api-gateway"
  "Gateway returns 502 during config reload|routing|api-gateway"
  "CORS header missing on 401 responses|cors|api-gateway"
  "Request ID is not passed to upstreams|tracing|api-gateway"
  "Large headers cause a 431 on some routes|routing|api-gateway"
  "Flag change takes 2 minutes to apply|sync|feature-flags"
  "Percentage rollout is not sticky per user|targeting|feature-flags"
  "Archived flags still show in the SDK payload|sdk|feature-flags"
  "Audit export misses events at the hour boundary|export|audit-log"
  "Audit search is case sensitive|search|audit-log"
  "Retention job deletes records one day early|retention|audit-log"
  "Login page leaks the user list through timing|login|auth-service"
  "Password policy error text is not translated|login|auth-service"
  "Credit note total ignores discounts|invoices|billing-api"
  "Search suggestions flicker while typing|query|search-indexer"
  "Breadcrumbs show raw IDs|layout|web-frontend"
  "SMS fallback sends to the old number|sms|notification-worker"
  "Payout report rounds amounts down|reports|payments-gateway"
  "Email change does not confirm the new address|profile|user-profile"
  "Subtitles lose styling after transcode|transcode|media-transcoder"
  "Funnel report drops users with no country|reports|analytics-pipeline"
  "Health check hits the database on every call|health|api-gateway"
  "SDK falls back to defaults on a slow network|sdk|feature-flags"
  "Audit entries show the wrong actor for API keys|ingest|audit-log"
)
ASSIGNEES=("Ana Silva" "Ben Okafor" "Chen Wei" "Dana Kowalski" "Eli Cohen" "Gus Andersen" "Hana Sato" "Ivan Petrov" "Jade Moreau" "Priya Nair")
if new_type bug -d "Defects found in the services" \
  --field 'title:string!' \
  --field 'severity:enum(low,medium,high,critical)!' \
  --field 'status:enum(open,in_progress,fixed,wontfix)!=open' \
  --field 'component:string' --field 'service:ref(service)' \
  --field 'reported:date!' --field 'fixed:date' --field 'assignee:string' \
  --field 'notes:text' --field 'tags:list(string)'; then
  i=0
  for b in "${BUGS[@]}"; do
    IFS='|' read -r title comp svc <<<"$b"
    i=$((i + 1))
    pick low medium medium high high critical
    sev=$PICKED
    pick open open open in_progress fixed fixed wontfix
    st=$PICKED
    ago=$((RANDOM % 60 + 1))
    args=(--set "title=$title" --set "severity=$sev" --set "status=$st"
      --set "component=$comp" --set "service=$svc" --set "reported=$(day "$ago")")
    if [[ $st == fixed ]]; then
      args+=(--set "fixed=$(day $((ago - ago / 3)))")
    fi
    if [[ $st != open ]] || ((i % 3 == 0)); then
      pick "${ASSIGNEES[@]}"
      args+=(--set "assignee=$PICKED")
    fi
    case $sev in
      critical) args+=(--set "tags=$comp,customer-facing,p0") ;;
      high) args+=(--set "tags=$comp,customer-facing") ;;
      *) args+=(--set "tags=$comp") ;;
    esac
    if ((i % 4 == 1)); then
      args+=(--set "notes=## Steps

1. Open the $comp area of $svc.
2. Repeat the action from the title.

**Expected:** no error. **Seen:** the problem in the title.")
    fi
    rec bug "${args[@]}"
  done
  echo "  ${#BUGS[@]} bugs"
fi

# ---- decision ----
# title|status|days ago|context|decision|consequences
DECISIONS=(
  "Use PostgreSQL for the billing ledger|accepted|300|The ledger needs transactions across tables and strong consistency.|Keep the ledger in PostgreSQL 16 with serializable transactions.|Ops runs one more database; reports read from a replica."
  "Move session storage to Redis|accepted|240|Sessions in the database cause lock contention at peak.|Store sessions in Redis with a 30-day TTL.|A Redis outage logs everyone out; we accept that risk."
  "Adopt OpenTelemetry for tracing|accepted|210|Three tracing libraries are in use, and no trace crosses services.|Use the OpenTelemetry SDKs and one collector per cluster.|Each service must pass the trace context; the gateway adds it."
  "Write new services in Rust or Go only|accepted|200|Six languages make on-call hard.|New services use Rust or Go; TypeScript stays for the web.|Python services stay until their next rewrite."
  "Keep feature flags in a service of our own|superseded|190|The vendor price went up by 4 times.|Build feature-flags in house.|Superseded by the decision to evaluate flags at the edge."
  "Evaluate feature flags at the edge|accepted|120|The SDK round trip adds 40 ms to each page.|The api-gateway evaluates flags and passes them in a header.|Flag rules must be fast and have no network calls."
  "Use GraphQL for the public API|rejected|180|Some customers asked for flexible queries.|Keep the REST API; add field selection with ?fields=.|No second API to version and secure."
  "Run search on managed Elasticsearch|superseded|170|We do not want to run a search cluster.|Use the managed service of the cloud provider.|Superseded: the cost at our volume was too high."
  "Run search on our own Tantivy indexer|accepted|90|The managed search cost tripled in a year.|search-indexer uses Tantivy on our own nodes.|We own reindexing and the shard layout."
  "Weekly release train for the web app|accepted|150|Releases at random times broke the support docs.|Release web-frontend every Tuesday; hotfixes any day.|Features wait up to a week behind a flag."
  "Require two reviewers for payments code|accepted|140|A rounding bug reached production in March.|Changes under payments-gateway need two approvals.|Reviews take longer; we added a reviewer rota."
  "Store audit logs for 7 years|proposed|20|The new compliance rules ask for 7 years.|Keep audit-log data 7 years in cold storage after 1 year.|Storage cost goes up; the retention job needs a new tier."
  "Drop Internet Explorer 11 support|accepted|260|Under 0.1 percent of sessions use it.|Stop testing on IE 11; show a notice.|We can use modern CSS and JavaScript."
  "Use server-sent events for live updates|proposed|12|Polling every 5 s loads the API.|Push changes to the web app with SSE through the gateway.|The gateway must hold many open connections."
  "Shard analytics by project|rejected|60|Large projects slow the rollups of the others.|Keep one store; add per-project quotas instead.|Quotas need a UI; no shard migration needed."
)
if new_type decision -d "Architecture and process decisions (ADRs)" \
  --field 'title:string!' \
  --field 'status:enum(proposed,accepted,superseded,rejected)!' \
  --field 'date:date!' --field 'context:text' --field 'decision:text!' \
  --field 'consequences:text'; then
  for d in "${DECISIONS[@]}"; do
    IFS='|' read -r title st ago ctx dec cons <<<"$d"
    rec decision --set "title=$title" --set "status=$st" --set "date=$(day "$ago")" \
      --set "context=$ctx" --set "decision=$dec" --set "consequences=$cons"
  done
  echo "  ${#DECISIONS[@]} decisions"
fi

# ---- benchmark ----
# service|latency ms|throughput rps|memory mb
BASES=(
  "auth-service|12|4200|180"
  "billing-api|35|1500|420"
  "search-indexer|48|900|2600"
  "api-gateway|6|9800|260"
  "payments-gateway|41|1100|380"
)
if new_type benchmark -d "Benchmark results from the nightly load tests" \
  --field 'name:string!' --field 'service:ref(service)' \
  --field 'metric:enum(latency_ms,throughput_rps,memory_mb)!' \
  --field 'value:number!' --field 'run_at:datetime!' --field 'commit:string' \
  --field 'notes:text'; then
  n=0
  for b in "${BASES[@]}"; do
    IFS='|' read -r svc lat thr mem <<<"$b"
    for run in 0 1 2 3 4 5 6 7; do
      # Eight runs over 30 days, the newest first.
      secs=$((run * 30 * 86400 / 8 + (RANDOM % 7200)))
      c=$((RANDOM * 32768 + RANDOM))
      printf -v commit '%07x' "$c"
      for metric in latency_ms throughput_rps memory_mb; do
        case $metric in
          latency_ms) base=$lat ;;
          throughput_rps) base=$thr ;;
          memory_mb) base=$mem ;;
        esac
        # +/- 10 percent, with one decimal.
        jitter=$((RANDOM % 201 - 100))
        value=$(awk -v b="$base" -v j="$jitter" 'BEGIN { printf "%.1f", b * (1 + j / 1000) }')
        args=(--set "name=nightly-$svc-$metric" --set "service=$svc" --set "metric=$metric"
          --set "value=$value" --set "run_at=$(at "$secs")" --set "commit=$commit")
        if ((jitter > 90)); then
          args+=(--set "notes=Outlier: the run shared the node with a reindex job.")
        fi
        rec benchmark "${args[@]}"
        n=$((n + 1))
      done
    done
  done
  echo "  $n benchmark results"
fi

# ---- the wiki page ----
if quiet t wiki show db-examples; then
  echo "wiki page db-examples exists: skip it"
else
  t wiki new db-examples -m "Examples of db blocks" --body - >/dev/null <<'EOF'
# DB examples

Each block below is a query of the project DB. The page shows the
result when you open it. Edit the page to see the queries.

## Open critical bugs

```db
type:bug status=open severity=critical sort:-reported fields:title,service,component,reported
```

## Open bugs

```db
type:bug status=open count:true view:count
```

## Bugs per component

```db
type:bug group:component view:group limit:15
```

## Open bugs per severity

```db
type:bug status!=fixed status!=wontfix group:severity view:group
```

## Latest benchmark results

```db
type:benchmark sort:-run_at limit:15 fields:service,metric,value,run_at,commit
```

## Latency of auth-service in the last 30 days

```db
type:benchmark service=auth-service metric=latency_ms run_at>-30d sort:-run_at fields:run_at,value,commit
```

## Accepted decisions

```db
type:decision status=accepted sort:-date view:list
```

## Unhealthy services

```db
type:service healthy=false fields:name,owner,language,url
```
EOF
  echo "made wiki page db-examples"
fi

echo "done: project $P"

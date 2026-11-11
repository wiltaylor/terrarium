# TER-870 write profile

F1: Enable `terrariumd::db_timing=debug` to record each write's static
`caller_file` and `caller_line`, `success`, blocking scheduling, mutex wait,
and lock-held service (`sql_us`, including transaction setup and commit).
Ordinary transaction errors include rollback time. Panics and cancelled
blocking tasks are not covered. Reads retain aggregate timing only.
Disabled profiling performs one enabled check, no profiling allocation,
clock read or additional lock. Labels contain no SQL, actor or parameters.

F2: Probe 1 on terrarium-helios-build2, commit 581db5a4, used the ignored
`it::ter221_latency::latency_under_load` test, 80 sequential pings, eight
matching and two unmatched writers, and 24 CPU hogs. All four card-check
locks were held in fixed order for the probe under `ionice -c3 nice -n19`.
The host exposed 32 CPUs, so affinity was restricted to 0-23. This is not
the same host configuration as TER-285's 24-CPU baseline. The profile covers
setup and drain too, not only the measured ping window.

| Caller | Count | Total service ms | p90 scheduling / wait / service us |
| --- | ---: | ---: | --- |
| latency test event emit, ter221_latency.rs:26 | 4956 | 23697.0 | 242 / 103654 / 6139 |
| rule action step, rules_engine.rs:1039 | 680 | 5329.8 | 1369 / 155650 / 9571 |
| rule planning, rules_engine.rs:1985 | 302 | 3594.5 | 151 / 75993 / 16349 |
| trigger planning, scheduler.rs:367 | 550 | 2573.9 | 170 / 89706 / 6093 |
| reads (aggregate) | 2797 | 382.4 | 819 / 156225 / 251 |

F3: Across 9410 DB samples, p90 scheduling / wait / service was
364 / 136308 / 6795 us (floor-index percentile). Plan latency
`event.at -> run.created` p90 was 128 ms, wall p90 631.7 ms, noise 141
events/s, backlog 7. TER-285 baseline was 165 / 629.7 ms and 143 events/s.
The new plan measurement meets 200 ms, but this is not proof of an
improvement because host state differs and no optimization was made.

D1: Ship profiling only. Event emission accounts for most service, while
rule action and planning writes have higher per-call service. These writes
include durable events, plan-time guards and outbox work. Caller totals
alone do not isolate a safe bounded change within those transactions.
No admission, ordering, lane, guard or persistence behavior is changed.
One of the three allowed load probes was used.

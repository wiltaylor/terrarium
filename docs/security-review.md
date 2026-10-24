# Terrarium security review

Read-only review before Terrarium runs with real subscriptions. Date: 2026-10-24.
Reviewer: pre-flight security pass (ai@wiltaylor.dev).

Scope: the daemon (`terrariumd`), the runner, the agent CLI, the host agent, the
adapters, the Docker image, and the dashboard. The review did not start agents and
did not use real credentials. It read the code and the plan. It did not change code.

The design is strong. The sandbox argv, the actor model, the tar handling, the
command construction on hosts, and the markdown sanitizer are careful and mostly
correct. Most findings are about integrity of local records, transport security of
the host link, and defence in depth in the dashboard. The review found no verified
remote unauthenticated path to code execution or to another agent's secrets.

---

## 1. Executive summary: the top 5 issues

1. **The sandbox binds the whole agent directory read-write.** A yolo agent can
   rewrite its own `events.jsonl`, `pty.log`, `runner.json`, and
   `runner-exit.json`. This defeats the usage budget, the audit and forensic
   record, and the resume logic. (HIGH, `crates/terrariumd/src/sandbox.rs:333`)

2. **The host agent link defaults to plaintext `ws://` with a bearer token.**
   Host agents are made for remote machines (PLAN §19). Over an untrusted network
   the token and all exec traffic travel in clear text and the token replays.
   (HIGH, `compose.yaml`, `crates/terrarium-host/src/config.rs:25`,
   `crates/terrariumd/src/main.rs:92`)

3. **The dashboard renders untrusted agent markdown with `innerHTML`.** It relies
   on a hand-written sanitizer, not a vetted one. One renderer bug gives stored
   XSS on the operator origin, where the bearer token sits in `localStorage`.
   (MEDIUM, `dashboard/src/components/Markdown.tsx:15`,
   `dashboard/src/components/ArtifactViewer.tsx:82`,
   `dashboard/src/stores/auth.ts`)

4. **The TCP bearer token travels in the `?token=` query string for WebSocket
   calls.** Query strings reach proxy logs, the daemon's own access logs, and
   browser history. (MEDIUM, `crates/terrariumd/src/api/mod.rs:207`)

5. **Artifacts mount on the dashboard port by default; the separate origin is
   optional.** The CSP `sandbox` header and the sandboxed iframe mitigate this,
   but the isolation is a config choice, not a structural one. (LOW, see §4)

---

## 2. Findings, sorted by severity

| # | Severity | Area | File:line | Summary | Status after fixes (2026-10-24) |
|---|---|---|---|---|---|
| F1 | HIGH | Sandbox | `sandbox.rs:333` | Whole agent dir bound rw; agent tampers with its own event log, pty log, and resume state | Fixed (see F11 for upgraded dirs) |
| F2 | HIGH | Host link | `compose.yaml`, `terrarium-host/src/config.rs:25` | Plaintext `ws://` host link; token and exec traffic in clear, replayable | Fixed, with residuals (§7) |
| F3 | MEDIUM | Dashboard | `components/Markdown.tsx:15` | Untrusted markdown through `innerHTML` and a hand-rolled sanitizer | Fixed; token still script-readable |
| F4 | MEDIUM | API auth | `api/mod.rs:207,256` | Bearer token in `?token=` query string for WebSockets | Fixed; `?token=` still accepted by default |
| F5 | LOW | Artifacts | `api/mod.rs:173`, `api/artifacts.rs:34` | Artifacts on the dashboard origin by default; isolation depends on CSP | Fixed |
| F6 | LOW | Login flow | `login_session.rs:789` | Login PTY output and `pty.log` in a temp dir hold the device code until cleanup | Partially fixed; the SQLite residual is fixed (a final state clears `url` and `device_code`). The `error` text residual stays. |
| F7 | LOW | API auth | `api/mod.rs:189` | Token compare returns early on a length mismatch (length leak only) | Fixed |
| F8 | INFO | SQL | `events.rs:208`, `store/usage.rs:177`, `store/mail.rs:266` | Dynamic SQL uses fixed identifiers and integers only; no injection found | Not re-checked (no change) |
| F9 | INFO | Host verbs | `terrarium-host/src/verb.rs`, `executor.rs:982` | Commands build as argv, not shell strings; no injection found | Not re-checked (no change) |
| F10 | LOW | Fleet | `fleet.rs:869,1042` | Any agent, a member too, overwrites the stored record of any fleet with an empty fleet of the same name | Fixed: an apply needs the rights over every object the stored fleet owns; a member gets 403; an empty fleet needs sysadmin; an API apply over a `file:` fleet needs `confirm` |
| F11 | LOW | Sandbox | `terrarium-runner/src/config.rs:150,181` | The split of an old agent dir follows symlinks that the agent planted before the upgrade | Fixed in the daemon (`runner_supervisor.rs`, `migrate_dir`): a dir with a symlink goes to `<ulid>.quarantine-<ts>`, and a clean dir takes its place |
| F12 | LOW | Usage | `usage_collector.rs:199,530` | The collector tails any file that a forged `SessionStart` hook names | Fixed: only a regular file under the session area of the harness, that names its session, and that no other agent owns; else a `usage.rejected_path` event |
| F13 | LOW | Policy | `policy.rs:140`, `api/agents.rs:141` | A `project_admin` adds global targets to `policy.targets`, its own too | Fixed: a global target, a `cross_project` entry, or a `mail_to` entry outside the project needs sysadmin (API and fleet) |
| F14 | INFO | Dashboard | `eslint.config.js:24`, `lib/sanitize.ts:76` | The sink lint rule has gaps; the sanitizer keeps remote images (the CSP blocks them) | New (2026-10-24) |

---

## 3. Detailed findings

### F1 — The sandbox binds the whole agent directory read-write (HIGH)

`crates/terrariumd/src/sandbox.rs:333`

```rust
push3(&mut args, "--bind", &layout.agent_dir, &layout.agent_dir);
```

The layout binds `<state>/agents/<ulid>` read-write at its own path inside the
sandbox (`runner_supervisor.rs:672`). That directory holds the runner's control
and record files, not only the harness config:

- `events.jsonl` — the event log. The daemon reads it for usage accounting and for
  resume (`runner_supervisor.rs:930`, `events_seq`). The runner appends to it
  (`terrarium-runner/src/events.rs:24`).
- `pty.log` and `pty.log.1` — the raw PTY bytes, the forensic keystroke and output
  log (`terrarium-runner/src/ptylog.rs:21`).
- `runner.json` — the full launch spec, written mode 0600 by the daemon
  (`terrarium-runner/src/spec.rs:218`). Its `launch.env` holds the injected API key
  of the agent's subscription (`runner_supervisor.rs:867`) and the full `bwrap`
  argv is in `runner-argv.json`.
- `runner-exit.json` — the last exit status. The container start reads it to decide
  resume (`docker/entrypoint.sh`, `process.rs:43`).

The harness runs in yolo mode. It can write every one of these files.

**Attack scenario.** A yolo agent, or a prompt-injected agent, rewrites its own
`events.jsonl`: it forges `TurnEnd` and usage events to stay under the per-agent
turn and time budget (PLAN §21, "Runaway cost"), or it deletes its activity so the
operator's event feed and the audit trail hide what it did. It can corrupt the file
to break resume, or write a false `runner-exit.json` to change restart behaviour.
The `runner.json` at rest also exposes the agent's own subscription key on disk;
the agent already holds that key in its process env, so this is not a new leak to
that agent, but it widens the on-disk footprint.

**Fix.** Do not bind the whole agent dir. Bind only what the harness needs:
`prompt.md` read-only, and, for pi, its per-agent `pi/` and `sessions/`
subdirectories. Move the runner's control and record files
(`events.jsonl`, `pty.log*`, `runner.json`, `runner-argv.json`, `runner-exit.json`,
`runner.log`) out of `agent_dir` into a sibling directory that the sandbox never
binds, for example `<state>/runners/<ulid>/` or the runtime dir. If a single bind
must stay for simplicity, make it read-only and give the harness a separate
writable workspace subdir.

### F2 — The host agent link defaults to plaintext ws:// (HIGH)

`compose.yaml` (ports `8082`, no TLS), `crates/terrarium-host/src/config.rs:25`
(`ws://`, `wss://`, `http://`, `https://` all allowed),
`crates/terrariumd/src/main.rs:92` (`TERRARIUM_HOST_LISTEN` default `0.0.0.0:8082`).

The daemon binds the host dial-in listener on all interfaces and terminates no TLS
itself. The host client config accepts a plain `ws://` URL. The host sends its
token as `Authorization: Bearer <token>` over that link
(`crates/terrariumd/src/hosts.rs:619`), and all exec traffic — command lines,
output, and copied file bytes — flows over it.

**Attack scenario.** A host agent runs on a remote machine or a lab network, which
is the stated purpose (PLAN §19). An attacker on that path reads the bearer token
from the first frame and replays it to register as the host, or reads the exec
output and copied files. The token is a long random value, so brute force is not
the risk; cleartext capture and replay is.

**Fix.** Require `wss://` for any non-loopback host. Document that `8082` and `8080`
must sit behind TLS (a reverse proxy, or native rustls in the daemon) when a host
agent is not on the same machine. Consider binding the host listener to loopback by
default and making the operator opt in to a public bind. Rotate the host token on
each successful connect, so a captured token has a short life (the resume token
already rotates per connect at `hosts.rs:295`; the long-lived registration token
does not).

### F3 — Untrusted markdown reaches the operator DOM through innerHTML (MEDIUM)

`dashboard/src/components/Markdown.tsx:15` (`el.innerHTML = props.html`),
`dashboard/src/components/ArtifactViewer.tsx:82`
(`<div ... innerHTML={renderMarkdown(text() ?? "")} />`),
`dashboard/src/lib/markdown.ts`.

Wiki pages and markdown artifacts come from agents, so they are untrusted. The
renderer escapes raw HTML tokens (`html(token) { return esc(token.text); }`) and
filters link schemes with `safeUrl` and `SAFE_URL`. The unit test in
`markdown.test.ts:37` covers `<script>`, `onerror`, `javascript:`, and `vbscript:`.
The sanitizer looks correct today.

The risk is the pattern, not a known bug. The dashboard writes the result straight
into the operator-origin DOM with `innerHTML`, and it keeps the bearer token in
`localStorage` (`stores/auth.ts`). A single gap in the hand-written renderer — a
marked parser change, a new token type, a mis-escaped attribute — becomes stored
XSS that reads the token and acts as the operator.

**Fix.** Run the rendered HTML through a vetted sanitizer such as DOMPurify before
`innerHTML`, as defence in depth behind the renderer. Prefer a token store that
script cannot read (an `HttpOnly` cookie on the API, or an in-memory token), so an
XSS bug cannot lift the operator token. Keep the current renderer and its tests.

### F4 — The bearer token travels in the query string for WebSockets (MEDIUM)

`crates/terrariumd/src/api/mod.rs:207` (`query_token`), `:256` (used for WS).

A browser cannot set an `Authorization` header on a WebSocket. The daemon accepts
the token as `?token=` for WebSocket upgrades. The dashboard builds that URL
(`dashboard/src/api/http.ts:162`). Query strings reach reverse-proxy access logs,
the daemon's own request logs if enabled, and browser history.

**Fix.** Pass the token as a WebSocket subprotocol value
(`Sec-WebSocket-Protocol`) instead of the query string, and read it there. If the
query parameter must stay for compatibility, make sure no layer logs request URIs,
and document the exposure.

### F5 — Artifacts mount on the dashboard origin by default (LOW)

`crates/terrariumd/src/api/mod.rs:173` (the main router merges
`artifacts::serving_routes()`), `crates/terrariumd/src/api/artifacts.rs:34`
(`ARTIFACT_CSP = "sandbox allow-scripts allow-forms allow-popups"`).

PLAN §15.2 wants a separate origin for artifacts. The separate listener
(`artifact_listen`, port 8081) is optional, and the main router also serves `/a/...`
on the dashboard port. An uploaded HTML page or bundle can therefore load from the
same origin as the dashboard.

The mitigations hold today. Every served artifact carries the CSP `sandbox`
directive without `allow-same-origin`, which forces an opaque origin, and the
dashboard embeds artifacts in an `<iframe sandbox="allow-scripts">` without
`allow-same-origin` (`ArtifactViewer.tsx`). An opaque origin cannot read the
dashboard's `localStorage` or call the API with the token. The content type is
sniffed and fixed on upload, executables download, and `X-Content-Type-Options:
nosniff` is set.

**Fix.** Make the separate artifact origin the default, or document that it is
required for a safe deployment. Keep the CSP `sandbox` header and the iframe
sandbox as they are. Consider adding a `connect-src 'none'` or a restrictive CSP to
the served artifacts, so a sandboxed script cannot exfiltrate through `fetch`.

### F6 — The login PTY output holds the device code until cleanup (LOW)

`crates/terrariumd/src/login_session.rs:789` (`start`), `:1049` (`attach`),
`:1017` (`input`).

The login flow is operator-only and well guarded. The pasted code is audited as a
character count only (`:1031`), never stored or echoed to the browser as a stored
value. The child env strips `CLAUDE*`, `ANTHROPIC*`, the API keys, and
`TERRARIUM_TOKEN` (`:256`). The temp dir is mode 0700 and is removed on finish.

The residual point: the login runner writes the raw PTY, which includes the device
code and the login URL, to a `pty.log` in the temp dir (through
`terrarium_runner::Runner`), and the daemon streams the raw PTY to the operator
browser. The code sits in those bytes until the ten-minute timeout or the cancel
removes the dir. A device code is short-lived and single-use, so the impact is low.

**Fix.** Shorten or disable the `pty.log` for login runners, and make sure the temp
dir is removed on every exit path, including a daemon crash. Confirm no login PTY
bytes land in the daemon log.

### F7 — Token compare returns early on a length mismatch (LOW)

`crates/terrariumd/src/api/mod.rs:189`.

```rust
fn token_matches(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() { return false; }
    given.iter().zip(expected).fold(0u8, |acc,(a,b)| acc | (a ^ b)) == 0
}
```

The byte compare is constant time. The length check is not, so the function leaks
the token length through timing. The token is high-entropy, so this does not help
an attacker recover it. No change needed beyond a note. If desired, hash both sides
to a fixed width before the compare.

### F8 — Dynamic SQL uses fixed identifiers only (INFO, no issue found)

`crates/terrariumd/src/events.rs:208`, `store/usage.rs:159-177`,
`store/mail.rs:244-266`, `store/memory.rs:95`, `store/board.rs:226`,
`store/targets.rs:138`, `store/hosts.rs:77-100`.

Every `format!` into SQL interpolates a fixed column-name constant, a table name
from a static map, or an integer limit. User-supplied strings always bind as
parameters (`params_from_iter`, `?` placeholders). The review found no place where
an agent-controlled string reaches a query as text. No injection.

### F9 — Host and exec commands build as argv, not shell strings (INFO, no issue found)

`crates/terrarium-host/src/verb.rs`, `docker.rs`, `vmlab.rs`,
`crates/terrariumd/src/executor.rs:982,2104`.

The docker, vmlab, and ssh verbs build an argument vector and pass it to
`Command::new` with discrete args. `verb.rs` rejects empty values, values that
start with `-`, and control characters. The two `sh -c` uses (push-via-shell and
the git checkout) run a fixed script and pass the variable parts as positional
arguments (`$1`, `$2`), not interpolated into the script text. The git checkout
also validates the branch and the destination path
(`executor.rs:2090`). No shell injection through names, paths, or env found.

---

## 4. Verification notes on the threat axes

**Sandbox (axis 1).** The argv is sound. `--unshare-all --share-net
--die-with-parent`, a tmpfs `$HOME`, `/etc` limited to name lookup, TLS roots, the
user database, the time zone, and the linker cache (`sandbox.rs:50`). The child env
starts empty and scrubs `CLAUDE*`, `ANTHROPIC*`, `OPENAI*`, the GitHub tokens,
`COPILOT*`, and `PI_*` (`sandbox.rs:73`, `child_env`). The daemon's `TERRARIUM_TOKEN`
does not pass to the harness, because `child_env` starts from a fixed allowlist, not
the daemon env. The Docker socket and the daemon's Unix API socket are not bound;
only the per-agent runner socket is (`sandbox.rs:341`). `--new-session` off by
default is reasoned and accepted (own PTY per sandbox). The one real sandbox issue
is F1.

Residual: `--share-net` gives the harness full outbound network, including the
cloud metadata address and any internal service the container can reach. This is
accepted by design (the harness must reach its model API), but it is also the main
data-exfiltration path for a compromised agent. See §5.

**Agent identity and policy (axis 2).** The actor comes from the runner socket, not
from a client header (`agent_rpc.rs:117`, `actor.rs`). `X-Terrarium-Agent` is
refused on the Unix socket unless `--insecure-agent-header` is set, and is ignored
on TCP (`actor.rs:122`). The RPC tunnel only forwards `/api/*` paths
(`agent_rpc.rs:104`). An agent cannot set the request extension. The policy table
matches the PLAN §10 table across the handlers (§2 maps each check). Role changes
are audited in the store layer (`store/agents.rs:230`); create, update, delete,
start, stop, send, and answer all audit. `--confirm` guards a delete of an agent
with a live session (`api/agents.rs:205`). No impersonation or role-escalation path
found.

**Credentials (axis 3).** API keys store mode 0600 under `<state>/credentials`
(`login_session.rs:1168`, `credentials.rs`). The git credential helper answers only
agents, only for a repo in the agent's own project, with a 30-per-minute rate limit
and an audit row per answer (`api/repos.rs:442`). Keys inject as env only for the
agent on that subscription (`credential_env_for`). `command_line` redacts secret
env in the dry-run and the start log (`sandbox.rs:500`). The one note is F1 (the key
is in `runner.json` on disk).

**Artifacts (axis 4).** The HMAC key is 32 random bytes, mode 0600, created on first
use (`store/artifacts.rs:127`). `verify` checks the expiry and the MAC in constant
time (`:188`). Bundle signatures sign `id:exp`, not the path, by design, so a
bundle's relative links work. The tar check rejects anything but regular files and
directories (no symlinks or hardlinks), rejects `..` and absolute paths, caps the
entry count, and requires a root `index.html` (`check_bundle`, `:327`). Unpack uses
`unpack_in` with permissions and overwrite controlled (`:374`). Serving rejects `..`
in the path (`api/artifacts.rs:510`). Content types are sniffed from the bytes, not
the client claim, and executables download (`sniff`, `is_executable`). This area is
solid; the only note is F5.

**Prompt injection (axis 5).** The default prompt tells the agent that mail is
untrusted (`prompt.rs:176`). The markdown sanitizer escapes HTML and filters schemes
(`markdown.ts`). `SAFE_URL` allows `data:image/(png|gif|jpeg|webp)` but not
`data:image/svg+xml`, which is correct, because an SVG data URI can carry script.
The one note is F3 (the render path, not the sanitizer).

**Host agents (axis 6).** The registration token stores as an unsalted SHA-256
(`store/hosts.rs:60`), which is fine for a 256-bit random value. The connect checks
the token before the upgrade and refuses an unknown or revoked token
(`hosts.rs:618`). The resume token rotates per connect (`hosts.rs:295`). A host only
accepts calls for its own targets. Command construction is clean (F9). The transport
default is the problem (F2).

**API surface (axis 7).** The bearer compare is constant time on the bytes (F7). The
Unix socket is mode 0660 (`api/mod.rs:291`), so a host user in the daemon's group
can connect as the operator — document this as the trust boundary. No CORS layer is
present, which is correct for a same-origin dashboard; confirm no permissive CORS is
added later. The firehose filters events by `visible_to` for a non-sysadmin agent
(`streams.rs:159`). Long-poll and body limits exist for uploads
(`UPLOAD_BODY_LIMIT`), though most JSON bodies use the axum default limit. SQL is
clean (F8). The notes are F4 and F5.

**Supply chain and build (axis 8).** The npm global installs pin exact versions
(`docker/Dockerfile:84`), the dashboard uses `npm ci` with a lockfile (`:25`), the
harness versions are pinned build args, and the seccomp profile documents its moby
source commit and SHA-256 (`docker/seccomp-bwrap.README.md`). The Copilot extension
installs at container start and keys on the image version. `systempaths=unconfined`
plus the custom seccomp profile are required for bwrap and are documented. This area
is careful.

**Logging and privacy (axis 9).** The dry-run and start log redact secret env. The
audit covers the privileged actions. The residual points are F1 (the agent can edit
its own `pty.log` and `events.jsonl`) and F6 (login PTY bytes).

---

## 5. Accepted by design — do not re-raise

These are known and documented trade-offs. The review confirms they are intentional.

- **Shared harness homes, read-write, with one login each.** OAuth tokens and
  `.credentials.json` are readable by every agent of that harness (PLAN §11;
  `crates/adapters/claude/src/config.rs` header). The blast radius is every agent on
  that shared login. This is the design, because a token refresh by one agent must
  reach all of them.
- **API keys and OAuth tokens in the harness env.** Visible to the harness's shell
  commands, including Copilot's `--secret-env-vars` list (PLAN §16.1, §21).
- **Yolo mode inside the sandbox.** The agent runs with permissions skipped; the
  sandbox is the boundary (PLAN §11).
- **Network stays on (`--share-net`).** The harness must reach its model API. A
  compromised agent can exfiltrate outward. Consider an egress allowlist or a
  proxy-only network namespace as a future hardening, but it is out of scope for v1.
- **`--new-session` off by default; TIOCSTI reasoning.** Each sandbox has its own
  PTY, so an injection reaches only the agent's own input (PLAN §11).
- **Transcripts shared and readable by peers.** `projects/<cwd>/<uuid>.jsonl` and the
  Codex and Copilot session files stay shared, so the usage collector and `--resume`
  can read them (adapter config headers).
- **The embedded login terminal is operator-only, short-lived, and audited** (PLAN
  §16.1). The device-code and paste-code flows are used because a browser loopback
  callback cannot reach into the container.
- **The operator Unix socket needs no token.** Access to the socket is the operator
  boundary (`api/mod.rs:277`). The socket is mode 0660.

---

## 6. Suggested order of work

1. F1: move the runner's control and record files out of the bound agent dir, or
   bind the dir read-only. This protects the budget, the audit trail, and resume.
2. F2: require `wss://` for remote hosts and bind the host listener to loopback by
   default.
3. F3: add DOMPurify behind the markdown renderer, and move the operator token out
   of `localStorage`.
4. F4: carry the WebSocket token in the subprotocol, not the query string.
5. F5: make the separate artifact origin the default.

---

## 7. Status after fixes (2026-10-24)

Re-check of the fixes against this review. The re-check read the committed code
at `HEAD` (`c26b9d8`; the security commits are `24a9cd5`, `3fe5f8d`, and
`5f19cdb`). It read a `git archive HEAD` copy, so edits in the working tree did
not change the result. The file:line references below are in that `HEAD`. The
next commit, `122b9f8`, changes one doc comment in `api/targets.rs` and passes
`TERRARIUM_ARTIFACT_PUBLIC_URL` and `TERRARIUM_WS_QUERY_TOKEN` through
`compose.yaml` (empty by default). It changes no verdict.

| # | Verdict | Main evidence | Test that proves it |
|---|---|---|---|
| F1 | Fixed | `sandbox.rs:340-346` binds `harness/` with `--ro-bind`; `private/` is never bound | `sandbox::tests::the_sandbox_never_binds_the_private_dir`, `sandbox::tests::bwrap_argv_matches_the_golden_command` |
| F2 | Fixed, with residuals | `hosts.rs:784-845` (loopback plaintext default, TLS, warning); `terrarium-host/src/tls.rs:280-286` (host refuses remote `ws://`); `store/hosts.rs:265-273` (atomic enrol swap) | `tls_link_with_a_pinned_certificate`, `plaintext_to_a_remote_host_needs_allow_insecure`, `host_listen_defaults_follow_tls`, `an_enrol_token_is_swapped_for_a_stored_long_lived_token`, `a_revoked_token_is_refused`, `rotation_keeps_the_live_link_and_retires_the_old_token` |
| F3 | Fixed; the token is still readable by script | `lib/sanitize.ts:157` (`setSafeHtml`, the one sink); `eslint.config.js:24-30`; CSP `script-src 'self'` (`vite.config.ts:39`) | `lib/sanitize.test.ts` (52 payloads, each through the renderer and through DOMPurify alone, plus a mount test) |
| F4 | Fixed; `?token=` is still accepted by default | `api/mod.rs:234-245` (subprotocol), `:387-393` (echo), `:316-330` (warning without the token); CLI uses `Authorization` (`crates/terrarium/src/client.rs:204`) | `api::tests::websocket_token_in_the_subprotocol`, `api::tests::websocket_upgrade_echoes_the_bearer_subprotocol`, `api::tests::query_token_warns_once_per_client` |
| F5 | Fixed | `api/mod.rs:159-161,190-192` (`/a/` only without an artifact listener); `api/artifacts.rs:379-410,444-453` (absolute signed URLs) | `api::tests::artifacts_mount_on_the_dashboard_only_without_their_own_origin`, `api::artifacts::tests::signed_urls_point_at_the_artifact_origin` |
| F6 | Partially fixed | `login_session.rs:867` (`log_max_bytes = 0`), `:1037` (dir removed before the final state), `:701` (`Drop`), `:945` (sweep) | `claude_login_with_a_pasted_code`, `codex_device_login_and_failures` (`assert_no_login_output_on_disk`, `assert_login_dir_gone`), `ptylog::tests::a_zero_limit_writes_nothing` |
| F7 | Fixed | `api/mod.rs:210-218`: SHA-256 of both sides, then a fold compare with no early exit | `api::tests::token_compare` |

Test runs on the `HEAD` copy, all green:

- `cargo test -p terrariumd --lib`: 106 passed.
- `cargo test -p terrariumd --test hosts --test artifacts --test runner_process --test fleet --test subscriptions`: all passed, 1 ignored (`live_probes_against_real_homes`, needs real logins).
- `cargo test -p terrarium-host -p terrarium-runner`: all passed.
- `vitest run src/lib/sanitize.test.ts src/stores/auth.test.ts src/api/sockets.test.ts`: 130 passed.

### F1 — Fixed

The argv builder binds `<state>/agents/<ulid>/harness` read-only
(`sandbox.rs:340-346`). It never names `<state>/agents/<ulid>/private` or the
agent dir itself. The runner records are all in `private/`: `runner.json` and
`runner-argv.json` (`terrarium-runner/src/remote.rs:617-629`), `runner.log`,
`events.jsonl` (`runner.rs:242`), `pty.log` (`ptylog.rs:23`), and
`runner-exit.json` (`process.rs:74-77`). `create_private_dir` sets mode 0700
(`terrarium-runner/src/config.rs:150-155`).

The golden test pins the bind part of the argv (`sandbox.rs:615-622`):

```
--proc /proc --dev /dev --tmpfs /tmp --tmpfs /home/agent
--bind /state/agents/01K8AGENT0000000000000000A/workspace /home/agent/work
--ro-bind /state/agents/01K8AGENT0000000000000000A/harness /state/agents/01K8AGENT0000000000000000A/harness
--bind /auth/claude /home/agent/.claude
--bind /state/agents/01K8AGENT0000000000000000A/harness/claude/claude.json /home/agent/.claude/.claude.json
--bind /state/agents/01K8AGENT0000000000000000A/harness/pi/sessions /state/agents/01K8AGENT0000000000000000A/harness/pi/sessions
--bind /run/terrarium/agents/01K8AGENT0000000000000000A.sock /run/terrarium/agent.sock
--ro-bind /usr/local/bin/terrarium /opt/terrarium/bin/terrarium
```

The read-write binds are the workspace, the shared harness home (accepted in §5),
the runner socket, and the per-agent overlays that the adapters return:

- Claude: `harness/claude/claude.json` over `~/.claude/.claude.json`.
- Copilot: `settings.json`, the history file, the open-sessions file,
  `sidebar-sessions-state/`, `harness/copilot/logs/`, and the cache dir
  (`crates/adapters/copilot/src/config.rs:250-298`).
- pi: `harness/pi/sessions/` and the settings overlay
  (`crates/adapters/pi/src/config.rs:160-181`).

Abuse check of these overlays:

- `claude.json` is the agent's own Claude state. The daemon reads it back on the
  next start, adds the quiet-start keys, and writes it with a temp file and a
  rename in `harness/claude/`, which the agent cannot write
  (`crates/adapters/claude/src/config.rs:180-188`). The daemon takes no trust
  decision from it. The agent can only change its own harness, which yolo mode
  gives it already.
- The runner reads only `private/runner.json` and the pid files
  (`spec.rs:213`, `process.rs:88-136`). It reads nothing from `harness/`.
- The hook config (`harness/claude/settings.json`, `harness/copilot/plugin/`,
  `pi/terrarium.ts`) is read-only now. The agent cannot rewrite its hooks.
- The agent can put a symlink in a writable overlay directory (`pi/sessions/`).
  The daemon only reads from that directory (session lookup, usage). See F12 for
  the read side.

Residuals:

- See F11: the migration of an agent dir from the old layout follows symlinks.
- The test uses a synthetic layout. Nothing in `wrap` refuses an adapter overlay
  whose source is under `private/`. Add an assert in `SandboxConfig::wrap_with`
  that drops (and logs) any bind whose source or target is under
  `layout.private_dir`, so a future adapter cannot undo F1.

### F2 — Fixed, with residuals

- The dedicated listener is plaintext on `127.0.0.1:8082` without a certificate,
  and TLS on `0.0.0.0:8082` with one (`hosts.rs:68-72,784-810`). A plaintext bind
  to a non-loopback address logs `INSECURE: ...` at warn level
  (`hosts.rs:827-838`). Compose passes `auto` and mounts `config/tls` read-only.
- The host agent refuses `ws://` to a host that is not loopback unless
  `allow_insecure = true` (`terrarium-host/src/tls.rs:280-286`). The pin
  verifier checks the SHA-256 of the leaf, and it still checks the handshake
  signatures with the real rustls functions (`tls.rs:369-425`).
- The enrol swap is one `UPDATE ... WHERE name = ?1 AND enrol_token_hash = ?2 AND
  enrol_expires_at > ?4`, and only `n == 1` returns a token
  (`store/hosts.rs:265-273`). It runs in the write transaction with the audit
  row (`hosts.rs:1003-1015`). A second dial with the same enrol token gets
  `goodbye token_revoked`.
- A revoked (deleted) or rotated token is `Auth::Unknown` and gets 401 before the
  upgrade (`hosts.rs:646-660`). A revoke also closes the live link
  (`api/hosts.rs:152-178`).

Residuals:

- The dashboard router also serves `/hosts/connect` (`api/mod.rs:193-196`). Port
  8080 is plaintext and compose publishes it on every interface. The daemon logs
  no plaintext warning for this path. A host with `allow_insecure = true` that
  dials 8080 sends its token in clear. Fix: serve `/hosts/connect` only on the
  dial-in listener, or log the same `INSECURE` warning when a host connects on
  the TCP API listener from a non-loopback peer.
- A rotate keeps the live link up (`store/hosts.rs:247-249`; test
  `rotation_keeps_the_live_link_and_retires_the_old_token`). After a suspected
  capture, the operator must revoke, not rotate. Document this in
  `docs/host-protocol.md`.
- The long-lived token does not rotate on each connect, as the original fix
  suggested. The enrol flow and the manual rotate replace that. This is
  acceptable with TLS.
- With `allow_insecure = true`, the enrol token and the long-lived token in
  `welcome.token` both cross the link in clear. A listener on the path can win
  the enrol race. Say this next to `allow_insecure` in the docs.
- The operator bearer token on port 8080 is still plaintext unless a proxy adds
  TLS. This was in the original fix text, and it stays a deployment rule.

### F3 — Fixed; the token is still readable by script

- `setSafeHtml` (`lib/sanitize.ts:157-159`) is the one sink. It appends a
  DOMPurify `RETURN_DOM_FRAGMENT` result with `replaceChildren`, so there is no
  second HTML parse (mXSS). `Markdown.tsx` and `SafeHtml.tsx` use it. A search of
  `dashboard/src` found no other `innerHTML`, `outerHTML`, `srcdoc`,
  `insertAdjacentHTML`, `DOMParser`, or `prop:innerHTML`.
- The config allows 31 tags and 15 attributes, forbids SVG and MathML, and checks
  each URL again after it removes whitespace and control characters
  (`sanitize.ts:15-120`). DOMPurify is pinned at 3.4.17.
- The page CSP (`vite.config.ts:33-58`) has `script-src 'self'`, so an injected
  inline handler cannot run even if the sanitizer fails. `img-src` is `'self'
  data:` and the artifact origin. `index.html` sets `referrer: no-referrer`.

Payload classes that the re-check tried in jsdom against `HEAD` (DOMPurify alone,
and the renderer plus DOMPurify):

| Payload | Result |
|---|---|
| `<img src="data:image/svg+xml;base64,...">` | `src` dropped |
| `<img src="data:image/png;charset=utf-8;base64,...">` | `src` dropped (the regex wants `;base64,` right after the type) |
| `<a href="//evil">`, `<a href="/\evil">`, `<a href="\t//evil">` | `href` dropped |
| `<a href="ws://evil">` | `href` dropped |
| `java​script:`, `javascript :` | `href` dropped |
| full-width `ｊａｖａｓｃｒｉｐｔ:`, `%6Aavascript:`, `javascript%3A` | kept, but the browser reads each one as a relative path; no script runs |
| `<input type=checkbox autofocus onfocus=...>` | becomes `<input type="checkbox" disabled="">`; `autofocus`, `onfocus`, `name`, `form`, `id` all dropped |
| `<input type=image ...>` | element removed |
| `<ol start=999999999>` | kept (`\d{1,9}`); `start=-5` and `reversed` dropped; harmless |
| nested `<table><a><img onerror>` mXSS | handler dropped; the re-parse round trip stays inert |
| `<code class="language-js&quot; onmouseover=...">` | `class` dropped |
| `<img loading=lazy src="https://evil/t.png">`, `![x](https://evil/t.png)` | **kept by the sanitizer**; the CSP `img-src` blocks the load, and `no-referrer` hides the page URL (see F14) |
| `<img src="/api/agents">`, `<a href="/api/agents/x/stop">` | kept; an image or a link sends no bearer header, so the API answers 401; no CSRF |

No payload class ran script. The one gap is that the sanitizer keeps remote
images. Only the CSP stops a tracking pixel.

Token storage: the token is in memory and in `sessionStorage`. A copy goes to
`localStorage` only when the operator ticks "Remember on this device"
(`stores/auth.ts:1-90`). An old `localStorage` copy moves to the tab. Script on
the dashboard origin can still read every copy. The exposure is smaller (one
tab, no persistent copy by default), but an XSS bug still gives the token. The
real fix is an `HttpOnly` session cookie from the daemon, which the dashboard
README names as future work.

### F4 — Fixed; `?token=` is still accepted by default

- The daemon reads `Sec-WebSocket-Protocol: bearer.<token>` (`api/mod.rs:234-245`)
  and echoes the entry in the 101 answer (`:387-393`).
- The dashboard sends the subprotocol and checks the echo (`api/sockets.ts`). It
  falls back to `?token=` only in a dev build with `VITE_ALLOW_QUERY_TOKEN=1`
  (`api/index.ts:14`).
- The CLI sends `Authorization: Bearer` on the WebSocket upgrade
  (`crates/terrarium/src/client.rs:204`). It never uses `?token=`.
- Logs: the daemon has no `TraceLayer` and no tracing call that writes a request
  URI. The deprecation warning logs only the client IP, once per address, with a
  4096 cap (`api/mod.rs:287-330`). The daemon log does not hold the token in the
  query form.

Residuals:

- `TERRARIUM_WS_QUERY_TOKEN` defaults to on (`settings.rs:232-239`). A reverse
  proxy in front still logs `?token=` from an old client. Set the default to off
  in the next release.
- The echo puts the token in a response header. Few proxies log
  `Sec-WebSocket-Protocol`, but say so in the proxy notes.

### F5 — Fixed

- With an artifact listener (the default, `0.0.0.0:8081`, `settings.rs:190`), the
  TCP router does not mount `/a/...` (`api/mod.rs:159-161,190-192`). Only an
  empty `TERRARIUM_ARTIFACT_LISTEN` puts it back on the dashboard port, and the
  viewer then shows a warning badge (`lib/artifact-origin.ts`).
- Signed URLs are absolute to the artifact origin (`api/artifacts.rs:444-453`).

`X-Forwarded-*` check: `origin_from` trusts `X-Forwarded-Host` and
`X-Forwarded-Proto` from any client (`api/artifacts.rs:396-408`). A caller that
spoofs them gets a URL to an origin of its choice, with a valid `exp` and `sig`.
This gives no new access:

- Minting needs the operator token or an agent actor. The agent RPC builds its
  request with no client headers (`agent_rpc.rs:125-145`), so an agent cannot
  set the header.
- The signature covers `id:exp` and gives read access to an artifact that the
  caller can already read.
- A browser cannot set `X-Forwarded-Host`.

Residual: set `TERRARIUM_ARTIFACT_PUBLIC_URL` in any deployment behind a proxy,
so the URL does not depend on headers. `ARTIFACT_CSP` still has no
`connect-src 'none'` (`api/artifacts.rs:36`), which the original fix suggested.
A sandboxed artifact script can still `fetch` out.

### F6 — Partially fixed

- The login runner writes no `pty.log` (`login_session.rs:867`,
  `rc.log_max_bytes = 0`). The output stays in memory, capped at 512 KiB.
- The watch task kills the command, waits for it, and removes the temp dir before
  it stores the final state (`:1030-1042`). `Drop` removes the dir on a panic
  (`:701-705`). The next login start removes stale `terrarium-login-<ULID>` dirs
  older than the timeout plus 60 s (`:945-973`).
- The device code and the URL are not in events, not in the audit detail, and
  not in the daemon log at info level. `login.input` audits a character count.

Residuals:

- `update_login` writes `url` and `device_code` to the `login_sessions` row
  (`store/subscriptions.rs:448-460`). A final state does not clear them. The code
  is on disk in SQLite for the life of the database, and in each backup. Fix:
  set `url` and `device_code` to `NULL` when the state becomes final.
  **Fixed (2026-10-24):** `update_login` sets both to `NULL` for a final
  state, the in-memory view clears them too, and the daemon start clears
  them in old rows (`expire_open_logins`).
- A failed login stores `the login command exited with code N: <last screen
  line>` in `error` (`login_session.rs:1011`). That text goes to the row and to
  the `login.end` audit detail. If the last line holds a code, it stays. Fix:
  keep the exit code only, or redact with the device-code pattern.
- The sweep runs only at the next login start, not at daemon start.

### F7 — Fixed

`token_matches` hashes both sides with SHA-256 and compares the 32-byte digests
with a fold that has no early exit (`api/mod.rs:210-218`). The length leak is
gone. Host tokens use a hash lookup in SQLite, which is fine for 256-bit tokens.

---

## 8. Re-check notes

### (a) Can an agent reach the runner control socket and set `stopped_by`?

No. The control socket is `<runtime>/runners/<ulid>.sock`, mode 0600
(`terrarium-runner/src/config.rs:222-224`, `socket.rs:31-34`). The sandbox binds
only `<runtime>/agents/<ulid>.sock` at `/run/terrarium/agent.sock` (golden argv
above). `set_stopped_by` is a method of the control server only
(`server.rs:439`). The agent socket server (`socket.rs:51`) has no such method.
The daemon calls it only from `stop` and `restart` (`runner_supervisor.rs:1851,1890`).

The runner treats SIGTERM, SIGINT, and SIGHUP as a container stop
(`process.rs:204-208`). The harness cannot send them: `--unshare-all` gives it a
new PID namespace, and the runner calls `setsid`, so a PTY hang-up goes to the
harness session, not to the runner.

### (b) Can the harness change `runner-exit.json` or the recover decision?

Not in a dir made with the new layout. The runner process writes
`private/runner-exit.json` through a temp file and a rename (`process.rs:231-245`).
The harness controls only its own exit code. `recover` reads the record
(`runner_supervisor.rs:2083-2110`), and only `stopped_by` and the exit status
decide the relaunch. The exception is F11: an agent dir from the old layout with
a planted `private` symlink.

### (c) Can a `project_admin` apply a fleet that grants `sysadmin` or reaches outside its project?

No for roles, projects, and global targets. `authorize` runs on the plan, and
`apply` plans and authorizes again in the write transaction
(`fleet.rs:869-905,957-958`). `roles_touched` is the set difference of the wanted
and the current roles (`fleet.rs:628-638`). For a new agent it holds every role.
`change_role` refuses `sysadmin` to anyone but the operator (`policy.rs:182-196`).
A scratch test on the `HEAD` copy, with the `terrarium/lead` agent
(`project_admin`), gave:

| Attempt | Result |
|---|---|
| Fleet with a new agent with `roles = ["sysadmin"]` | 403 `granting or revoking sysadmin needs the operator` |
| Fleet with an agent in project `ops` | 403 `changing a project from a fleet needs project_admin of ops` |
| New project, global target, subscription | 403 (existing test `policy_refuses_members_and_limits_project_admins`) |
| New agent with `roles = ["project_admin"]`, `policy.targets = ["buildbox"]`, `cross_project = ["ops/*"]` | 200: allowed, the same as `POST /api/agents` |

Two gaps came out of this check: F10 and F13.

---

## 9. New findings from the re-check

### F10 — Any agent can overwrite the stored record of any fleet (LOW)

`crates/terrariumd/src/fleet.rs:869` (`authorize`), `:1042` (the `fleets` upsert).

`authorize` checks only the entries of the plan. Without `prune`, it skips the
removals. A fleet that declares nothing is valid (a warning only,
`terrarium-core/src/fleet.rs:688`). So a fleet with no objects and the name of an
existing fleet passes `authorize` for any actor, a `member` too. The apply then
replaces `content`, `origin`, and `applied_by` of that fleet.

The scratch test confirmed it. A `member` agent sent `version = 1` and
`name = "terrarium"` to `POST /api/fleet/apply` and got 200. After that,
`GET /api/fleet/terrarium` showed `origin = "api"`, `applied_by = agent:...`, and
a plan that deletes every object of the fleet. A `project_admin` can do the same
with only the objects of its own project.

Impact: the operator's drift view now shows the real objects as removals. A
later `fleet apply --prune` of the stored content deletes agents and targets in
projects that the agent cannot touch. The file auto-apply restores the record at
the next reload, and the `fleet.apply` audit row names the agent.

Fix: when the fleet name exists, require that the actor may manage every object
that the fleet owns now (run the removal checks of `authorize` even without
`prune`), or require `sysadmin` to apply to a fleet that another actor applied.
Refuse an API apply to a fleet whose origin is `file:`. Refuse an empty fleet
for a non-sysadmin.

### F11 — The split of an old agent dir follows planted symlinks (LOW)

`crates/terrarium-runner/src/config.rs:150` (`create_private_dir`), `:181`
(`migrate_legacy_layout`); `runner_supervisor.rs:806-811,2087`.

Before the fix, the sandbox bound the whole agent dir read-write. A yolo agent
could create `<agent dir>/private` as a symlink to its workspace before the
upgrade. Terrarium agents work on this repository, so the new layout name is not
a secret. After the upgrade, `create_dir_all` accepts the symlink, and
`set_permissions` follows it. The runner then writes `runner.json` (with the
injected keys), `events.jsonl`, `pty.log`, and `runner-exit.json` into the
workspace, which the agent can read and write. This restores F1 for that agent. A
planted `events.jsonl` or `runner.json` symlink at the top level moves into
`private/` as a symlink with `rename`, with the same result. A planted `harness`
symlink sends the daemon's hook config into a writable place.

Fix: in `create_private_dir` and in `migrate_legacy_layout`, use
`symlink_metadata`. Refuse (or remove and log) a symlink at `private/`,
`harness/`, or any moved entry. Move only regular files and directories. Open
the record files with `O_NOFOLLOW`. Add a test with a planted symlink.

### F12 — The usage collector tails any file that a hook names (LOW)

`crates/terrariumd/src/usage_collector.rs:199` (`host_path`), `:530`.

The `transcript_path` of `session_started` comes from the hook payload
(`crates/adapters/claude/src/hooks.rs:122-128`). The harness, or the agent, sends
that payload to the runner socket (`{"type":"hook",...}`,
`crates/terrarium/src/runner_socket.rs:36`). `host_path` uses the path as it is
when it exists on the daemon side, with no check that it is under the agent's
harness home. An agent can point the collector at an empty file to report no
usage, or at another agent's transcript to take its usage. The parsers read only
usage records, so the data leak is small. The main effect is on per-agent spend
and budgets.

Fix: canonicalize the path and accept it only under the shared home's session
directory of that harness (for example `projects/` for Claude) or under the
agent's own `harness/` dir. Refuse a symlink. Log a refused path at warn level.

### F13 — A `project_admin` can grant use of global targets (LOW)

`crates/terrariumd/src/policy.rs:140-158` (`use_target`),
`crates/terrariumd/src/api/agents.rs:141-153` (`update`).

PLAN §10 keeps "define global targets" for `sysadmin`. But `use_target` allows a
global target to any agent whose `policy.targets` names it, and a
`project_admin` sets `policy` for every agent of its project. The update handler
has no check for the agent's own record, so a `project_admin` can add a global
target to its own policy, and then exec on that host. `cross_project` mail works
the same way. The fleet apply has the same result (check (c) above).

This matches the PLAN text ("Set an agent's system prompt, harness, policy: in
project"), so it can be a design choice. But a `project_admin` agent reads the
project inbox, which is a prompt-injection path. Fix, if the operator wants the
boundary: let only a `sysadmin` (or the operator) add a global target or a
`cross_project` entry to a policy, and forbid an agent to change its own
`policy`.

### F14 — Defence-in-depth gaps in the dashboard sink rules (INFO)

`dashboard/eslint.config.js:24-30`, `dashboard/src/lib/sanitize.ts:76-82`.

- An ESLint run on a scratch file showed that the rule catches `el.innerHTML = x`
  and the `innerHTML` JSX attribute only. It does not catch `el["innerHTML"] = x`,
  `Object.assign(el, { innerHTML })`, `new DOMParser().parseFromString(...)` with
  an append, `<div prop:innerHTML={x} />`, or `<iframe srcdoc={x} />`. No code
  uses these today. Add selectors for computed members, `prop:` names,
  `srcdoc` in JSX, and `DOMParser`.
- `isSafeImageSrc` keeps any `https:` image. Only the CSP `img-src` stops a
  tracking pixel in a wiki page. The CSP is a meta tag only; the daemon sends no
  CSP header for the dashboard (`crates/terrariumd/src/assets.rs`). Drop remote
  images in the sanitizer (keep `'self'`, `data:`, and the artifact origin), and
  send the CSP as a header too.

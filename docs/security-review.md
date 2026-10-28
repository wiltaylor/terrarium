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
| F3 | MEDIUM | Dashboard | `components/Markdown.tsx:15` | Untrusted markdown through `innerHTML` and a hand-rolled sanitizer | Fixed; the token residual is fixed too: an `HttpOnly` session cookie, and the dashboard keeps no token (§7) |
| F4 | MEDIUM | API auth | `api/mod.rs:207,256` | Bearer token in `?token=` query string for WebSockets | Fixed; the daemon now refuses `?token=` by default (§7) |
| F5 | LOW | Artifacts | `api/mod.rs:173`, `api/artifacts.rs:34` | Artifacts on the dashboard origin by default; isolation depends on CSP | Fixed |
| F6 | LOW | Login flow | `login_session.rs:789` | Login PTY output and `pty.log` in a temp dir hold the device code until cleanup | Partially fixed; the SQLite residual is fixed (a final state clears `url` and `device_code`). The `error` text residual stays. |
| F7 | LOW | API auth | `api/mod.rs:189` | Token compare returns early on a length mismatch (length leak only) | Fixed |
| F8 | INFO | SQL | `events.rs:208`, `store/usage.rs:177`, `store/mail.rs:266` | Dynamic SQL uses fixed identifiers and integers only; no injection found | Not re-checked (no change) |
| F9 | INFO | Host verbs | `terrarium-host/src/verb.rs`, `executor.rs:982` | Commands build as argv, not shell strings; no injection found | Not re-checked (no change) |
| F10 | LOW | Fleet | `fleet.rs:869,1042` | Any agent, a member too, overwrites the stored record of any fleet with an empty fleet of the same name | Fixed: an apply needs the rights over every object the stored fleet owns; a member gets 403; an empty fleet needs sysadmin; an API apply over a `file:` fleet needs `confirm` |
| F11 | LOW | Sandbox | `terrarium-runner/src/config.rs:150,181` | The split of an old agent dir follows symlinks that the agent planted before the upgrade | Fixed (daemon quarantine + runner no-follow opens): `bc85806` and `efd9663` (§9) |
| F12 | LOW | Usage | `usage_collector.rs:199,530` | The collector tails any file that a forged `SessionStart` hook names | Fixed: only a regular file under the session area of the harness, that names its session, and that no other agent owns; else a `usage.rejected_path` event |
| F13 | LOW | Policy | `policy.rs:140`, `api/agents.rs:141` | A `project_admin` adds global targets to `policy.targets`, its own too | Fixed: a global target, a `cross_project` entry, or a `mail_to` entry outside the project needs sysadmin (API and fleet) |
| F14 | INFO | Dashboard | `eslint.config.js:24`, `lib/sanitize.ts:76` | The sink lint rule has gaps; the sanitizer keeps remote images (the CSP blocks them) | Fixed: the lint rule covers the gaps, with a fixture test; the daemon sends the CSP as a header. Remote images are accepted (§5) |
| F15 | INFO | API auth | `password.rs`, `api/session.rs` | The new operator password login, JWT sessions, and the CLI session store: a design review of the change (2026-10-27) | Built with the controls of §10; the independent review findings R1 to R10 are fixed (§10); residuals in §10 |
| F16 | INFO | Credentials | `accounts.rs`, `account_swap.rs`, `sandbox.rs` | Plan accounts: several plan logins per provider, per-account credentials, live swaps, and the refresh rule (2026-10-27) | Built with the controls of §11; a CI test keeps the token endpoints out of the code; residuals in §11 |

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
that agent, but it widens the on-disk footprint. (Later: the key no longer goes into
`runner.json`; see §14, "Residuals".)

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

**Supply chain and build (axis 8).** The image holds no harness (since commit
5cda78a, 2026-10-26). `just ci::check` fails when `docker/Dockerfile` installs one.
The harnesses and the Copilot prompt extension live in the tools volume (`./tools` at
`/opt/terrarium/tools`, one npm prefix for each version). `terrariumd harness install` runs at
each container start (the entrypoint, then the daemon). It installs each missing
package at the exact pin in `harnesses.toml` and leaves a present package at its
version. It writes `tools/terrarium-manifest.json` with the package, version, and
install time of each one. The Copilot extension installs into the Copilot home when
its version or the Copilot version changes. An update runs `npm install -g
<package>@<version>` into a new version dir through the daemon (`POST /api/harnesses/{name}/update`,
`crates/terrariumd/src/harness_tools.rs`). Only a sysadmin or the operator can call it,
and each call writes the audit row `harness.update` and a `harness` event. Thus npm
reaches the registry at the first start and at each update, not at the image build:
the install trusts the registry at that moment (PLAN §21). The pins limit the first
install, the manifest records what came in, and the version guard warns (or, with
`TERRARIUM_HARNESS_VERSION_POLICY=refuse`, refuses) an agent start on a version
outside the tested range. The dashboard uses `npm ci` with a lockfile, the Node image
is pinned in `harnesses.toml` (`[node] image`) and checked against the Dockerfile
default, and the seccomp profile documents its moby source commit and SHA-256
(`docker/seccomp-bwrap.README.md`). `systempaths=unconfined` plus the custom seccomp
profile are required for bwrap and are documented. This area is careful.

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
- **Transcripts per agent (no longer shared, 2026-10-25).** This entry used to accept
  shared transcripts. The live cross-harness run showed that every agent could read,
  and write into, the transcripts of every other agent in every project: one cwd
  (`/home/agent/work`) for all agents means one Claude `projects/<cwd>/`, and Codex
  and Copilot had one `sessions/` and one `session-state/`. That contradicts the
  prompt line "You cannot see the files of other agents" and is a path for prompt
  injection. Now each agent has its own session files as read-write overlays from its
  `harness/` dir: Claude `projects/<cwd>/` (with the auto memory), `file-history/`,
  `todos/`, `session-env/`; Codex `sessions/`, `session_index.jsonl`,
  `shell_snapshots/` (each holds the exported env of the agent's shell), and its
  SQLite home (`-c sqlite_home`; `thread_history_1` holds every turn item); Copilot
  `session-state/` and `session-store.db` with `-wal` and `-shm` (its `turns` table
  holds every turn, and the model can query it). The cross-project exposure is
  closed. `--resume` finds each session in the agent's own dir, and the usage
  collector reads only that dir (F12). Residuals: Claude
  `security_warnings_state_<uuid>.json` (a file in the home root), `paste-cache/`,
  and `shell-snapshots/` stay shared (the Claude snapshots hold `PATH` and shell
  functions, the same for every agent); Codex `thread-writer-locks/` stays shared
  (empty lock files keyed by the session ID); transcripts written before the
  change stay in the shared home, hidden from agents by the binds. With sandbox mode
  `none` (development only) nothing is bound, and the session files are shared.
- **The embedded login terminal is operator-only, short-lived, and audited** (PLAN
  §16.1). The device-code and paste-code flows are used because a browser loopback
  callback cannot reach into the container.
- **The host installer needs no token (2026-10-26).** `GET /install/host.sh`,
  `GET /install/terrarium-host`, and `GET /install/terrarium-host.sha256` are
  outside `/api` on the dashboard port, so `require_token` lets them through, as it
  does the dashboard assets (`api/install.rs`). They hold no secret: the public URLs,
  the systemd unit, the SHA-256 of the host listener certificate (every TLS client
  sees the certificate), and the binary. The enrol token comes only from
  `POST /api/hosts`, which needs the operator. Values from the request (`Host`, the
  `arch` query) go into the script only after a check of the characters; an unsafe
  value is left out. The host listener and the ingest listener do not serve these
  routes. Residual: the script and the binary come over the same link, so the
  SHA-256 finds a broken download but not a changed one. On a network that you do
  not trust, put the dashboard port behind HTTPS.
- **The operator Unix socket needs no token.** Access to the socket is the operator
  boundary (`api/mod.rs:277`). The socket is mode 0660.
- **The harness tools volume is writable outside the sandbox.** `./tools`
  (`/opt/terrarium/tools`) is writable by the host user and by the daemon, which runs
  npm there. Each sandbox binds it read-only (`crates/terrariumd/src/sandbox.rs`), so
  an agent cannot change a harness. A process with the host user's rights, or a
  compromise of the daemon, can change every harness; that is the same trust as the
  `./config` and `./state` mounts.
- **Harness updates are an operator or sysadmin action.** `POST
  /api/harnesses/{name}/update` and `POST /api/harnesses/update` need sysadmin or the
  operator. Each update writes the audit row `harness.update` (`package`, `version`,
  `from`, `to`, `force`, `ok`, `error`) and a `harness` event. A sysadmin agent can thus pick the version that every agent runs next;
  that is in the sysadmin role by design.
- **A running harness keeps its files during an update.** Each harness version
  is its own npm prefix (`tools/versions/<name>/<version>/`), and a version dir does
  not change after it is complete. An update installs a new dir and switches the
  links with an atomic `rename`. An agent launch runs the harness from the version
  dir that was active at the launch, with that `bin/` first on `PATH`, so a late
  `require` in the middle of a turn reads the files of its own version. The GC
  never removes the version dir of a live agent. So the daemon does not refuse an
  update while agents run; the answer lists them (`agents_on_old_version`).
- **Remote `https:` images in agent-written markdown.** The sanitizer keeps them, so
  a wiki page can hold a tracking pixel. The page CSP `img-src` (`'self'`, `data:`,
  and the artifact origin) blocks the load, not the sanitizer. This is accepted
  (F14).

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
| F3 | Fixed; the token residual is fixed later (see the F3 update) | `lib/sanitize.ts:157` (`setSafeHtml`, the one sink); `eslint.config.js:24-30`; CSP `script-src 'self'` (`vite.config.ts:39`) | `lib/sanitize.test.ts` (52 payloads, each through the renderer and through DOMPurify alone, plus a mount test) |
| F4 | Fixed; `?token=` is refused by default later (see the F4 update) | `api/mod.rs:234-245` (subprotocol), `:387-393` (echo), `:316-330` (warning without the token); CLI uses `Authorization` (`crates/terrarium/src/client.rs:204`) | `api::tests::websocket_token_in_the_subprotocol`, `api::tests::websocket_upgrade_echoes_the_bearer_subprotocol`, `api::tests::query_token_warns_once_per_client` |
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

**Update (2026-10-24, `22ccd5d`, `85e35eb`):** the daemon now gives signed
`HttpOnly` session cookies (`POST /api/session`, `api/session.rs`). The
dashboard keeps only the CSRF value and the end of the session, not the token
(`stores/auth.ts`). A mutating call with the cookie needs `X-CSRF-Token`. The
token is no longer readable by script after sign-in. A daemon without sessions
makes the dashboard fall back to bearer mode, with a notice.

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

**Update (2026-10-24, `22ccd5d`):** `TERRARIUM_WS_QUERY_TOKEN` now defaults to
`0` (`settings.rs`), so the daemon refuses `?token=`. `1` turns it back on.

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

**Fixed (2026-10-24, `bc85806`):** `authorize` (`fleet.rs`) refuses a plain
member, and checks every object that the stored fleet owns, with or without
`prune`. `check_overwrite` needs sysadmin for an empty fleet over a stored
fleet, and `confirm` for an API apply over a `file:` fleet
(`409 confirm_required`). `docs/api.md` §18.4 and `docs/fleet.md` §5 have the
rules.

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

**Fixed (2026-10-24, `bc85806`):** the daemon checks an old agent dir before
the split (`runner_supervisor.rs`, `migrate_dir`). A dir with a symlink goes to
`<ulid>.quarantine-<ts>`, and a clean dir takes its place.

**Fixed (2026-10-24, `efd9663`):** the runner opens every private file with
`O_NOFOLLOW` and refuses a file that is not a regular file
(`terrarium-runner/src/safefs.rs`). `create_private_dir` refuses a symlink at
the agent dir and at `private/`, and a symlink or a special file with a record
name in `private/`. The migration refuses planted links. A refusal is
`RunnerError::UnsafePath`, with the path.

**Follow-up (2026-10-24):** the daemon uses the same no-follow opens
(`terrarium_runner::safefs`) for the private files that it reads and writes
itself: `session-prompt.sha256` and `events.jsonl`. An `UnsafePath` at launch
puts the agent in `error`, and the status event has the path in its reason.
The restart policy does not try again, because a planted entry does not go
away. The test is `unsafe_private_dir_is_an_error_without_a_restart_loop`
(`crates/terrariumd/tests/runtime.rs`).

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

**Fixed (2026-10-24, `bc85806`):** `check_transcript` (`usage_collector.rs`)
accepts only a regular file under the session area of the harness. A Claude
Code, Codex, or Copilot file must carry the session ID in its name. A file that
another agent reads is refused. A refused file gives a warn log and a
`usage.rejected_path` event (`docs/api.md` §16.7).

**Narrowed (2026-10-25):** the session files are now per agent (§5). In the
sandbox the only root is the agent's own session dir:
`harness/claude/projects/`, `harness/codex/sessions/`, or
`harness/copilot/session-state/`. A sandbox path (`/home/agent/.claude/projects/...`)
maps into that dir, not into the shared home. A file in another agent's
`harness/` dir, or in the shared home, is refused even with the right session ID.
The session-ID match stays. With sandbox mode `none` the shared home's session
area is allowed too, because no overlay is bound there.

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

**Fixed (2026-10-24, `bc85806`):** `policy_grants` and `set_policy`
(`policy.rs`) need a sysadmin or the operator for a new global target, a new
`cross_project` entry, or a new `mail_to` entry in another project. The API
and the fleet apply use the same check. An entry that the policy has already
may stay (`docs/api.md` §6.1).

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

**Fixed (2026-10-24, `85e35eb`, `22ccd5d`):** the sink rule
(`dashboard/eslint.config.js`) now catches computed members (`el["innerHTML"]`
and template keys), `Object.assign` and `Reflect.set`, `new DOMParser()`,
`setHTMLUnsafe`, and the JSX `innerHTML`, `prop:`, `attr:`, and `srcdoc`
names. `dashboard/src/lib/sink-lint.test.ts` runs the rule on a fixture of each
sink. The daemon now serves the CSP as a response header, with
`frame-ancestors 'none'` (`api/mod.rs`). The remote image gap stays: §5 accepts
it, because the CSP `img-src` blocks the load.

**Also fixed (2026-10-24, `8853861`):** `DELETE /api/session` now needs the
CSRF token when a session cookie is present. This closes a logout CSRF that
the dashboard client work found (`api/session.rs`, `docs/api.md` §1.1).

## 10. Operator password and JWT sessions (F15, 2026-10-27)

The change: the operator logs in with a password, and a login gives a
session token (a JWT) that lives for weeks, so the operator does not paste
`TERRARIUM_TOKEN` again and again. The dashboard keeps the token in an
`HttpOnly` cookie. The CLI keeps it per daemon URL in
`$XDG_CONFIG_HOME/terrarium/credentials.json` (`terrarium login`). The
static token still works everywhere. The contract is `docs/api.md` §1.1.
The code is `crates/terrariumd/src/password.rs`,
`crates/terrariumd/src/api/session.rs`, the middleware `require_token` in
`crates/terrariumd/src/api/mod.rs`, `crates/terrarium/src/credentials.rs`,
and `dashboard/src/stores/auth.ts`.

### Threats and controls

| Threat | Control | Test |
|---|---|---|
| Password storage | Only an Argon2id PHC hash (m = 19 MiB, t = 2, p = 1, a random salt) in `<state>/operator.password`, mode 0600 (set again at a read that finds a looser mode). Each write holds a `flock` on `operator.password.lock`, writes a unique temp file, fsyncs it, renames it, and fsyncs the dir. `terrariumd password set` refuses to run as another user than the owner of the state dir. The daemon never logs or returns the password; the audit rows hold the IP only. A broken file fails closed. `TERRARIUM_PASSWORD(_FILE)` is read from the env only (no flag, so never in argv), and the daemon starts again without both variables, so no child process inherits them. | `password::tests`, `tests/password_cli.rs` (`the_first_password_leaves_the_env`) |
| Brute force | Per client address (an IPv6 address as its /64): a check takes a slot **before** Argon2, and the checks in flight use the budget (at most 2 per address, and never more than the free failures left), so a parallel burst cannot pass. 5 free failures, then a lockout of 30 s that doubles with each failure up to 15 min (`429` and `Retry-After`). At most 16 checks in flight from all addresses, at most 2 Argon2 checks at one time, so one host cannot queue in front of the operator. 100 failures from all addresses in 10 min lock the password login until the window ends (the static token still works). The map holds at most 10,000 addresses; a new address past that gets no entry, so another address's lockout never reaches it. Every wrong password gets the same `401 "login failed"`. A 12-character minimum. | `a_burst_of_logins_cannot_pass_the_limit`, `password_login_rate_limit`, `tickets_in_flight_count`, `global_budget_locks_password_logins`, `limiter_is_bounded`, `ipv6_keys_by_64` |
| JWT algorithm confusion | The daemon makes one header, `{"alg":"HS256","typ":"JWT"}`, and takes only `alg` `HS256` with no other header field (`deny_unknown_fields`). `none`, `HS512`, `hs256`, and a `kid` all fail before the signature check. No JWT library: the code uses the `hmac` and `sha2` crates that it used before. The signature compare is constant time. | `verify_is_strict`, `cli_token_is_a_bearer_with_no_csrf` |
| Token theft | The browser token is `HttpOnly` and `SameSite=Strict`, and `Secure` on HTTPS, so script cannot read it. The page keeps only the CSRF value, in memory; no password, token, or CSRF value goes into web storage, and the old copies are deleted at load. A token has a `kind`: a browser token works only as the cookie, a CLI token only as `Authorization: Bearer`. A logout revokes the session (`sid`) on the server; "log out everywhere" and a new password end every token (the `pv` claim), and a token is minted only for the password version that was checked. A renewed browser session never lives past `auth_time` + `TERRARIUM_SESSION_MAX_AGE` (90 days). The CLI speaks `https://` (rustls, system roots, `TERRARIUM_CA_FILE`), and `terrarium login` refuses plain `http://` to another host without `--insecure-http`. The CLI file is mode 0600 in a 0700 dir. | `password_change_ends_every_session`, `log_out_everywhere`, `mint_needs_the_checked_version`, `renewal_has_an_absolute_cap`, CLI `tls::tests`, `plain_http_login_needs_the_flag`, `login_and_logout_with_a_temp_xdg_dir` |
| CSRF | Unchanged: a cookie `POST`, `PUT`, `PATCH`, or `DELETE` needs `X-CSRF-Token`, an HMAC of the session's `sid`. The renewal keeps the `sid`, so the CSRF value does not change and there is no race between two tabs. `DELETE /api/sessions` and `PUT /api/auth/password` go through the same check. A wrong `current` password answers `403`, not `401`, so the dashboard does not drop a good session. | `session_cookie_needs_csrf_to_change_state`, `sliding_renewal` |
| Session fixation | The server makes each `jti` and `sid` from 16 random bytes at the login; a client cannot choose them. A login always starts a new `sid`. A session token cannot buy another session (`401`). A login with a cookie in the request ignores that cookie. | `session_cookie_create_use_and_clear` |
| Login CSRF | A body needs `Content-Type: application/json`, which a cross-site form cannot send without a CORS preflight; the daemon answers no preflight on `/api`. The attacker would also need the password. | `password_change_ends_every_session` |
| Replay after logout | The revocation table keys the session (`sid`), so a logout also ends each renewed cookie of that session, until the last one could end. | `sliding_renewal` |

### Findings of the independent review (2026-10-27) and their fixes

| # | Severity | Finding | Fix |
|---|---|---|---|
| R1 | HIGH | Parallel requests passed the limiter: the check ran before the wait for Argon2, and the count after. | A `Ticket` is taken before the wait and counts as in flight; at most 2 per address and never more than the free budget left; a second look at the lockouts after the wait. Test: 50 parallel wrong logins from one address, at most 5 Argon2 checks (in fact 2), the rest `429`. |
| R2 | MEDIUM | The password version was read again at the mint (and at a renewal), after the check. | `OperatorAuth::check` answers the version of the hash that matched; the login mints with it, a renewal with the token's `pv`; a changed version gives `409` and no token. |
| R3 | MEDIUM | An unbounded queue in front of Argon2 let one host starve the operator. | At most 16 checks in flight from all addresses; past it, `429` at once, before the queue. |
| R4 | MEDIUM | IPv6 rotation in a /64, no global budget, a shared overflow bucket that locked out new addresses, and an O(n) prune at each failure. | /64 keys; 100 failures in 10 min lock the password login globally (the static token still works); no overflow bucket (an untracked address has no lockout of its own; the global budget counts it); a prune of a full map at most once a minute. |
| R5 | MEDIUM | The CLI sent the password and a 90-day token over plain `http://`. | `https://` in the CLI client (rustls, system roots, `TERRARIUM_CA_FILE`); `terrarium login` refuses plain `http://` to a host that is not loopback without `--insecure-http`, and warns loudly with it. |
| R6 | LOW | The password file write raced across processes, and did not fsync the dir. | A unique temp file, a `flock` on `operator.password.lock` around each read-modify-write, an fsync of the file and of the dir. Test: 8 threads with separate stores bump 80 times, no version lost. |
| R7 | LOW | Sliding renewal had no absolute cap. | The `auth_time` claim; a renewal copies it and caps `exp` at `auth_time` + `TERRARIUM_SESSION_MAX_AGE` (default 90 days); an older browser token gets `401`. |
| R8 | LOW | `TERRARIUM_PASSWORD` could reach child processes and argv. | No `--password` flag; the daemon starts again without both variables (a process cannot clear its own env without `unsafe`); the login commands and npm strip them too. Test: `/proc/<pid>/environ` of a running daemon. |
| R9 | LOW | The mode of the password file was not checked on read; a root `password set` could lock the daemon out. | A read sets 0600 again, with a warning; `terrariumd password set` refuses to run as another user than the owner of the state dir. |
| R10 | LOW | One audit row per failed login let an attacker flood the audit table. | One `auth.login_failed` row per address per 10 min with a `count`, and one `auth.lockout` row when a lockout starts; the success rows stay. |

### Residuals

- **The limiter is in memory, and the lockouts are a denial of service.**
  Behind a reverse proxy, every client shares the proxy's address, so an
  attacker can lock the operator's password login for up to 15 min; with
  100 failures from many addresses, for up to 10 min for everyone. The
  static token still works in both cases. The daemon does not trust
  `X-Forwarded-For`, on purpose. A restart clears the counts.
- **Long sessions.** A stolen cookie or CLI token works until it ends (30
  and 90 days by default, and a browser session at most 90 days from the
  login through its renewals) or until "log out everywhere". Shorten
  `TERRARIUM_SESSION_TTL`, `TERRARIUM_SESSION_MAX_AGE`, and
  `TERRARIUM_CLI_SESSION_TTL` on a shared machine.
- **`TERRARIUM_PASSWORD` in `.env`.** The daemon process drops it, but it
  stays in `.env` and in the container's configuration (`docker inspect`)
  until the operator removes it. The daemon logs a warning at each start
  while it is set. `terrariumd password set` is the better path.
- **`--insecure-http`.** With the flag, the CLI sends the password in clear
  text. It is for a network that encrypts by itself.
- **The CLI token file.** It is a bearer credential on disk, protected only
  by the file mode, like an SSH key with no passphrase.
- **The key file.** `session.key` signs every token. Anyone who reads the
  state dir can mint tokens; the state dir already holds the database and
  the harness credentials.
- **The `v1.` cookies** of the older daemon are refused. The dashboard shows
  the login screen once after the update.

## 11. Plan accounts and the refresh rule (F16, 2026-10-27)

The change: each plan login is its own subscription with its own account,
`<state>/credentials/accounts/<subscription>/` (PLAN §16.3). The shared
harness homes keep the settings; the credentials move into the accounts.
An agent moves between the accounts of its pool: live for Claude Code and
pi, by a restart with resume for Codex and Copilot. The code is
`crates/terrariumd/src/accounts.rs`, `account_swap.rs`, the launch in
`runner_supervisor.rs`, and the binds in `sandbox.rs`.

### The refresh rule

The daemon never refreshes an OAuth token, never calls an OAuth token
endpoint, and never writes a token that it got itself. Each harness
refreshes its own token with its own code, under its own lock. The daemon
may:

- read a credential file (the probes read the access token for the usage
  endpoints, as before);
- copy or move a credential file when the operator adds or migrates an
  account;
- make directories and symlinks, and re-point an agent's symlink;
- take a harness's own lock (a `proper-lockfile` directory) to keep a swap
  atomic. Holding a lock is not a refresh;
- restart agents.

`crates/terrariumd/tests/no_token_endpoint.rs` scans the Rust sources of
every crate for `oauth/token`, `auth.openai.com/oauth`,
`platform.claude.com/v1/oauth`, `console.anthropic.com/v1/oauth`,
`login/oauth/access_token`, `copilot_internal/v2/token`, and `grant_type`.
`just ci::check` fails when it finds one. A probe that finds an expired
token reports it, and waits for the harness of an agent to refresh it.

### Threats and controls

| Threat | Control | Test |
|---|---|---|
| An agent reads an account outside its pool | The sandbox binds only the stores of the agent's pool members that have a login of its harness (Claude Code, pi), each at its own path. `account.json`, the backups, the git credentials, and the stored API keys stay unbound. Codex gets a file bind of its current account only; Copilot gets the env token of its current account only. | `accounts::tests::a_launch_binds_only_the_pool`, `sandbox::tests::only_the_pool_accounts_are_bound`, and `a_sandboxed_process_sees_only_its_pool` (a real bwrap run) |
| An agent re-points itself to another account | The pointer `harness/cred/account` is in the harness dir, which the sandbox binds read-only. Only the daemon changes it, by a new symlink and a rename. | `accounts::tests::the_pointer_swaps_atomically` |
| A swap during a refresh loses a rotated refresh token | A live swap takes Claude's `.oauth_refresh.lock` and `.storage-write.lock` in the old and the new account (pi: the shared `auth.json.lock`) before it moves the pointer, and waits up to 45 s for a holder. Claude's dead-holder takeover answers `no_owner_record` for a lock without an owner file, so it waits for the daemon and does not take over. A lock older than its `stale` time is removed, as `proper-lockfile` does. | `locks_follow_proper_lockfile`, `a_claude_swap_takes_both_accounts_locks` |
| Two copies of one rotating refresh token | The migration moves a shared login (a copy, a sync, then the removal of the shared file); it never leaves two live copies. It runs only while no agent of the harness runs, under the supervisor's slot lock, so no launch reads the shared file during the move. | `the_migration_moves_each_shared_login_once`, `the_migration_finishes_a_cut_move_and_refuses_a_guess`, `shared_logins_migrate_and_full_accounts_swap` |
| A login touches another account | The login wizard runs in a scratch home (mode 0700) and moves only the credential into the account of its subscription. A second login of a provider makes a second account. A logout removes the account dir and runs no harness command. | `an_import_touches_only_its_account`, `claude_login_with_a_pasted_code` |
| Symlink tricks in a store | A pool agent can change the files of a pool store. The daemon reads and writes credentials with `O_NOFOLLOW` on the last part, refuses a symlinked store dir, and writes new files by `create_new` and a rename. | `accounts.rs` (`read_nofollow`, `private_dir`) |
| Secrets in logs | The launch logs the names of the env vars only; `command_line` hides `*TOKEN*` values; `account.json` holds no secret. | `command_line_hides_secrets` |

### Residuals

- **Pool members share their tokens.** A Claude Code or pi agent can read
  and change the credential of each account of its pool. Give a pool only
  to agents that may use all of its accounts.
- **Codex has no cross-process lock.** All Codex agents on one account
  refresh at about the same time. Codex reloads the file before a proactive
  refresh, so the window is short, but a reuse of a refresh token can
  revoke the token family (`refresh_token_reused`). Then each agent on the
  account needs a new login. Keep few Codex agents on one account.
- **pi's lock is global.** pi locks `/auth/pi/auth.json.lock` (`realpath:
  false`), so refreshes of different accounts wait for each other. This is
  safe, and only slower.
- **Claude's `oauthAccount` is stale after a live swap** until the next
  start of the agent. The daemon does not edit `.claude.json` while Claude
  Code runs.
- **An undocumented env var.** `CLAUDE_SECURESTORAGE_CONFIG_DIR` can change
  in a later Claude Code release. The compat suite must check it.
- **The backups.** `<state>/credentials/backup/` keeps a copy of each moved
  shared login (mode 0600 in a 0700 dir). Its refresh token is old after the
  first refresh; delete the backups after a good rollout.
- **Copilot and the shared `config.json`.** Copilot's code is closed. With
  `COPILOT_GITHUB_TOKEN` set, it is not known whether Copilot writes a token
  into `config.json`. A launch warns (a `warning` event) when the shared file
  holds a token again, and the next quiet migration moves it.

## 12. The config repo (F17, 2026-10-28)

The change: the operator's prompts, harness settings, and an export of
the declarative objects live in a git repo, `config/terrarium/`
(`/config/terrarium`, PLAN §8.3), which Terrarium instances share through a
remote. A repo that leaves the machine must never hold a secret. The code
is `crates/terrariumd/src/config_repo.rs`, `api/config.rs`,
`crates/terrarium-core/src/secret_scan.rs`, and `git.rs` (`GitRunner`).

### What may go in, and what never does

| In the repo | Never in the repo |
|---|---|
| `prompts/**/*.md`, `harness/**/*.{json,toml,yaml}`, `export/**/*.{toml,json,yaml,md}`, `README.md`, `.gitignore` | The harness homes (`config/<harness>`: logins, sessions, history), `state/` (the database, the credential store, the plan accounts, `session.key`, `operator.password`), `.env`, an API key, a login, a token, a password, a host token or enrol token, a TLS key, artifact data, session transcripts, `.terrarium-defaults.json`, the `.new` files |

### Threats and controls

| Threat | Control | Test |
|---|---|---|
| A credential file lands in the repo | The `.gitignore` is an allowlist: `/*` ignores everything, then only the known dirs and file types come back, then the credential file names (`.env*`, `auth.json`, `.credentials.json`, `credentials.json`, `hosts.yml`, `*.pem`, `*.key`, `id_*`) are ignored again. The daemon writes the file at each start and before each commit, so an edit does not stay. | `init_writes_the_allowlist_and_only_known_paths_are_committed` |
| A secret in an allowed file (a token pasted into a prompt or a settings file) | The secret scan runs on each staged blob (the index, not the work tree) before each daemon commit. The rules: the token patterns (`sk-ant-`, `sk-or-`, `sk-proj-`, `sk-kimi-`, GitHub, AWS, Slack, private keys, JWTs, the host tokens `thk_` and `the_`), a credential-like key with a value of 16 characters or more (env references such as `$ZAI_API_KEY`, placeholders, and paths are not values), a run of 32 or more characters with an entropy over 4.0 bits, and the credential file names. A hit blocks the commit and unstages. | `the_scan_blocks_each_secret_pattern`, `secret_scan::tests` |
| A secret in a commit made by hand with git | The daemon installs a `pre-commit` hook that runs `terrarium admin config scan --staged` (or `terrariumd config scan --staged`). With no Terrarium binary on `PATH`, the hook fails closed. | `the_hook_blocks_a_manual_commit_with_a_secret` |
| The scan leaks what it finds | A finding holds the path, the line, and the rule, never the matched text. The API error, the CLI, and the hook print only these. | `findings_never_hold_the_value`, the value check in `the_scan_blocks_each_secret_pattern` |
| The export copies a secret or an instance ID | The export names a credential or a subscription by its name only, drops the user part of each repo URL, drops the mail `thread` of a schedule, and holds no ULID. Event sources (webhook secrets), hosts, and subscriptions are not exported. | `the_export_has_no_secrets_and_no_instance_ids` |
| The remote credential lands in `.git/config` or the repo | `remote set` keeps the URL and the credential name in `<state>/config-repo.json` (mode 0600). No `[remote]` is written: each fetch and push names the URL on the command line. The token reaches git through a one-shot credential helper in a 0700 run dir, the SSH key through `GIT_SSH_COMMAND` with a 0600 key copy, as for a workspace clone. A URL with a user or a password is refused. The status shows the URL without its user part. | `two_instances_sync_a_prompt_edit_both_ways`, `remote_urls_with_secrets_and_agents_are_refused`, `urls_show_no_credentials` |
| A pull runs code from the remote | Each git command runs with the hardening of the workspace updates (`core.hooksPath=/dev/null`, no fsmonitor, the dangerous transports off, no global or system config, no askpass). The daemon makes its own commits with `--no-verify` after its own scan. Pulled prompts and settings are data; pulled `export/` files apply only with `pull --apply`, through the fleet checks and rights. | `git.rs` (`HARDEN`), `a_pulled_export_is_a_plan_until_apply` |
| A sync loses local work | A merge is a fast-forward or a merge commit. A conflict stops the merge with the files listed; nothing is reset, and no push is forced. `abort` goes back to the last local commit. | `a_conflict_stops_with_no_data_loss` |
| An agent changes the shared config | Every `/api/config/...` route needs the operator or a sysadmin; `remote set` needs the credential rights too. The sandbox does not bind the config repo. | `remote_urls_with_secrets_and_agents_are_refused` |

### Residuals

- **The scan is a pattern list.** A secret with no known prefix, a short
  value, or low entropy (a dictionary password) can pass. The allowlist
  and the review of each diff stay the main control. Read
  `t admin config diff` before each commit.
- **`git commit --no-verify` skips the hook.** A commit by hand can skip
  the scan on purpose. A daemon sync then pushes it. Do not commit by hand
  with `--no-verify`.
- **The allow marker.** A line with `terrarium-scan: allow` skips the
  `credential-key` and `high-entropy` rules (not the token patterns). A
  reviewer must check each one.
- **The history.** A secret that reached the remote before these controls,
  or through `--no-verify`, stays in the history. Rotate the secret; a
  history rewrite is a manual step.
- **A pulled persona is a prompt.** A shared repo lets each instance
  change the prompts and the harness settings of the others (for example
  a Claude `permissions` block or a Codex `mcp_servers` entry). Give the
  remote write access only to operators that you trust as much as your
  own.

## 13. Skills (F18, 2026-10-28)

The change: Terrarium-managed skills (PLAN §8.6). A skill is a folder in
the config repo (`skills/<name>/`): a `SKILL.md` with instructions, and
optional scripts, references, and assets. At each start the daemon copies
the agent's skills into its harness dir, and the harness loads them. The
code is `crates/terrarium-core/src/skills.rs`, `crates/terrariumd/src/skills.rs`,
`api/skills.rs`, the delivery in `runner_supervisor.rs`, and the skill
keys of each adapter.

A skill is **prompt content that the operator controls**. Its text goes
into the model's context when the agent loads it, as a persona or a
prompt template does. Its scripts are files: they run only when the agent
runs them, in the sandbox, with the rights of the agent.

### Threats and controls

| Threat | Control | Test |
|---|---|---|
| An agent writes or changes a skill (its own, or one of another agent) | Every write of `/api/skills` needs the operator or a sysadmin. The delivered copy is in `harness/`, which the sandbox binds read-only. The config repo is not bound into the sandbox. | `crud_validation_and_rights`, `the_sandbox_never_binds_the_private_dir` |
| An agent reads skills that it does not have | `GET /api/skills` and `GET /api/skills/{name}` give a non-sysadmin agent only the skills that its own `skills` list picks; another name is `403`. The delivery copies only the agent's skills. | `crud_validation_and_rights`, `a_launch_delivers_only_the_agents_skills`, `deliver_copies_only_the_assigned_skills` |
| A skill holds a secret, and the config repo carries it to a remote | Each write runs the secret scan on each file (`422`, the finding without the value). The commit scan and the `pre-commit` hook (§12) cover a skill file that came by hand. The `.gitignore` admits only the skill file types under `skills/`; the credential file names stay ignored. | `each_rule_is_checked`, `skill_files_go_into_the_config_repo_with_the_scan` |
| A path or a link in a skill escapes its dir (on write, or on the copy into the agent dir) | A path must be relative, with no `..`, no hidden part, and at most 4 parts. A symbolic link or a special file in a skill dir is refused, and such a skill is not delivered. The delivery makes a new, empty dir each start. | `read_dir_refuses_symlinks`, `each_rule_is_checked`, `write_get_list_remove` |
| A large skill fills the disk or the context | At most 64 files and 1 MiB for each skill, and 1024 characters of description. | `each_rule_is_checked` |
| Skills that Terrarium does not manage load into an agent (the operator's own skills in a shared harness home, bundled skills that start cloud work, a marketplace plugin) | Claude Code: `--setting-sources ""`, `disableBundledSkills`, no `enabledPlugins`, `CLAUDE_CODE_DISABLE_POLICY_SKILLS`. Codex: the agent's dir over `$CODEX_HOME/skills`, `skills.bundled.enabled = false`. Copilot: `ignoredSkillsLocations` and the built-ins in `disabledSkills`. pi: `--no-skills`. With or without agent skills. | the adapter tests `the_skill_tool_follows_the_delivered_skills`, `the_skill_list_follows_the_delivered_skills`, `skills_go_to_the_settings_and_the_replace_prompt`, `delivered_skills_load_with_no_skills`, `overlay_list` |
| An agent with no skill gets a skill tool (and its built-in skills) | Claude Code: `Skill` in `permissions.deny`. Copilot: `--excluded-tools=skill`. Codex: `skills.include_instructions = false`. pi has no tool. | as above |
| The dashboard renders a skill as HTML | A description shows as text. The SKILL.md preview goes only through the DOMPurify Markdown sink (F3, F14). | the dashboard tests |

### Residuals

- **A skill is a prompt.** The operator (or a sysadmin agent) can put any
  instruction in a skill, and a shared config repo lets each instance
  change the skills of the others, as for the prompts (§12). Give the
  remote write access only to operators that you trust.
- **A sysadmin agent writes skills.** A sysadmin agent can change the
  skills of every agent, as it can change their prompts and settings.
- **Scripts run in the sandbox.** A skill script runs with the rights of
  the agent that runs it: its workspace, its targets, and the network. The
  sandbox is the limit, not the skill.
- **Codex reads the workspace.** `<workspace>/.agents/skills` and a
  project `.codex/skills` still load in Codex (no key turns a repo root
  off). The workspace is the agent's own dir, so such a skill is the
  agent's own text.
- **The mechanisms are read, not run.** The live check of
  `docs/compat.md` ("Skills") is open.

## 14. Secret manager review (2026-10-29)

An independent review of the secret manager (PLAN §16b, commits
7a07953..23a4ae7). The reviewer did not write the code, and proved each
"verified" finding with a test in a copy of the source. S2 and S3 are in
8ca3edd. S4, S5, S7, S8, S11, and S12 are in 9fa4b78. The fixes of S1,
S6, S9, S10, and S13 are in 43a0307. Each fix has a test that reproduces the finding.

| # | Finding | Status |
|---|---|---|
| S1 (HIGH) | The final env var names of a `generic` secret (`<ENV>_<FIELD>`) skip the reserved-name check, so a grant can set `LD_PRELOAD`, `GIT_CONFIG_*`, `BASH_ENV`, or `TERRARIUM_*`. The launch env is the env of the `bwrap` process outside the sandbox, so a loader var runs code of the agent as the daemon user. | **Fixed.** (a) One check of each final name just before it goes into the launch env (`grants::launch`), and in the grants API for each final name (`422`). A refused name in an old grant row gives a warning event at the start, not a crash. (b) The name rule is an allowlist shape, `^[A-Z][A-Z0-9_]{0,127}$`, and a larger deny set: the loader and C library, `GIT_*`, the shell, the language runtimes, the proxy and CA vars, the harness and account vars, `XDG_*`, `SSH_*`, `TERRARIUM_*`, and the session vars (docs/api.md §5.2, "The name rule"). `GH_TOKEN` stays allowed. (c) No secret value goes to `bwrap`: the sandbox starts the harness through `terrarium exec-env`, which gets the vars from the daemon inside the sandbox (`POST /api/agent/secrets/env`) and replaces itself with the harness. Tests: `generic_env_names_pass_the_name_rule`, `the_name_rule_is_an_allowlist_shape_and_a_deny_set`, `the_launch_env_comes_from_the_daemon_and_follows_the_name_rule`, `exec_env_wraps_the_program_inside_the_sandbox`, `exec_env_adds_the_granted_vars_and_execs`, `secret_grants_reach_the_launch`. |
| S2 (HIGH) | A project_admin gets global secrets with no grant: a project repo or target names a global secret, and the credential helper, a checkout, an ssh exec, or a display login uses it. | **Fixed: grant required** (8ca3edd). A project object can name a global secret only when the project holds a grant of it, or when the operator, a sysadmin, or a definition file set the object (`credential_admin` on `repos` and `targets`). The daemon checks at save (`403` with the fix) and at each use (refused, with a `secret.refused` warning event). The rows from before the upgrade count as set by an admin. PLAN §16b.6a, docs/api.md §5.2, `tests/global_secret_rule.rs`. |
| S3 (MEDIUM) | A `use` grant of a token shows the value: the agent calls the credential helper itself. | **Accepted, documented: `use` is `read` for tokens** (8ca3edd). The credential helper must hand the token to git, and the agent runs git. The docs and the grant form say so; only an `ssh_key` stays hidden under `use`. A test asserts the behaviour. The real fix is an HTTPS proxy in the daemon that adds the token. PLAN §16b.3, docs/api.md §5.2. |
| S4 (MEDIUM) | A project_admin can blind the audit, the events, and the mail of all projects through the global redaction matcher. | **Fixed** (9fa4b78). The redaction keeps the JSON keys and the identifier fields, and a put refuses an identifier or a weak value for a machine secret (`token`, `api_key`, `github_token`). |
| S5 (LOW) | Redaction is a guess oracle, and it shows the names of secrets in other projects. | **Fixed** (9fa4b78). The label of a match follows the view of the writer or the reader. |
| S6 (LOW) | Plain values stay on disk outside the sealed store: `private/runner.json` holds the env of each `read` grant (also after a revoke, and in the trash), and `credentials/backup/*-credential-*` holds each moved credential forever. | **Fixed.** The env values never go into the launch spec, so `runner.json` holds none (S1 (c)); a restart after a revoke writes a new file with no value. A move to the trash removes `private/runner.json` (a file of an older daemon can hold values). The daemon removes each `*-credential-*` backup after the trash retention (7 days), at the start and at each prune. Decision: delete, not seal; a sealed backup needs the same key as the store (PLAN §16b.7). Tests: `secret_grants_reach_the_launch` (no value in any file of the agent dir), `trash_and_prune`, `old_credential_backups_are_pruned`. |
| S7 (LOW) | A project named `global` can exist; its project_admin lists the global secrets. | **Fixed** (9fa4b78). The project name `global` is reserved. |
| S8 (LOW) | A config import can grant any secret that has a value to any agent. | **Fixed** (9fa4b78). A new or wider imported grant of a secret with a value needs the approval of the operator (`--approve-grants`). |
| S9 (LOW) | A checkout on a target runs git in a repo that the agent can change, with the token in the env: a planted `core.fsmonitor` or hook reads it. | **Fixed.** `CHECKOUT_SCRIPT` runs each git command with the `-c` overrides of `git.rs` (no hooks, no fsmonitor, no helper from a config, the safe transports only), with `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, no askpass, a `GIT_SSH_COMMAND` that wins over `core.sshCommand`, and the git env vars that point at another repo unset. Before it touches an existing repo, it checks the key names of `.git/config` against the allowlist of `git::config_allowed` and refuses `.git/commondir`; a key outside the list gives `not_updated` with the key in `note`. The token and the key file go away after the fetch or the clone, before `status` and `merge`. Test: `checkout_script_ignores_a_planted_repo_config` (the script of 23a4ae7 leaks the token to the planted fsmonitor in the same setup). |
| S10 (LOW) | The ssh-agent has no sign rate limit, and each sign decrypts the key (bcrypt KDF for a key with a passphrase). | **Fixed.** A token bucket for each agent socket: 10 signs a second, a burst of 30. A sign over the limit answers `SSH_AGENT_FAILURE` with no call to the key source, and counts as `limited` in the `secret.use` audit row. The source keeps an opened key for 60 seconds in a cache of that agent only; the grant check stays at each sign, and a revoke removes the kept key at the next sign. A cache, not "decrypt once for each connection": `ssh` and `git` open a new connection for each login, so a cache for each connection does not stop a loop. Tests: `signs_over_the_rate_limit_fail_without_the_source`, `signs_open_the_key_once_in_the_cache_time`, `the_socket_serves_the_grants_and_audits_once_a_minute`. |
| S11 (INFO) | The redaction matcher is built after the commit; a write in that window stores a value unredacted. | **Fixed** (9fa4b78). The daemon builds the matcher again inside the write lock (`Tx::rebuild_redactor`). |
| S12 (INFO) | The zeroization is partial (plain `String`s in answers and in the matcher). | **Fixed** (9fa4b78). The answers zeroize, and the docs say what stays. |
| S13 (INFO) | The ssh-agent dir is bound read-write, and `bind_private` changes the mode through a path that can be a symlink. | **Fixed.** The sandbox binds the dir with `--ro-bind`; `connect()` works on a socket in a read-only bind. `bind_private` opens the bound name with `O_PATH` and `O_NOFOLLOW`, checks that it is a socket, and changes the mode through `/proc/self/fd/<n>`. `agent_keys::serve` refuses a socket dir that is not a real dir. Tests: `the_ssh_agent_socket_works_in_a_read_only_bind` (a real `bwrap` and `ssh-add -l`; a write into the dir fails), `chmod_private_does_not_follow_a_symlink`, `the_ssh_agent_dir_binds_with_its_socket_var`. |

### The choice for S1 (c): `terrarium exec-env`

Four ways can give the env to the harness and not to `bwrap`:

| Way | Result |
|---|---|
| `bwrap --setenv NAME VALUE` | Refused: the value is on the argv of `bwrap` (`/proc/<pid>/cmdline`). |
| `bwrap --args FD` with the `--setenv` args in a pipe or memfd | Not possible with the runner: `portable-pty` closes each file descriptor above 2 before it starts `bwrap`, and it has no hook to keep one. |
| An env file in the runtime dir, bound into the sandbox | A value in a file again, and a race to remove it. |
| `terrarium exec-env` asks the daemon over the runner socket | **Chosen.** No value in a file, in an argv, or in the env of `bwrap`. The daemon reads the grants at the request, so a revoke applies at the next start. The CLI is in the sandbox already (hooks, git helper), and the runner socket is up before the harness starts (a request waits for the daemon up to the queue timeout). |

The answer gives the agent nothing that `terrarium secret get` does not
give it: the agent can call the endpoint itself, and it counts in the same
rate limit.

### Notes

- **bwrap is not setuid in the image.** The Dockerfile installs the
  Debian `bubblewrap` package and changes no mode; Debian ships
  `/usr/bin/bwrap` with mode 0755 and relies on user namespaces (the
  seccomp profile `docker/seccomp-bwrap.json` allows them). A non-setuid
  `bwrap` honours `LD_PRELOAD` from its env, which is why S1 (c) keeps the
  secret vars out of that env. On this host `/usr/bin/bwrap` is 0755 too.

### Residuals

- **Other launch env (closed).** The subscription API keys of §16 (with
  `ANTHROPIC_AUTH_TOKEN` for Z.ai and `KIMI_API_KEY` for Kimi Code) and
  the Copilot account token (`COPILOT_GITHUB_TOKEN`) now take the
  `exec-env` path too: `POST /api/agent/secrets/env` gives them in
  `credentials`, and the launch spec holds the names only. So no
  credential value is in `runner.json` or in any other file outside the
  credential stores. With no `terrarium` CLI, such a start fails; it does
  not fall back to the spec. When the daemon does not answer,
  `exec-env --require-credentials` exits 78 and does not start the
  harness. Test: `the_harness_credentials_come_through_exec_env_only`
  (each provider kind, the whole state dir checked). The plan logins of
  Claude and Codex stay files that the sandbox binds (PLAN §16.3).
- **An old `runner.json`.** A file that an older daemon wrote can hold
  a subscription key. It stays until the next start of the agent (which
  writes a new one with no value) or its move to the trash.
- **The credential env inside the sandbox.** Any process of the agent can
  call `terrarium exec-env -- env` and see the subscription key. This is
  not new: the harness has the key in its env, and the agent can read it
  there (`/proc/<pid>/environ`).
- **The env inside the sandbox.** A `read` grant with an env var shows the
  value to the agent by design (`/proc/self/environ`, `env`).
- **Backups for 7 days.** A state backup in the first 7 days after the
  move can hold the plain values of the moved credentials.
- **The repo config of a target.** As in `git.rs`, git reads the live
  `.git/config` at each command. The check runs before the fetch; an
  edit after the check and before the fetch can still add a key for that
  one fetch, which runs with the token. The window is short, and the
  token goes away before the work tree commands.

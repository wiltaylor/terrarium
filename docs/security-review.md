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

| # | Severity | Area | File:line | Summary |
|---|---|---|---|---|
| F1 | HIGH | Sandbox | `sandbox.rs:333` | Whole agent dir bound rw; agent tampers with its own event log, pty log, and resume state |
| F2 | HIGH | Host link | `compose.yaml`, `terrarium-host/src/config.rs:25` | Plaintext `ws://` host link; token and exec traffic in clear, replayable |
| F3 | MEDIUM | Dashboard | `components/Markdown.tsx:15` | Untrusted markdown through `innerHTML` and a hand-rolled sanitizer |
| F4 | MEDIUM | API auth | `api/mod.rs:207,256` | Bearer token in `?token=` query string for WebSockets |
| F5 | LOW | Artifacts | `api/mod.rs:173`, `api/artifacts.rs:34` | Artifacts on the dashboard origin by default; isolation depends on CSP |
| F6 | LOW | Login flow | `login_session.rs:789` | Login PTY output and `pty.log` in a temp dir hold the device code until cleanup |
| F7 | LOW | API auth | `api/mod.rs:189` | Token compare returns early on a length mismatch (length leak only) |
| F8 | INFO | SQL | `events.rs:208`, `store/usage.rs:177`, `store/mail.rs:266` | Dynamic SQL uses fixed identifiers and integers only; no injection found |
| F9 | INFO | Host verbs | `terrarium-host/src/verb.rs`, `executor.rs:982` | Commands build as argv, not shell strings; no injection found |

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

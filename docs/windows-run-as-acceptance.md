# TER-272 native acceptance proposal

Not authorization to execute. Native acceptance has not run. The card stays
blocked before review until Meta approves a window, the exact mutations and
recovery, and the remaining assertions below pass. TER-274 is a separate window.

## Fixed scope

- D1: application code never persists or logs the delivered password. The
  UTF-16 logon buffer is wiped, including invalid input. Wire `String` copies
  and Windows logon/LSASS state are not claimed to be erased or absent. Interactive
  password logon intentionally carries outbound network credentials. No Windows
  credential-cache, domain, logon-right, UAC or firewall policy is changed.
- D2: native logon/profile/environment, token selection, process creation and
  synchronous impersonation live only in `terrarium-winapi`. Profile loading
  enables already-held backup/restore privileges only on a private duplicate
  service token. No account right or shared process privilege is granted.
- D3: `[windows_run_as] enabled = false` is the default. Enabling requires a
  protected host file read through locked, non-reparse handles. File owner must
  be SYSTEM/Administrators. Ancestor owners may also be TrustedInstaller. Other
  trustees cannot have replacement rights, or file write rights. Unsupported
  ACEs fail closed. This read-only elevation trust check provisions no workspace
  or ACL. Workspace support remains off for TER-274.
- D4: only an exact host-file account entry with `elevated = true` selects a
  full token. Other spellings/accounts select ordinary or filtered tokens.
  Missing filtered/linked tokens fail, with no service-account fallback. C uses
  `.\<name>` for filtered and `WINHOST\<name>` for full, proving both modes for
  the same SID without changing UAC or restarting between those assertions.

```toml
[windows_run_as]
enabled = true

[[windows_run_as.accounts]]
account = 'WINHOST\<C-name-from-manifest>'
elevated = true
```

The test host has a separate protected config/token and `auto_update = false`.
That does **not** isolate all existing Windows service state. `main.rs` still
opens `C:\ProgramData\Terrarium\host\service.log` and protects its parent before
SCM dispatch. `windows_update.rs` reads the fixed `host\update.json` before
config loading. Run startup also protects/prunes the fixed default env directory.
Before the proposed ImagePath switch, Meta must prove no update journal exists
and no shared env files exist, inventory those exact paths/SDDL, and explicitly
authorize fixed log append and any startup ACL effects. A nonempty journal/env
directory stops the proposal without changes. Restore any approved changed SDDL.
No test-only service-state override or LocalSystem launcher is introduced.
No development artifact enters the production signature/update flow. The binary
refuses updates without a compiled release key. No signing access is assumed.

## Inventory and names

Meta's 2026-11-12 17:56 read-only inventory: WINHOST `winhost.lab.test`
`10.79.1.20`, Windows 11 Enterprise Eval 24H2 `10.0.26100.9711` X64.
DC01 `dc01.lab.test` `10.79.1.10`, Server 2025 Standard Eval 24H2
`10.0.26100.33438` X64. Domain `lab.test`, NetBIOS `LAB`. Existing share
`\\dc01.lab.test\share`, DC directory `C:\share`. Linux lab owner `winlab`
uid 1009, inventory `/home/winlab/win-lab/vmlab.wcl`. SSH recovery is
`terrarium@10.3.0.2:2254` using the pinned fingerprint reference MEM-588.
Never bypass verification. Refresh readiness and exclusive use before the run.

Meta's 18:13:23 UTC rollback metadata (source message
`01M78FFS378XWCM8CNX5EJ606K`) records `TerrariumHost` Running, LocalSystem,
Auto/Start2, no delayed start. Exact original ImagePath:

```text
"C:\Program Files\Terrarium\host\versions\0.1.0+7cdc823\terrarium-host.exe" --config "C:\ProgramData\Terrarium\host\config.toml" service
```

Original executable SHA256:
`44d9864df32ffe488aa19648f30429db88e9a6a112c13773358ba415db5317b7`.
FileVersion was null, so the directory name is not executable version proof.
Failure reset 86400 seconds, restart delays 5000/15000/30000 milliseconds,
noncrash TRUE, empty command/reboot fields. Exact service SDDL is retained in
that Meta source and must accompany the expanded approved manifest. Protected
paths were SYSTEM/Administrators full only. This metadata is neither backup/
restore proof nor switch authorization. Fixed-path existence/type/owner/SDDL/
size/mtime and safe journal-status inventory remain pending. Unknown update
status is a stop condition. Never read config/token contents to fill this gap.

META-86 F7 subsequently recorded only metadata: `service.log` 41336 bytes,
`update.json` absent, env directory present, config.toml 367 bytes, with owners/
SDDL/mtimes retained there. Meta explicitly leaves pending/confirmed transaction
status **UNKNOWN**. An absent pathname is not accepted as a transaction-status
proof. Do not switch while that status or any shared-state effect is unreviewed.

Candidate source audit (applies to the final candidate, not just old 7cdc823):

| Code | Candidate entry point | Potential fixed/shared effect |
|---|---|---|
| F4 | `main.rs::main` before SCM dispatch | `private_dir(host)` can create the directory, set owner Administrators, replace its protected inheritable DACL with SYSTEM/Administrators FullControl, and propagate inherited ACEs. Then creates/appends fixed `host\service.log`. Inventory affected descendants, including config/token ACL metadata, not just the parent. No log deletion/truncation is approved. |
| F5 | `main.rs::run`, `session.rs::private_dir/remove_private_files` | Creates/protects fixed `host\env` using the same owner/DACL operation and removes regular files with the Docker env prefix. Unknown/nonempty shared state is a stop condition. An empty directory check does not authorize later pruning. |
| F6 | `windows_service_host.rs::serve`, `windows_update.rs::startup` | Reads fixed `host\update.json` before config loading. Depending on a matching prior transaction it can write the image's failed sidecar and remove a journal. Unknown/malformed or unreviewed state stops the switch proposal. |
| F7 | `client.rs::after_welcome`, `selfupdate.rs::confirm`, `windows_update.rs` | Confirmation/collection is not disabled merely by `auto_update=false`. A matching pending transaction can persist the journal through `update.writing`; a later confirmed start can delete the previous version directory and journal. Service health-failure handling can restore SCM selection through rollback. No such production transaction mutation/collection is authorized. |
| F8 | `client.rs::adopt_token`, `config.rs::write_secret` | A welcome may replace the token at the configured token_file and protect its parent. Only the new isolated fixture token path may be selected. Original token/config contents are never read, copied, logged or replaced by the proposal. |

All fixed-path contents remain unread by inventory. Meta must explicitly accept
the expanded bounded startup effects, exact before/after SDDL and retention/
rollback semantics, or reject the deployment proposal. Existing source behavior
is not authorization to prune, collect versions, delete logs or change ACLs.

Let `COMMIT` be the final 40-hex pushed commit and `RUN_ID` the full UTC
`YYYYMMDDTHHMMSSZ` execution ID. The fixture namespace is
`ter272_<COMMIT[:8]>_<RUN_ID>`. A SAM name is
`t272_<COMMIT[:7]>_<sha256(RUN_ID ASCII)[:6]><role>` (exactly 20 ASCII
characters, role `a`, `b`, `c` or `d`). `account_name` in the prebuilt runner
computes this without creating an account. Meta collision-checks each name
case-insensitively immediately before any approved creation. Never reuse or
alter LAB agenttest. Record actual SIDs, not inferred RID values.

## Bounded proposed mutation manifest

Nothing in this table is approved by the document. Record exact expanded paths,
SIDs, hashes, before/after SDDL and cleanup owner in the approved manifest.

| Code | Proposed item | Bound and recovery |
|---|---|---|
| M1 | LAB domain A and B | Two new ordinary accounts only, no group/logon-right grants. New profiles only on WINHOST. A gets the fixture share/directory, B gets no grant there. Remove only these accounts and approved new profiles after unloading. |
| M2 | WINHOST local C | One new disposable local Administrators member, never a DC account. Existing UAC policy unchanged. Remove membership/account/profile after process drain. Needs explicit admin-fixture authorization. |
| M3 | WINHOST local D | One new disabled ordinary local account, for a disabled-account negative test without disabling A/B/C mid-suite. No group/privilege grant. This fourth fixture is an additional proposal requiring approval. |
| M4 | DC share fixture | Only `C:\share\<namespace>` on existing `share`, with A and SYSTEM/Administrators access and no B grant. Existing share permissions must already allow a scoped fixture without changing baseline access. If a share-level grant is needed, stop for exact separate approval. Do not alter the baseline share root/DACL or agenttest. Record owner, DACL and share-grant baseline. |
| M5 | WINHOST fixture tree | Only `C:\ProgramData\Terrarium\Tests\<namespace>`. SYSTEM/Administrators-owned protected base, separate A/B/C scratch leaves granted only to that account plus SYSTEM/Administrators, and a C-only admin sentinel. No Users/Authenticated Users access. Meta checks ancestor replacement rights before creation. No workspace claim. |
| M6 | HKCU fixture | `HKCU:\Software\Terrarium\<namespace>` for A/B, value `ter272`, different non-secret sentinel values. First approved product exec initializes the key, a new session must read A's value and profile owner SID. Remove only these new keys. |
| M7 | Isolated daemon state | `/home/winlab/ter272/<namespace>` with private state, new `secrets.key`, config, config-repo, work, repos, runtime, TLS cert/key and operator/host credentials. No production state/key/grant/token/config copy. Remove exact approved new files only after process drain. No harness agents spawned or tools installed. |
| M8 | Test target/secret/grants | In the isolated daemon only: A, B, C-filtered, C-full, wrong-password-A, disabled-D targets, and narrowly scoped generated login secrets. Wrong-password-A gets exactly one attempt after Meta checks lockout policy. All passwords stay in the isolated secret store and wire delivery, never host config or artifacts. Record target IDs and grant IDs for exact removal. |
| M9 | Temporary host execution | Preferred proposal is an explicitly authorized temporary SCM `TerrariumHost` ImagePath switch to the independently hash-verified fixture host and its separate config. Original versioned binaries and config/token are untouched. Capture exact original ImagePath/arguments, executable SHA256, service account/state/start/failure actions and SDDL first. Stop/switch/start only after parking all affected sessions. Restore those exact bytes/settings and prove Running/online afterward. This requires explicit authorization naming rollback hash/path/SDDL. Do not install a second hardcoded-name service or invent a LocalSystem launcher. |
| M10 | Artifacts/results | Copy only named host/probe/test/daemon/CLI artifacts with build SHA256s, source commit and this runner. Native tests create temporary files, plus `copy-local.bin`, `copy-smb.bin`, `exec-smb.bin` and `timeout-grandchild.txt` under exact fixture leaves. Remove only recorded files after checking no fixture process/SMB handle remains. Preserve unrelated files, shared caches and snapshots. |

The fourth account can instead be avoided with a separately approved second
phase that disables disposable C only after positive tests. That requires a
phase-specific fixture/runner adjustment, not a silent skip of the current D case.

## Isolated endpoint and credential flow proposal

**Network blocked:** Meta reports no assigned VM-only Linux bind IP or guest.
Private ranges are blocked except the existing severus:8182 route, with no
18482 exception. Do not reuse that endpoint, assume a 10.79.x Linux address,
bind wildcard, widen LAN access, or add a tunnel around the control. Ports below
are proposals, not reservations. No daemon/listener is started by this document.

Required allocation for Meta's separate design/security review:

- N0a: one owned Linux test-daemon peer and unprivileged UID/scratch directory,
  with the CLI/test runner colocated. If the peer is not the inventoried uid1009
  lab host, replace the proposed root/UID explicitly in the approved manifest.
  No user, VM, NIC or segment is created automatically.
- N0b: one explicit WINHOST-to-test-peer TCP WSS route on a dedicated isolated
  segment or equivalent reviewed boundary. Only WINHOST is allowed to reach the
  assigned peer/port. Other lab peers, DC management, LAN and public networks
  must be denied in both directions unless an exact separately approved rule
  is required. Keep WINHOST's existing DC/SMB route unchanged. Meta owns any
  future interface/routing/firewall allocation, none is authorized here.
- N0c: Meta records exact interface/IP/port, WINHOST positive reachability and
  independent other-network denial checks, plus negative access to daemon API
  and unrelated ports. Binding a private address alone is not isolation proof.
  TLS SAN names exactly the allocated peer IP/name. A new dedicated key/cert is
  owned by the allocated UID (private directory 0700, key 0600), never a shared
  production key. Verify its SHA256 pin out-of-band and protected host config
  ACLs before connection. Record exact key lifecycle/teardown and never print
  private key/token material. No trust-store/DNS change or skipped verification.

- N1: run the matching Linux daemon as uid 1009 on the lab Linux host, with
  API **only** `http://127.0.0.1:18480` and private Unix socket
  `<root>/runtime/api.sock`. The test runner is colocated. Meta checks port
  availability. No production port, endpoint or token is reused. Loopback API
  traffic never crosses the VM network.
- N2: host listener is `wss://<LAB_BIND_IP>:18482`, using a new dedicated
  certificate/key under `<root>/tls`, pinned in the isolated host config.
  `LAB_BIND_IP` is a required Meta-verified Linux address on the existing
  VM-only network, not inferred from guest addresses. No listener starts if
  the address, exclusive network scope, certificate pin or port is missing.
  No firewall/DNS/trust-store change. No tunnel to production.
- N3: Meta's approved launcher supplies a clean, limited environment. New
  isolated operator credentials and secret values enter through its protected
  environment/private inputs, never argv or stdout. The isolated daemon alone
  decrypts the test login secret on session admission and sends the password
  over pinned WSS. The host consumes it once, retains the token/profile, and
  sends no password into child environment, result frames or replay. Do not
  inherit production credential variables into the isolated daemon.

Proposed daemon invocation, only after the launcher has supplied the isolated
environment (including `TERRARIUM_TOKEN`, host TLS inputs and `TERRARIUM_HOST_LISTEN`):

```sh
"$ROOT/bin/terrariumd" --listen 127.0.0.1:18480 \
  --socket "$ROOT/runtime/api.sock" --state-dir "$ROOT/state" \
  --config-dir "$ROOT/config" --config-repo "$ROOT/config-repo" \
  --runtime-dir "$ROOT/runtime" --work-dir "$ROOT/work" --repos-dir "$ROOT/repos" \
  --defaults-dir "$ROOT/empty-defaults" --tools-dir '' --no-harness-install \
  --artifact-listen '' --git-proxy-listen '' --sandbox none
```

Keep its own PID/run ID and log/exit receipt. Stop only that process tree by
recorded ID at teardown. `sandbox none` here is development-only, no harness
agent execution. Meta/lead must approve this deployment and its clean env.

## Prebuilt execution recipe

Build only on build2 with pinned Rust 1.97.1, eight jobs, `ionice -c3 nice -n19`
and a held matching shared card-check slot/target directory. Final commands:

```sh
cargo clippy --locked -p terrarium-winapi -p terrarium-host \
  --target x86_64-pc-windows-gnu -- -D warnings
cargo build --release --locked -p terrarium-host --target x86_64-pc-windows-gnu
cargo test --locked -p terrarium-winapi --target x86_64-pc-windows-gnu \
  --no-run --message-format json
cargo test --locked -p terrarium-host --test windows_session \
  --target x86_64-pc-windows-gnu --no-run --message-format json
cargo build --release --locked -p terrariumd -p terrarium
```

Also run the Linux host policy/protocol and daemon `windows_run_as::` and
`windows_paths::` regressions, then clean pushed-commit `just ci::validate-affected`
with changed paths and `RESULTS_DIR`. The manifest feature change selects the
full suite under current AGENTS.md. Cross-builds are not runtime evidence.
Record artifact SHA256s and commit in the card before booking.

On Windows, in the approved fixture directory only, run each prebuilt winapi
and host integration executable serially with
`--test-threads=1 -Z unstable-options --format json --report-time` and
`RUSTC_BOOTSTRAP=1`. Capture exact exit receipts, including expected ignored
helper tests separately from runnable test count. Do not infer success from
an empty selection. Existing immediate-grandchild and ConPTY cases remain gates.

On the colocated Linux runner, after Meta starts its independent DC observer:

```sh
TERRARIUM_NATIVE_RUN_AS_FIXTURE="$ROOT/fixture.json" \
TERRARIUM_NATIVE_APPROVED_RUN="$RUN_ID" \
TERRARIUM_URL=http://127.0.0.1:18480 \
uv run --with pytest==8.4.2 pytest -q scripts/windows_run_as_native.py \
  --junitxml="$ROOT/results/native-run-as.xml"
```

`fixture.json` contains no passwords. Required fields are enforced by
`validate_fixture`: full commit/run ID/namespace, isolated and production daemon
URLs, probe path, share/local paths, admin sentinel, protected host config path,
HKCU path/value, DC receipt path, wrong-password target, single-attempt approval,
and `a`, `b`, `c_filtered`, `c_full`, `d_disabled` objects. Each account object
names target, exact qualified account, actual SID, username/domain and own
scratch path. All values must be filled from approved fresh inventory. The
runner refuses a missing approval, mismatched endpoint or incomplete manifest.

The runner has nine native test methods. It checks A/B SID and user environment,
profile ownership/HKCU, ConPTY, parallel identity separation, C's filtered/full
token and admin sentinel, host-config write denial, local and SMB push/pull
including B deny and unchanged bytes, UNC access from actual user exec, one
wrong-password attempt and disabled D, immediate detached-grandchild timeout
containment under all four selected tokens, and independent DC evidence.
Meta's DC observer records namespace, A SID, actual SMB session/open-file
identity and file owner SID in `dc_receipt`. The runner holds an A-authenticated
SMB file open for 15 seconds so the observer can capture it. No receipt means
failure, not skip. The evidence must come from DC-side Get-SmbSession and
Get-SmbOpenFile, not an invented client assertion.

## Additional mandatory window assertions

- V1: actual durable `terrarium run` plus `run log` and exit receipt must report
  the same token/profile as exec. Exercise stdin backpressure, cancellation,
  timeout, and disconnect/reconnect/resume while retaining job containment.
  An isolated-host connection restart needs separate approved host process
  control. Reconnect must not replay credentials or create a second logon.
- V2: rotate A's isolated secret between two new sessions and prove the new
  session uses the new credential. Bound any old-password negative attempt
  separately against current lockout policy. Observe token/profile/handle
  cleanup after success/failure and parallel same-profile sessions.
- V3: submit a daemon frame containing `elevated=true` and prove it has no
  effect on C-filtered. Native read-only config preflight must reject a
  fixture with an untrusted write ACE and a fixture reparse ancestor without
  changing either. Source policy tests supplement, not replace, these checks.
- V4: collect redacted host/daemon TRACE-equivalent captures with a non-secret
  sentinel. Scan each unique password and its hex/base64 representations in
  logs/state/transcripts via a secret-aware harness that reports counts only.
  Do not output the seed, decrypted isolated secret store or host enrollment
  token. Do not interpret encrypted-at-rest secret storage as a plaintext leak.

## Window and recovery

Propose TER-272 alone: 0–10 minutes fresh exclusive-use/preflight/baseline and
hash checks, 10–40 minutes scripted cases and V1–V4, **final 15 minutes recovery**.
Stop new tests at the recovery cutoff. A missed or failing assertion books
another window, never a skipped acceptance gate. Keep pinned SSH recovery.

Lead parks affected workers with pushed handoffs and inventories sessions,
durable runs, reconnects and host jobs. Stop only consented owned IDs. Meta
confirms no other VM/share/account user and records original service hash,
ImagePath, SDDL and health before allowing M9. Cleanup removes only expanded
manifest objects after logon/profile/SMB drain, restores exact service selection
and settings, proves original version Running/online and baseline domain/share
health, and lists any residual item needing more approval. No routine DC
snapshot restore or broad recursive deletion. Resume parked workers only after
lead/Meta recovery clearance.

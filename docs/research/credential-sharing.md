# Research: many plan logins per provider, one settings folder per harness

Date: 2026-10-27. Method: static analysis of the installed harness code and
of public docs. No network request used the real credentials. No harness
login or logout ran. The files in `config/` were opened only to read their
key names; no token value was printed.

## Labels

- **[V]** verified: the statement comes from code or docs that we read. The
  source is next to it.
- **[I]** inferred: the statement follows from the code, but no code line
  says it directly, or the code is closed. A live check must confirm it.

## Sources

| Harness | Version | What we read |
|---|---|---|
| Claude Code | 2.1.310 | `tools/versions/claude/2.1.310/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`. A Bun binary. The JS is plain text inside it. The excerpts below quote minified names; grep the binary for the quoted marker strings (for example `tengu_oauth_token_refresh_lock_acquiring`). Minified names change with each build. |
| Codex | 0.164.0 | `github.com/openai/codex`, tag `rust-v0.164.0`, commit `6aaac694803004a63c6f393d153c96d22521a6b9`. Files: `codex-rs/login/src/auth/storage.rs`, `codex-rs/login/src/auth/manager.rs`, `codex-rs/login/src/auth/account_grant.rs`, `codex-rs/config/src/types.rs`, `codex-rs/core/src/client.rs`. |
| Copilot CLI | 1.0.103 | `tools/versions/copilot/1.0.103/.../copilot-linux-x64/copilot`. A Node SEA binary. Its ELF note section holds a gzip tar of `package/`. We extracted `package/cli-main.js` (JS UI layer) and `package/prebuilds/linux-x64/runtime.node` (a closed Rust runtime, 146 MB). The auth core is in the Rust runtime. Only its strings are readable. |
| pi | 1.4.0 | `tools/versions/pi/1.4.0/lib/node_modules/@earendil-works/pi-coding-agent/dist/core/auth-storage.js`, `dist/utils/paths.js`, `dist/config.js`, and `node_modules/@earendil-works/pi-ai/dist/auth/resolve.js`, `.../auth/oauth/{anthropic,openai-codex,github-copilot}.js`. Plain JS. |
| Docs | fetched 2026-10-27 | Claude Code authentication: <https://code.claude.com/docs/en/authentication>. Codex auth: <https://learn.chatgpt.com/docs/auth> (redirect from `developers.openai.com/codex/auth`). GitHub token expiry: <https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/token-expiration-and-revocation>. |

Terrarium context: `crates/terrariumd/src/sandbox.rs` binds each shared
harness home (`/auth/<harness>`) read-write at `/home/agent/.<harness>`.
Per-agent files are file binds over parts of it
(`crates/adapters/*/src/config.rs`). A rename over a file bind fails with
`EBUSY` (`docs/compat.md`, Copilot rows).

## Summary

| | Claude Code | Codex | Copilot CLI | pi |
|---|---|---|---|---|
| Credential file | `<dir>/.credentials.json`, `<dir>` = `CLAUDE_SECURESTORAGE_CONFIG_DIR` or `CLAUDE_CONFIG_DIR` | `$CODEX_HOME/auth.json` (fixed name) | `$COPILOT_HOME/config.json` `authTokens` (plain-text fallback) | `$PI_CODING_AGENT_DIR/auth.json` (fixed name) |
| Reads the file | before each model request (mtime check), and on a 401 | at start; before a proactive refresh; on a 401; only for the **same account id** | not known (closed code) | before each request (stat revision check) |
| Refresh trigger | 5 min before `expiresAt`, or on a 401 | 5 min before the JWT `exp`, or `last_refresh` older than 8 days, or on a 401 | no refresh of the GitHub token (`gho_`) | 5 min before `expires` |
| Refresh token rotates | yes [I, strong] | yes, with reuse detection [V] | not applicable | provider-specific |
| Write | temp file + rename; on `EBUSY`/`EXDEV`/`EPERM`/`EEXIST` an in-place truncate + write; mode 0600; no fsync; `O_NOFOLLOW` on the in-place path | in-place `O_TRUNC` write; mode 0600 at create; follows symlinks; no fsync | rename (inode changes) [V, compat.md] | in-place `writeFileSync`; mode 0600 at create; follows symlinks; no fsync |
| Cross-process lock | yes: `proper-lockfile` dir `<dir>/.oauth_refresh.lock`, also `<realpath(dir)>.lock` and `<dir>/.storage-write` | **no** (in-process semaphore and mutex only) | not known | yes: `proper-lockfile` dir `<auth.json path>.lock` (`realpath: false`) |
| Race handling | lock + re-check under lock + compare-and-swap on save | re-read before refresh; save refuses a changed login | not needed (no refresh) | lock + re-check under lock |
| Live swap to another account | yes, at the next request | **no**: Codex refuses to load another account id | yes, with `/user switch` (TUI dialog) [I] | yes, at the next request |
| Recommended mechanism | **B-dir**: `CLAUDE_SECURESTORAGE_CONFIG_DIR` points at a per-agent symlink to the account directory | **A + daemon pre-refresh**; swap = restart with resume | **env var per agent** (`COPILOT_GITHUB_TOKEN`); swap = restart with resume, or `/user switch` after a live test | **C**: per-agent copy, daemon is the only refresher |

## Claude Code 2.1.310

### 1. Where the credential lives

- [V] The plain-text store is `<O0()>/.credentials.json`. Marker:
  `function g(){let e=O0(),r=".credentials.json";return{storageDir:e,storagePath:Z(e,".credentials.json")}}`.
- [V] `O0()` reads an **undocumented** env var first:
  `function O0(){let e=process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR;if(e!==void 0)return(e||u(o(),".claude")).normalize("NFC");return Le()}`.
  `Le()` is the config dir (`CLAUDE_CONFIG_DIR` or `~/.claude`). So the
  credential directory can differ from the settings directory. The public
  docs name only `CLAUDE_CONFIG_DIR` (authentication page, "Credential
  management").
- [V] Keys (from `config/claude/.credentials.json`, names only):
  `claudeAiOauth.{accessToken, refreshToken, expiresAt, refreshTokenExpiresAt, scopes, subscriptionType, subscriptionTypeSource, rateLimitTier}`.
  The save function `Vw` also writes `clientId` and
  `subscriptionTypeVerifiedAt` when present.
- [V] Env alternatives: `CLAUDE_CODE_OAUTH_TOKEN` gives an access token with
  `refreshToken:null,expiresAt:null` (function `bp`). It is not refreshable.
  The docs say `claude setup-token` makes a one-year token, for model
  requests only (no Remote Control, no claude.ai connectors).
  `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` is also read (`dE()`), also with
  no refresh token.
- [V] On a 401 with `CLAUDE_CODE_OAUTH_TOKEN` set, Claude Code keeps the env
  token. Marker: `OAuth 401: keeping the user-supplied CLAUDE_CODE_OAUTH_TOKEN`.

### 2. When it reads the credential

- [V] The client builder `m$` runs the refresh check before it builds each
  client: `let Pt=await Eue({credentials:L,storageV5:j}),Vt=Ln()`. The
  retry loop `hRe` builds a new client for each model call (`w===null` at
  the start). So the check runs before each model request.
- [V] The check `Ru` starts with `await dA(e,S)`. `dA` stats the file and
  compares the mtime:
  `r=(await _p(hp(s,".credentials.json"))).mtimeMs ... if(r!==e.lastCredentialsMtimeMs){e.lastCredentialsMtimeMs=r,Nh(),mp(e);return}`.
  `Nh()` clears the in-memory copy, so the next `Ln()` reads the file again.
  `_p` is `stat`, so it follows symlinks.
- [V] On a 401, `WS` → `f$` clears the cache, reads the file again, and
  uses the token on disk when it differs from the failed one. Marker:
  `tengu_oauth_401_recovered_from_keychain`. Only when the disk has the same
  token does it force a refresh.
- [V] No file watcher. No timer.

### 3. How it refreshes

- [V] Endpoint: `TOKEN_URL:"https://platform.claude.com/v1/oauth/token"`,
  body `{grant_type:"refresh_token",refresh_token,client_id,scope}` (function
  `koe`).
- [V] Trigger: `function rC(e,n=Date.now()){if(e===null)return!1;return n+300000>=e}`.
  This is 5 minutes before `expiresAt`. A 401 forces a refresh.
- [I, strong] The refresh token rotates. `koe` keeps the old one only when
  the response has none (`refresh_token:j=e`). The code marks a refresh
  token "dead" on `invalid_grant` and has a compare-and-swap save that
  adopts a sibling's newer write. Both make sense only with single-use
  refresh tokens.
- [V] On `invalid_grant` (`N7`): first it reads the file again. If another
  process wrote a new access token, it adopts it
  (`tengu_oauth_token_refresh_race_recovered`). If not, `Cqr` marks the token
  dead and clears `accessToken`, `refreshToken`, and `expiresAt` on disk,
  **but only if the file still holds the same refresh token**. Marker:
  `tengu_oauth_refresh_token_cleared_on_disk`. The user then sees
  "Login expired · Please run /login".

### 4. How it writes the file

- [V] `re.write` calls `Dn(n,_(e),384)` then `chmod(n,384)` (mode 0600).
  `Dn` calls `ree` with no `flush` and no `followSymlinks`.
- [V] `ree`: writes a temp file `<path>.tmp.<8 hex>` in the same directory
  (`OI`, created with `wx`), then `fs/promises.rename` over the path.
- [V] If the rename fails with a code in
  `iP=new Set(["EXDEV","EPERM","EEXIST","EBUSY"])`, it falls back to an
  in-place write: `open(path, O_WRONLY|O_CREAT|O_NOFOLLOW|O_NONBLOCK)`,
  `truncate(0)`, `writeFile`. So a **file bind** works: the rename gets
  `EBUSY`, the fallback writes the bound file in place.
- [V] No fsync (`flush` is not set).
- [V] Symlinks: a **file** symlink at `.credentials.json` is replaced by the
  rename (the link goes away). The in-place fallback uses `O_NOFOLLOW` and
  refuses a symlink. A **directory** symlink (the credential directory is a
  link) works: the temp file and the rename both happen inside the target
  directory.
- [V] The hard-link refusal (`nlink>1`) is off in this build (`blr()` returns
  false).
- [V] Every save is a read-modify-write under the lock `<O0()>/.storage-write`
  (`proper-lockfile`, `stale:15000`). Function `S` → `dwn`.

### 5. Locking

- [V] `lri(O0())` takes two `proper-lockfile` locks:
  `<O0()>/.oauth_refresh.lock` (`stale:60000`, `update:5000`) and a legacy
  lock `${realpath(O0())}.lock` (a sibling of the directory). A failure to
  take the legacy lock for a reason other than `ELOCKED` is logged and
  ignored.
- [V] The lock holder re-reads the file under the lock. If the access token
  changed, it skips the refresh (`tengu_oauth_token_refresh_race_resolved`).
- [V] It retries `ELOCKED` 5 times with 1–2 s waits, then can take over a
  dead holder.
- So all processes with the same credential directory share one lock.

### 6. Multi-process behaviour

- [V] Two processes near expiry: one takes the lock and refreshes. The other
  waits, re-reads, sees a new access token, and stops. The save is a
  compare-and-swap (`wqr`): it writes only if the file still holds the
  refresh token that it posted, else it adopts the newer write
  (`tengu_oauth_refresh_save_adopted_newer_write`).
- [V] The other process sees the new token at its next request (mtime
  check).
- [V] `docs/compat.md` notes that the shared-home refresh with many agents
  was not tested live. The code supports it.

### 7. Effect of a swap

- [I] A swap is safe at the next request, because the mtime check reloads.
- [I] Risk: a swap while a refresh is in flight. The refresh holds the lock
  in account A's directory. Then the save resolves the path again. It reads
  account B's file. The compare-and-swap sees a different refresh token and
  "adopts" B. **Account A's new, rotated refresh token is then lost**, and
  account A needs a new login. The window is about one refresh (≤ 30 s
  timeout, usually under 1 s).
- [I] `.claude.json` (`oauthAccount`: email, org UUID) is a per-agent
  overlay. After a swap it still names account A. The display and some
  org-scoped calls can use the old org UUID. A live test must check this.

## Codex 0.164.0

### 1. Where the credential lives

- [V] `get_auth_file`: `codex_home.join("auth.json")` (`storage.rs`). The
  name is fixed. Only `CODEX_HOME` moves it.
- [V] Keys (struct `AuthDotJson`, and `config/codex/auth.json` names):
  `auth_mode`, `OPENAI_API_KEY`, `tokens.{id_token, access_token, refresh_token, account_id}`,
  `last_refresh`, plus optional `agent_identity`, `personal_access_token`,
  `bedrock_*`.
- [V] Store mode `cli_auth_credentials_store`: default `File`; also
  `Keyring`, `Auto`, `Ephemeral` (`config/src/types.rs`). The docs say the
  same.
- [V] Env alternatives: `CODEX_API_KEY` (API key, wins over all, when the
  env var is enabled), `OPENAI_API_KEY`, `CODEX_ACCESS_TOKEN` (a personal
  access token or an agent-identity JWT). None of them is a refreshable
  ChatGPT plan login. `CODEX_REFRESH_TOKEN_URL_OVERRIDE` changes the refresh
  endpoint.

### 2. When it reads the credential

- [V] `AuthManager` caches the auth. Its doc comment: "External modifications
  to stored credentials will NOT be observed until `reload()` is called
  explicitly."
- [V] `auth()` (called per request: `core/src/client.rs:1171`, and at many
  other sites) reads the disk only when a proactive refresh is due. Then
  `refresh_token_with_reason` calls `reload_if_account_id_matches` first. If
  the disk changed, it adopts the disk and skips the refresh ("Skipping token
  refresh because auth changed after guarded reload.").
- [V] On a 401, `UnauthorizedRecovery` step 1 is `Reload`
  (`reload_if_account_id_matches`), step 2 is `RefreshToken`.
- [V] **Account guard**: both paths reload only if the `account_id` on disk
  equals the cached one. Else: `ReloadOutcome::Skipped` and the error
  "Your access token could not be refreshed because you have since logged out
  or signed in to another account. Please sign in again."
- [V] No file watcher. No timer.

### 3. How it refreshes

- [V] Endpoint `https://auth.openai.com/oauth/token`, client id
  `app_EMoamEEZ73f0CkXaXp7hrann`, JSON body.
- [V] Trigger (`proactive_refresh_reason`): the access-token JWT `exp` is
  within 5 minutes (`CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5`). If no
  `exp` can be parsed: `last_refresh` older than 8 days
  (`TOKEN_REFRESH_INTERVAL = 8`). A 401 also triggers it.
- [V] Rotation with reuse detection: the error code `refresh_token_reused`
  maps to `RefreshTokenFailedReason::Exhausted`, message "your refresh token
  was already used".
- [V] On `invalid_grant` or 401 from the token endpoint: a permanent error.
  It is recorded for that auth snapshot. Later refreshes of the same snapshot
  fail fast with no network call. The file is not changed. No logout. A later
  reload that sees a changed file (same account) clears the failure.
- [I] Risk: with OAuth reuse detection, a reused refresh token can revoke the
  whole token family on the server. Then every agent on that login fails.

### 4. How it writes the file

- [V] `FileAuthStorage::save`: `OpenOptions::new().truncate(true).write(true).create(true)`,
  `mode(0o600)` (applies only at create), `write_all`, `flush`. In place. No
  rename. No fsync. `open` follows symlinks, so a symlink's target gets the
  write.
- [V] `persist_tokens` re-reads the stored file first. It refuses to write if
  the stored login is not the same login (`same_account_login`: same
  `login_owner_id`, `account_id`, and `chatgpt_user_id`): "Account credentials
  changed during refresh." So a refresh never overwrites another account's
  file.

### 5. Locking

- [V] Only in-process: `refresh_lock: Arc<Semaphore>` (1 permit) and the
  static `AUTH_STORAGE_COMMIT: Mutex<()>`. The comment says: "Protect
  in-process read-modify-write commits, never network requests." There is
  **no lock file and no flock**.

### 6. Multi-process behaviour

- [V] Two processes near expiry: each does a guarded reload, then a refresh.
  The reload protects only when the other process has already written.
- [I] All agents on one login have the same `exp`. Each enters the 5-minute
  window at its first request after `exp − 5 min`. Two refreshes in the same
  second are likely with many agents. The loser gets `refresh_token_reused`.
  It recovers when it reloads the winner's write, unless the server revoked
  the family.

### 7. Effect of a swap

- [V] A different account in the file is never adopted by a running Codex
  (account guard). A swap needs a restart. `codex resume <id>` keeps the
  session, so the conversation is not lost.

## Copilot CLI 1.0.103

### 1. Where the credential lives

- [V] `config/copilot/config.json`: `//` comment lines, then
  `authTokens["https://github.com:<login>"].token` and
  `authTokens["https://github.com:<login>:github"].token` (same token),
  `loggedInUsers[]`, `lastLoggedInUser{host,login}`. The token prefix is
  `gho_` (length 40). We printed only the prefix.
- [V] The native runtime strings name `authTokens`, `copilotTokens`,
  `loggedInUsers`, and "// This file is managed automatically.".
- [V] Precedence (`docs/compat.md` and `crates/adapters/copilot/src/auth.rs`):
  env (`COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`) > keychain
  (libsecret; not reachable in the sandbox) > plain-text `config.json`.
  `cli-main.js` lists `$8o=["COPILOT_GITHUB_TOKEN","GH_TOKEN","GITHUB_TOKEN"]`.
- [V] The login is the device flow (`/login/device/code`,
  `/login/oauth/access_token`) with client id `Ov23ctDVkRmgkPke0Mmm` (runtime
  strings). An `Ov23…` id is an OAuth App, and its tokens are `gho_`.

### 2–3. Read and refresh

- [V, docs] GitHub OAuth App tokens have no fixed expiry. GitHub revokes one
  when it is unused for one year, or on other events (pushed to a public
  repo, user or owner revocation, more than ten tokens per user/app/scope).
  GitHub App user tokens (`ghu_`) expire after 8 hours and use `ghr_`
  refresh tokens. Copilot CLI stores `gho_`, so it has no refresh.
- [V] The strings `copilot_internal/v2/token` and `v2/token` are **absent**
  from `runtime.node`, `cli-main.js`, and the outer binary. The runtime has
  `https://api.githubcopilot.com` and `copilot_internal/user`.
- [I] So version 1.0.103 sends the GitHub token itself to the Copilot API,
  with no per-process token exchange. (pi's Copilot provider still uses
  `copilot_internal/v2/token`; see below.)
- [I] When the runtime re-reads `config.json` is not known. The string
  "Credential reload using plaintext fallback" shows a reload path exists.

### 4. How it writes

- [V, compat.md] Copilot replaces `config.json` by a rename (the inode
  changes). A rename over a file bind fails with `EBUSY`. So `config.json`
  must not be a file bind.
- [V] `settings.json` gets the same rename write (`--experimental` case,
  `EBUSY` and exit 1).

### 5–6. Locking and many processes

- [I] Not known (closed Rust). There is a `settings.json.lock` in
  `config/copilot/`. Because the token is not refreshed, two processes do not
  race on a refresh. They can race on other `config.json` writes
  (`loggedInUsers`, `firstLaunchAt`, flags).

### 7. Native multi-account support

- [V] Copilot keeps many logins: `/user show`, `/user list` ("List all
  authenticated users"), `/user switch` ("Switch to a different user",
  `allowDuringAgentExecution:!1`, opens the `user-switcher` dialog).
- [V] The switch keeps the session: `nt.switchUser(F,w.selectionId)` with the
  current session id `F`, then "Switched to <login>" (`cli-main.js`).
- [I] The selected account is probably also saved as `lastLoggedInUser` in
  the shared `config.json`. Then a new agent starts on the last switched
  account. A live test must check this.

## pi 1.4.0

### 1. Where the credential lives

- [V] `getAuthPath()` = `join(getAgentDir(), "auth.json")`. `getAgentDir()`
  reads `PI_CODING_AGENT_DIR`. No env var moves `auth.json` alone.
- [V] Format: one object per provider id. OAuth:
  `{type:"oauth", access, refresh, expires, ...}`. API key:
  `{type:"api_key", key, env}` (`ReadOnlyAuthStorage.load`). The current
  `config/pi/auth.json` is `{}` (2 bytes): pi has no OAuth login today.
- [V] Providers with OAuth: `anthropic` (token URL
  `https://platform.claude.com/v1/oauth/token`, Claude Code's client id),
  `openai-codex` (`https://auth.openai.com/oauth/token`, Codex's client id),
  `github-copilot` (`refresh` is the GitHub token; `access` comes from
  `copilot_internal/v2/token`), and others.

### 2. When it reads the credential

- [V] Every `read()` calls `readLatestData()`. It compares
  `getFileRevision(path)` = `` `${dev}:${ino}:${size}:${mtimeNs}:${ctimeNs}` ``
  with the last revision. On a change it reloads under the lock. So pi sees a
  new file content at its next request.

### 3. How it refreshes

- [V] `resolveStoredOAuth`: refresh when `Date.now() + 5 min >= expires`.
  (For `anthropic`, the stored `expires` is already 5 minutes early, so the
  refresh starts 10 minutes before the real expiry.)
- [V] `refreshStoredOAuthCredential` runs inside `credentials.modify()`
  under the lock. It re-checks `needsRefresh(current)` with the file content
  read under the lock. If another process refreshed, it does nothing.
- [V] On failure: it throws `ModelsError("oauth", "OAuth refresh failed for
  <provider>")`. The file is not changed. No logout. The next request reads
  the file again.
- [V] Rotation: `anthropic` stores `refresh: data.refresh_token` (a new one
  each time). `github-copilot` keeps the GitHub token as `refresh` [I].

### 4. How it writes the file

- [V] `writeFileSync(this.authPath, next, {encoding:"utf-8", mode:0o600})`.
  In place (`O_TRUNC`). Mode only at create. Follows symlinks. No fsync. No
  rename. A file bind works.

### 5. Locking

- [V] `proper-lockfile` with `realpath: false` on the auth path, so the lock
  is the directory `<PI_CODING_AGENT_DIR>/auth.json.lock`. Async: `stale`
  30 s, retry with back-off up to 30 s. Sync: 10 tries, 20 ms apart.
- [V] With `realpath: false`, the lock name comes from the path, not the
  target. All agents that see `/auth/pi/auth.json` (bind or not) share the
  lock in the shared home. Agents with different symlink paths do **not**
  share a lock.

### 6. Multi-process behaviour

- [V] With one shared file: lock + re-check under the lock. Only one process
  refreshes. Correct.
- [V] With per-agent **copies** behind one shared lock name: the re-check
  reads the agent's own copy, which another agent did not update. A second
  agent then refreshes with a refresh token that the first agent already
  used. So copies need one refresher outside pi (option C).

## The options against the findings

### (A) Bind the account file over the agent's credential path (rw)

| Harness | Works? | Notes |
|---|---|---|
| Claude | yes [V] | Rename gets `EBUSY`, the in-place fallback writes. A temp file `.credentials.json.tmp.*` appears for a moment in the shared home. The lock is in the shared home, so all accounts share one lock (only extra waiting). No live swap. |
| Codex | yes [V] | In-place write. No live swap (account guard, and no rebind). Refresh race between agents (no lock). |
| Copilot | **no** [V] | `config.json` is written by rename → `EBUSY`. |
| pi | yes [V] | In-place write. Shared lock name in the shared home. No live swap. |

### (B) Per-agent directory with a symlink that the daemon swaps

- Claude, file symlink: **no** [V]. The rename replaces the link with a
  regular file. Then the agent is cut off from the account file.
- Claude, **directory** symlink through `CLAUDE_SECURESTORAGE_CONFIG_DIR`:
  **yes** [V for the code paths, I for the full flow]. Temp file, rename, and
  both lock files resolve inside the account directory. All agents on one
  account share the lock and the compare-and-swap. The mtime check catches a
  swap at the next request.
- Codex: the path is fixed at `$CODEX_HOME/auth.json` in the shared home. A
  per-agent symlink there needs a bind over the file in each sandbox, which
  is option A. Also the account guard blocks a live swap. **Not useful.**
- Copilot: rename over the link. **No.**
- pi: the path is fixed in the shared home, same problem as Codex. Also the
  lock name comes from the link path, so agents on one account do not share
  a lock. **Not useful.**

### (C) Daemon-owned refresh and sync, per-agent copies

- The daemon refreshes each account well before the harness's own window
  (for example 30 minutes before expiry). It writes the new tokens into every
  copy of that account in place. Then no harness reaches its own refresh
  window, so the daemon is the only refresher of each refresh token. This
  avoids two refreshers.
- Sync-back: if a harness refreshes anyway (daemon down, clock skew, a 401),
  inotify on the copies sees the write. The daemon copies the new tokens to
  the account file and to the other copies at once. Between the harness
  write and the sync, another agent on the same account can still refresh
  with the old refresh token. Keep the daemon margin large to make this rare.
- Claude: works [I]. The mtime check picks up the daemon write at the next
  request. Swap = write the other account's tokens into the copy. The
  daemon must also take the harness's `.oauth_refresh.lock` (same
  `proper-lockfile` protocol: a `mkdir`) while it writes, so a harness
  refresh does not interleave.
- Codex: works for refresh [I]. Codex keeps the old access token in memory
  until its own 5-minute window, then the guarded reload adopts the daemon's
  newer file and skips its refresh [V]. This needs the old access token to
  stay valid until its `exp` [I]. Swap: **no** (account guard).
- Copilot: not needed (no refresh).
- pi: works [V for the read path]. Every request checks the revision. Swap
  takes effect at the next request; pi has no account guard.
- Cost: the daemon must run each provider's OAuth refresh with the
  harness's client id. A daemon bug can burn a refresh token for every agent
  of that account.

### (D) Auth proxy that injects the current account's token

- Claude: `ANTHROPIC_BASE_URL` changes only the model API. Subscription
  features use other hosts and endpoints (claude.ai connectors, profile, MCP
  connectors, Remote Control, usage). Claude Code also changes behaviour for
  a non-default base URL (first-party checks). The dummy stored credential
  would still trigger refresh attempts. **Not recommended.**
- Codex: `chatgpt_base_url` and the model provider URL can point at a proxy.
  The proxy must handle the Responses websocket, rewrite `Authorization` and
  the ChatGPT account-id header, and keep `id_token` claims that Codex reads
  locally (plan type, account id). It is the only way to swap Codex live.
  High effort, fragile on each Codex release. **Not recommended now.**
- Copilot: the GitHub token goes to `api.github.com` and to
  `api.githubcopilot.com`. A proxy needs TLS interception of both. **No.**
- pi: custom providers in `models.json` already allow a base URL. Possible,
  but option C is simpler.

## Recommendation per harness

### Claude Code: B-dir

1. Keep `CLAUDE_CONFIG_DIR=/auth/claude` (shared settings).
2. Set `CLAUDE_SECURESTORAGE_CONFIG_DIR` to a per-agent path, for example
   `/state/agents/<id>/cred/claude` (a symlink).
3. Point the symlink at `state/credentials/accounts/<subscription>/claude/`.
   That directory holds `.credentials.json`. Bind the account directories
   that the agent may use, at the same path inside and outside the sandbox.
4. To swap: the daemon takes `.oauth_refresh.lock` in the old and the new
   account directories (mkdir, `proper-lockfile` format), replaces the
   symlink atomically (new link + rename), and releases the locks.

- Live swap: at the next model request (mtime check). The session stays.
- Refresh race: handled by Claude Code itself. One lock and one
  compare-and-swap per account directory.
- Verify live: (a) 2.1.310 honours `CLAUDE_SECURESTORAGE_CONFIG_DIR` for
  read, write, and `claude auth status`; (b) a refresh writes through the
  directory link (force it with a copied, expired test login, not Wil's);
  (c) the swap reaches the next request; (d) what a stale `oauthAccount` in
  the per-agent `.claude.json` breaks; (e) whether `<realpath>.lock` beside
  the account directory needs a writable parent (the code ignores a failure).
- Risk to the real logins: an undocumented env var can change in a later
  release. The swap-during-refresh case loses a rotated token unless the
  daemon takes the lock. Every agent can read the tokens of each account
  directory that is bound into its sandbox.
- Fallback: option C with per-agent copies, if the env var fails live.

### Codex: A plus daemon pre-refresh; swap by restart

1. Bind the account file `state/credentials/accounts/<subscription>/codex/auth.json`
   over `$CODEX_HOME/auth.json` in each agent (rw). Keep the rest of
   `/auth/codex` shared.
2. The daemon refreshes each account about 30 minutes before the access
   token `exp`, with a lock of its own. It writes the file in place (same
   format, `last_refresh` updated). Then the agents never reach their own
   5-minute window, and the reuse race between agents does not occur.
3. To swap: stop the agent, change the bind source, start it with
   `codex resume <session-id>`.

- Live swap: never without a restart (account guard [V]).
- Refresh race: Codex has no cross-process lock [V]. The daemon pre-refresh
  removes the race. If the daemon is down, the agents refresh in place on
  the shared file. The race can then come back, but the guarded reload and
  the "same login" save check limit the damage.
- Verify live: (a) an old access token works until its `exp` after a refresh;
  (b) a running Codex adopts a daemon-written file at its 5-minute window
  with no own refresh (the log line "Skipping token refresh because auth
  changed after guarded reload."); (c) resume after a swap keeps the
  conversation.
- Risk: if the server revokes a token family on reuse, one race kills the
  login for all agents on it. Test the daemon refresh with a throw-away
  login first.

### Copilot CLI: env token per agent; swap by restart, or `/user switch`

1. Store each account's `gho_` token in the daemon (`state/credentials/accounts/<subscription>/copilot/token`).
2. Launch each agent with `COPILOT_GITHUB_TOKEN=<token>`. The env var wins
   over `config.json` [V]. Nothing writes a token file, and nothing
   refreshes.
3. To swap: restart with resume. Or, after a live test, keep all accounts in
   the shared `config.json` and send `/user switch` to the TUI between turns.

- Live swap: by restart now. With `/user switch` the session stays [V], but
  the dialog is interactive and is refused during agent execution [V].
- Refresh race: none (`gho_` tokens do not expire [V, docs]).
- Verify live: (a) `COPILOT_GITHUB_TOKEN` with a `gho_` token works with no
  `config.json` login; (b) whether Copilot 1.0.103 still exchanges for a
  Copilot API token (no `copilot_internal/v2/token` string was found); (c)
  whether `/user switch` writes `lastLoggedInUser` to the shared `config.json`;
  (d) whether `/user switch` can select a stored account while an env token
  is set.
- Risk: an env var is visible to every process in the sandbox (the agent can
  read it). The same is true for the plain-text `config.json` today. A token
  that is unused for a year is revoked by GitHub.

### pi: C (per-agent copy, daemon is the only refresher)

1. Each agent gets a per-agent `auth.json` copy, bound over
   `$PI_CODING_AGENT_DIR/auth.json` (rw). The copy holds the provider entries
   of the agent's accounts.
2. The daemon refreshes each OAuth account 30 minutes before `expires`. It
   writes all copies of that account in place, under pi's lock name
   `/auth/pi/auth.json.lock` (mkdir) so no pi process reads a half write.
3. inotify on the copies: if pi refreshes anyway, the daemon copies the new
   tokens back at once.
4. To swap: the daemon writes the other account's entry into the copy.

- Live swap: at the next request (revision check [V]). The session stays.
- Refresh race: pi's lock is shared, but its re-check reads only the own copy.
  So the daemon must stay the only refresher. A shared file (option A) is
  safe for refresh but has no per-agent swap.
- Verify live: (a) a daemon write reaches the next pi request; (b) pi does
  not refresh while the daemon keeps `expires` far ahead.
- Risk: today pi has no OAuth login (`auth.json` is `{}`). Wait with this
  work until pi uses a plan login.

## Open questions for Wil

- Is it acceptable that an agent can read the tokens of every account that
  its sandbox can swap to (Claude B-dir binds those account directories)?
- May the daemon run OAuth refreshes with the harness client ids (options C
  and the Codex pre-refresh)? This is what pi already does.
- For Codex, is a restart with `codex resume` an acceptable "swap"?

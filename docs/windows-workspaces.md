# Windows account workspaces (TER-274)

A Windows host agent can hold agent workspaces at
`C:\Terrarium\work\<project>\<agent>`. `terrarium cp`, the default exec
cwd and the managed `terrarium repo checkout --target` use them.

## Turn it on

Off by default. The host file turns it on, and only a protected host file
(the `[windows_run_as]` rule, docs/windows-run-as-acceptance.md D3) is
honoured:

```toml
[windows_workspaces]
enabled = true
```

The host reports `no_workspaces = false` only when the opt-in is on and
`C:\Terrarium\work` passes the checks below. Otherwise it keeps the TER-272
path handling and reports `no_workspaces = true`.

## Install prerequisite

Install creates `C:\Terrarium` and `C:\Terrarium\work`. The host never
creates them. Each must be owned by SYSTEM or Administrators, with a
protected DACL. A plain directory under `C:\` inherits Modify for
Authenticated Users and fails the check.

| Directory | Untrusted trustees must not have |
|---|---|
| `C:\`, `C:\Terrarium` | delete, delete child, write DACL, write owner, generic write or all |
| `C:\Terrarium\work`, `<project>` | all of the above, and add file, add subdirectory, write EA, write attributes |
| `<project>\<agent>` | any allow ACE. Only SYSTEM, Administrators and the run-as account |

The host service must run as LocalSystem, so that the directories it
creates are owned by SYSTEM or Administrators.

## How a path is opened

- The daemon checks the path lexically (TER-969). The host splits it into
  `<project>`, `<agent>` and the components below.
- The service opens every component from `C:\` to the agent directory by
  itself (`FILE_FLAG_OPEN_REPARSE_POINT`), without `FILE_SHARE_DELETE`, and
  holds it for the whole operation. It creates a missing project directory
  (inherited DACL) and a missing agent directory (protected DACL: SYSTEM,
  Administrators, the run-as account SID, inherited by children).
- An existing agent directory that grants another account, or that was
  made for service mode, is refused. It is never re-permissioned.
- The components below the root are opened as the run-as account
  (impersonation), with the same no-reparse open and hold.
- Each handle is refused when it is a reparse point (junction, symbolic
  link, mount point), the wrong type, or when its normalized final path
  differs from the requested spelling (8.3 short names).
- A push writes a `CREATE_NEW` temporary file in the held directory and
  renames it by handle. A failed or cancelled push deletes it by handle.
- A pull refuses a file with more than one hard link.
- An exec cwd in the base is provisioned and checked before the spawn.

## Managed git checkout

Off unless the host also names a trusted Git for Windows root:

```toml
[windows_git]
root = 'C:\Program Files\Git'
```

- The host reports `windows_git_checkout` only with workspaces on and a
  root whose whole tree passes the audit: no reparse point, alias or
  unsupported name, every directory and file protected against untrusted
  change (at most 100000 entries, depth 32). Each checkout audits it again.
- The daemon sends the typed `git.checkout` frame only to such a host, only
  for an agent, only for a target with `windows_run_as`, always in `managed`
  mode (provenance and push-deny hooks), into that agent's workspace. Any
  other case is refused before a frame. The host refuses a checkout without
  a run-as logon too: git never runs as the service.
- Git runs from the root by exact path, as the run-as account, in a job
  object, with the `-c` overrides of the Linux checkout script, a pinned
  `PATH`, `GIT_CONFIG_NOSYSTEM=1` and hooks off during clone or fetch.
- SSH uses `StrictHostKeyChecking=yes` with the daemon's pinned host keys
  only. No pin, no SSH checkout.
- The token goes only into the git child env. The SSH key and host keys are
  delete-on-close files in a session directory of `C:\Terrarium\git`.
  SYSTEM has full control there, the account read and execute only, so it
  cannot add a hook or replace the pinned host keys. Cancel waits for the
  job to drain. A failed delete of the session directory fails the checkout.
- Hooks are written by handle through the no-reparse walk. A repo hook that
  is not managed is kept as `<name>.repo`. A new hook never replaces a file
  that appeared after the check, and a failed install restores the repo
  hook.

## Paths that name the base another way

While `[windows_workspaces]` is on, the host refuses these before it opens
anything, so they never reach the TER-272 handling, which follows links:
a verbatim or device prefix (`\\?\`, `\\.\`), a component with `~` on a
drive path (8.3 short names), a UNC path to an administrative drive share
(`\\host\C$`), and any path whose existing prefix resolves into
`C:\Terrarium\work` (a junction or link), checked as the account.

## Limits

- R1: the DACL separates different ordinary accounts only. The same
  account and elevated administrators are not isolated. A command that runs
  as the account keeps all of its rights.
- Paths outside `C:\Terrarium\work` keep the TER-272 handling.
- The checkout token is readable by the same account and administrators
  while git runs.
- Unproven until native tests: msys `ssh.exe` reading a delete-on-close key
  file, the hook `-x` test of Git for Windows `sh`, and crash cleanup. That
  the kernel removes the credential files after a host crash is NOT proven,
  and an empty session directory can stay in `C:\Terrarium\git`.

## Native acceptance (not yet run)

Cross-builds are not runtime evidence. Native acceptance needs an approved
window, fixture accounts and roots, and a matching deployment:

- workspace creation and DACL for two different accounts, and refusal of
  the other account's workspace
- cp push and pull hashes, nested mkdirs, cancel leaves no temporary file
- junction, symbolic link, intermediate reparse point, 8.3 name and hard
  link escapes refused, outside sentinels unchanged
- a bounded rename-swap race on an inner directory during a push
- the default exec cwd for a run-as session
- checkout: token and SSH clone with an exact head hash, fast-forward,
  diverged, local changes, config injection refused, hooks installed and
  enforced, repo hook kept, pin missing or mismatched, unprotected or
  reparse runtime refused, cancel leaves no key file and no git process

# Windows account workspaces (TER-274)

A Windows host agent can hold agent workspaces at
`C:\Terrarium\work\<project>\<agent>`. `terrarium cp` and the default exec
cwd use them. A git checkout on a Windows host is refused for now
(TER-1102).

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

## Limits

- R1: the DACL separates different ordinary accounts only. The same
  account and elevated administrators are not isolated. A command that runs
  as the account keeps all of its rights.
- Paths outside `C:\Terrarium\work` keep the TER-272 handling.

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

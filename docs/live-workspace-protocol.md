# Live workspace model and isolated helper (B1/B2)

The B1 model is protocol, storage and state-machine work only. B2 adds an
isolated root helper, described below. There is no daemon route, host
capability advertisement or production caller.
`Activation` can represent only `disabled`. Neither writing a schema row nor
negotiating a compatible version activates anything. Existing targets and
workspace execution paths are unchanged. B4 must enforce reservations across
all execution paths before any activation is permitted.

## Identity and ordering

The enrollment binds a random workspace ID to host and agent ULIDs, a mapping
hint and an opened directory's device/inode. Paths and agent names are not
identity selectors. Immutable enrollment rows survive agent deletion. Unknown
history cannot be reserved. Permanent reservation rows bind that workspace to
one domain, one optional current Docker registration, and one optional owning
sandbox incarnation. Revoke and target destruction clear references, not the
domain seal. B4 must check actual history and authority in a FULL-synchronous
`BEGIN IMMEDIATE` transaction, not trust a supplied `fresh` value.

Insert-conflict guards reject replacement inserts and upserts on existing
enrollment, reservation, registration, attachment and operation identities,
including alternate unique keys. They work even with SQLite recursive triggers
disabled. Daemon connections also enable recursive triggers as defence in depth.
Identity-column UPDATE guards apply in every state, including revoked and
destroyed history. They compare OLD/NEW values with null-safe `IS NOT`, so
same-value identity assignments can accompany legal state updates. A revoked
attachment ID cannot be renamed to permit its reuse. All five tables use
`WITHOUT ROWID`: neither `rowid`, `_rowid_` nor `oid` provides an unguarded
replacement identity. All declared alternate unique keys are guarded too.
Only explicit state edges are mutable: target verification follows pending
reconciliation, destruction is terminal, and attachment revoke proceeds through
authority-ended and pending before verified disappearance. Quarantine permits
retry of revoke, never resurrection of an attachment.

One stream per registration covers all lifecycle operations. Original requests
are typed structs without maps. The v1 canonical hash is SHA-256 of compact
`serde_json::to_vec(Request)` output in declared field order. The original body
and hash are immutable. A superseded grant changes only its separate delivery
envelope. It consumes the original revision with no publication. Once helper
intent exists, changing delivery is a conflicting retry. Cancellation of an
already-dispatched grant requires ordered recovery/revoke, not rewriting it.

New helper intents require exactly `last_contiguous_revision + 1`. Gaps change
nothing. An exact committed retry returns the stored result without repeating
an external action. An exact uncommitted retry is busy. Conflicting duplicate
IDs, stale revisions and runtime mismatches fail. Revoke intent writes the
attachment tombstone before any external action. A later grant needs a new
attachment ID and current authority. Container IDs and namespace identities
are checked independently of names and numeric PIDs.

## Durable boundaries and recovery

The serializable `Journal` is a model, not a durable journal implementation.
B2 must protect its storage, bound loading, use synced append/atomic replacement,
sync journal and scaffold parent directories, and verify actual kernel state.
The model's `advance` records that verification has happened, it does not
perform or prove it. Host and receiver mount evidence are separate
namespace-scoped records. Equal mount IDs across views prove nothing.

The required sequence is daemon FULL-synchronous intent plus reservation,
helper synced intent, externally verified transitions with synced records,
helper synced committed result, acknowledgement, then daemon runtime commit.
`DaemonOperation` separates the last two boundaries and rejects mismatched
acknowledgements. `runtime.attached` is never write-ahead intent.

Grant phases are prepared, staged, stage verified, published, verified and
committed. Revoke phases are prepared, ordinary unmount observed, optional
lazy detach observed, placeholder removed, verified and committed. B2 must
verify private propagation and `nosuid,nodev` before publication. Revoke
verification means fresh-path disappearance in the pinned namespace, not
revocation of descriptors, cwd, maps or processes.

Without fresh external evidence every uncommitted crash state quarantines.
This includes a crash after an external action but before recording its result.
The B1 model does not clear quarantine or infer successful mutation from a
saved phase. Missing or divergent committed history quarantines too. B2/B4
must implement evidence-based recovery of exact operation identities, and
must never skip a revision, recreate a placeholder on revoke recovery, erase
a domain seal or silently delete an unknown mount.

Recovery reconstructs the attachment tombstones from revoke history and requires
an exact match with the saved map. Missing, changed or orphan tombstones are
divergence, not permission to reuse an attachment. Multiple revoke intents retain
the highest revision for that attachment.

Frames are at most 64 KiB and deny unknown request fields. Capability matching
requires protocol/state versions, the separate workspace-mount right and
startup readiness. Compatibility alone is not authorization or proof that the
pinned kernel/Engine/security profile works. Those checks, privileged tests,
installation and activation belong to later reviewed cards and the disposable
VM proof, never a shared build host.

## Isolated root helper (B2, TER-925)

`terrarium-workspace-helper` is a separate executable, not a root mode of the
self-updating host agent. Nothing installs, starts or advertises it. M1a may
install it inactive only after B2/B3 review. M1b may start it only on the
disposable proof VM after B4 review. Production enablement still requires B5.
`--capabilities` always reports `ready=false` while the daemon gate is disabled.

The service reads only `/etc/terrarium-workspace-helper/config.json`. The
root-owned regular config must not be group/world writable or hard-linked.
Its strict, bounded fields are `host_agent_uid`, `host_agent_gid`, `work_root`
and `registrations`. Each registration supplies root-protected `publisher_root`,
`staging_root` and `state_root`. These are administrator configuration, never
request fields. An empty registration list authorizes no operations. The
service cannot create a registration, bootstrap a missing journal, create a
Docker target or opt in an existing target. Provisioning and admission remain
separate reviewed work. Never create real registration journals before B4.

Every administrative ancestor is opened through no-follow directory FDs and
must be root-owned without group/world write permission. The socket directory
`/run/terrarium-workspace-helper` must already be root-owned, mode 0750, and
have the configured host-agent group. The socket is root-owned, mode 0660 with
that group. Every connection also checks SO_PEERCRED against the configured
non-root host-agent UID. This authenticates the trusted forwarder, not a lease.
The daemon/host-agent/Docker authority boundary from TER-750 still applies.
No generic host verb exposes this protocol. Link-originated grants are refused.

Before opening any store (including creating its lock), startup opens every
registration root and runs a read-only confinement preflight over the whole
configuration. State, config and socket directories must be disjoint from all
work, publisher and staging trees. This checks pinned root/ancestor dev+inode
identities in both directions, namespace paths, and filesystem-relative mount
roots. It also inspects mounted descendants for aliases of control directories
or files into a workspace. A bind alias of a state subtree cannot evade the
check by having a different administrative pathname. No helper is constructed
and no startup snapshot is written before every registration passes.

Frames have a four-byte big-endian byte length followed by the B1 envelope,
at most 64 KiB, with a five-second connection deadline. Replies use the same
framing and a typed `Reply`. Unknown fields, paths, namespace PIDs and arbitrary
mount flags are not accepted. Operations select an existing registration by
immutable ID, then require the journal's exact host/target/runtime identity.
Only grant, revoke, superseded grant and reconcile are supported. Registration
and lifecycle requests return `unsupported` without modifying the journal.

Each journal uses an exclusive lifetime lock. Snapshot files are root-owned,
single-link regular files with no group/other access, bounded at 8 MiB. Updates
create an exclusive `journal.next`, sync it and its parent, rename to
`journal.json`, then sync the parent again before acknowledgement. A leftover
next file, malformed state, write failure or ambiguous kernel transition fences
admission. No history is pruned to fit the limit. Committed exact retries never
repeat a syscall. Restart requires both the B1 history checks and exact fresh
host/receiver topology matching the last committed evidence. Unknown mounts
are quarantined, never implicitly deleted. Uncommitted crash states remain
quarantined rather than guessing whether a syscall completed.

Docker inspection uses a pinned root-owned `/usr/bin/docker` with a fixed local
socket and no PATH, shell or inherited environment. Output and execution time
are bounded. Registration/runtime labels are
`terrarium.workspace.registration` and `terrarium.workspace.runtime`.
The immutable full container ID, running/paused/restarting state, Docker start
timestamp, proc start ticks and mount-namespace identity are rechecked around
FD acquisition and verification. B3 must additionally validate the full Docker
security configuration and pinned profile. Inspect alone never proves attached.

The kernel backend requires a private independent staging mount and an isolated
shared per-registration publisher prepared by B3. It resolves the enrolled
workspace through openat2 beneath/no-symlink/no-magic-link/no-XDEV constraints,
checks device/inode, and scans without following links for nested mounts,
sockets, devices and FIFOs. The scan is bounded. Later socket creation cannot
be filtered by bind mounts: B4's exclusive writer/domain seal is mandatory.
Within a helper journal, workspace ID, enrollment and domain remain immutable
across revoke; aliases cannot acquire a new identity.

The source is cloned non-recursively through a pinned open_tree FD, attached to
private staging, explicitly made private, restricted with mount_setattr, and
verified before publication through move_mount to a new protected placeholder.
Workspace mounts are `rw,nosuid,nodev` and deliberately permit exec. The host
publisher remains writable for administrative placeholder changes. The receiver
scaffold is read-only and slave-only. Mount IDs and parent IDs are checked
within their own namespaces, never equated across namespaces. Extra children,
wrong source/root, flags or propagation relationships quarantine admission.

Revoke first attempts ordinary unmount. Only EBUSY permits a separately recorded
lazy detach. It removes the protected placeholder and verifies fresh lookup
ENOENT in both pinned views before committing `path_revoked_verified`. Open FDs,
cwd, mmap and copied data survive. A retained directory FD can still perform new
relative lookups. B4's permanent trust-domain seal is not released by revoke.

Ordinary unmount retains a pinned root-only parent and validates the derived
leaf's mount ID and source identity immediately before the syscall. It closes
its own lookup FD first, so it does not manufacture EBUSY by keeping an open
handle on the very mount being unmounted. The final component is no-follow.
Lazy detach reopens and revalidates the same mount when genuine users cause
EBUSY. Namespace and journal FDs remain pinned throughout.

The only unsafe code is the operator-approved `terrarium-sys` mount_setattr
wrapper. Its API borrows an FD and accepts bounded typed attributes. Every other
crate retains the workspace forbid lint, checked by a regression test. The
privileged fixture is ignored by ordinary checks and must be run explicitly on
the disposable VM in a new private mount namespace with
`TERRARIUM_B2_DISPOSABLE=1`. Skipping it is not kernel evidence. No helper service
is installed or activated by any test.

The interrupted-state disposable test aborts a separate helper child with
SIGABRT immediately after real staging, private/attribute, publication,
ordinary/lazy unmount and placeholder-removal syscalls, before their next
durable phase record. It also aborts after committed snapshots before grant
and revoke replies. Parent and helper child share only the outer private test
mount namespace, so the mutations survive child death. Reopening the real
backend must quarantine pending state or return the exact committed retry,
without changing the surviving mount table, deleting unknown mounts or
resurrecting a revoked path. An unrelated holder and its canary must survive
every case. Fault hooks and the aborting child probe are compiled only into
test builds, never the installed executable.

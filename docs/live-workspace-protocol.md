# Inactive live workspace model (B1, TER-924)

This is protocol, storage and state-machine work only. There is no helper
binary, socket, route, host capability advertisement or production caller.
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

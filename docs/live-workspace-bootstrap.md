# B4 registration bootstrap

This is an implementation contract, not activation approval. The daemon gate
and helper readiness stay disabled. No production bootstrap caller is enabled.
M1a installs inactive files only. M1b requires B4 security review, and production
requires B5. Administrator-written operational journals are not an alternative.

## Ownership and ordering

A bootstrap request contains immutable operation, host, target, registration,
runtime-incarnation and trust-domain IDs. It contains no pathname, PID, Docker
name selector, mount flags or arbitrary Docker options. The helper derives
administrative children from registration IDs beneath configured protected
parents. Peer authentication is necessary but does not replace daemon authority.
Link provenance is rejected without operator-route exceptions.

The request also binds the canonical digest of the helper-selected receiver
specification (pinned image digest, fixed command and runtime profile). The
helper must match that digest against its protected configuration before any
intent or side effect. A caller supplies no image, command or profile override.
Changing the helper specification requires a new operation, never replay under
the old identity. A configured profile alone is not B5 approval or activation.

Canonical receiver encoding is fixed-field-order compact JSON with domain and
format `terrarium.workspace.receiver/v1`, no maps. It contains the specification
and the effective fixed security options, receiver bind policy, administrative
parents, receiver-name template, identity-label keys and host-mount policy
version. The same security-option and bind-policy constants drive create flags.
An effective policy or encoding change must update its digest contract and
negative tests. Every retry and recovery must recheck protected configuration,
even when returning a stored bootstrap result. Digest equality alone supplies
neither fresh runtime evidence nor runtime-profile approval.

Receiver configuration inspection must match the bound full container ID,
all three identity labels, the helper-derived name, exact pinned image reference
and locally resolved immutable image ID. The configured command is Docker Cmd,
not an inherited image entrypoint. Nonempty entrypoints are refused rather than
silently executing a different program. Configuration parsing alone does not
verify mount propagation, process identity or continued runtime ownership.

The daemon first commits a FULL-synchronous immediate transaction recording its
intent and reservation. The helper then syncs its immutable bootstrap intent
and containing directory before creating anything. Every externally observed
transition has its own synced record. Runtime attachment state is committed
only after an exact helper acknowledgement and renewed daemon authority check.

The bootstrap journal itself needs an administrative directory. That directory
is an exclusively created, ID-derived child of the fixed bootstrap parent, not
an operational scaffold. Its parent is synced before opening the locked Store.
The initial intent is synced there before any publisher, staging, registration
child or runtime is created. A crash before intent leaves an unknown child:
subsequent allocation refuses it, even if it is empty. No automatic adoption or
repair is permitted. This makes the metadata-creation crash window fail closed.

The first scaffold attachment also requires a private surrounding host mount.
Shared, slave and unbindable surrounding mounts are refused before attaching
anything. The helper does not remount an inherited parent to make it acceptable.
Each newly attached clone is explicitly made private, restricted and freshly
checked before the empty publisher receives its unique shared group. Staging
stays private. These checks are not receiver or attachment verification.

The initial request cannot contain a Docker container ID: that ID does not
exist before create. A separate immutable binding records the full container ID
after create and before start. The registration and incarnation IDs must appear
in fixed Docker labels at create. This bootstrap stream precedes the normal
runtime-bound operation stream, rather than inventing a container ID or changing
the original request after create.

## Create, adopt and quarantine

| Durable state | Permitted next action | Recovery rule |
| --- | --- | --- |
| No intent | Persist a new authorized immutable intent | Refuse any existing administrative child or matching container |
| Intent | Create protected empty scaffold and record its pinned ownership | Existing children without an exact ownership record quarantine |
| Scaffold verified | Persist create-pending, then create the fixed empty read-only rslave receiver | No container selection by name |
| Create pending | Inspect all containers with the exact operation, registration and incarnation labels | Zero matches may retry create only after proving no owned runtime exists. One exact match may bind only after complete configuration and ownership verification. Multiple or foreign matches quarantine |
| Container bound | Start only the bound full container ID and pin its incarnation and namespace | A different ID, restart, substituted scaffold or mismatched identity quarantines |
| Receiver verified | Record fresh host and receiver evidence and commit the runtime journal | Missing or stale evidence quarantines |
| Committed | Return the exact stored result on an exact retry | Never create or start again on retry |
| Quarantined | Report the durable reason, refuse grants and exec | No automatic adoption, deletion or clearing based on Docker inspect alone |

An exact duplicate means the same operation ID and canonical immutable request
bytes. Reused IDs with another body, registration or domain are conflicts.
Missing earlier revisions are barriers. A crash between a side effect and its
durable observation is ambiguous unless the preceding durable ownership record
and complete fresh pinned identity prove the exact object. An empty directory,
matching name, Docker label alone or successful hello is not ownership proof.
Unknown live mounts and foreign children are never deleted implicitly.

## Required negative coverage

- F1: Peer UID, link provenance, malformed/unknown fields and arbitrary paths.
- F2: Parent ownership, substitution, bind aliases and scaffold child races.
- F3: Divergent replay, revision gaps, multiple container matches, wrong labels,
  wrong full ID, runtime incarnation and namespace mismatches.
- F4: Crashes on both sides of intent sync, scaffold creation, create-pending
  sync, Docker create, runtime binding, start, evidence sync and final commit.
- F5: Admission across every workspace-consuming path, permanent domain seals,
  policy and lease authority changes, stop/delete/expiry and restart quarantine.

Pure state-machine tests are not filesystem, Docker or kernel proof. Privileged
synthetic tests require the freshly snapshotted disposable VM and an explicit
lease. They never install or activate the service or enroll a real workspace.

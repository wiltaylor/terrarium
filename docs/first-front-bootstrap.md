# First-front bootstrap-v1 source contract

This is TER-1054 source, not an executable migration plan. TER-1042 D25-D32
and its D26 clarification govern it. Source/model permission does not permit
image assembly, signing, publication, Docker lifecycle, installation, boot,
provisioning, production use or data-loss rollback. No artifact digests are
selected here. TER-1055 supplies the host transaction. TER-1056 must supply
separately authorized real-runtime evidence and independent security review.

## Startup and trust boundary

The fixed image entrypoint invokes `terrarium-bootstrap.py preflight` before
writable directory preparation. A directory mount at
`/run/terrarium-bootstrap` selects bootstrap. Image metadata at
`/usr/local/share/terrarium/bootstrap-v1.json` in recovery mode makes that
mount mandatory. Destination mode permits ordinary startup only without
bootstrap controls or retained bootstrap intent/receipt. An absent or corrupt
helper fails startup. No enable variable bypasses bootstrap.

Bootstrap accepts only the image CMD `--daemon-service terrariumd` and the
image's actual nonroot service UID/GID. Root startup, altered command arguments,
path environment and executable/defaults symlinks are rejected. Exact allowed
paths and PATH are the constants `ENVIRONMENT` and `LINKS` in
`docker/bootstrap.py`. Optional path variables may be absent only where the
image/daemon default is that same constant. `XDG_RUNTIME_DIR` is unsupported.
The host must inspect entrypoint, CMD, mount graph, all effective environment,
numeric users, runtime dependencies and executable resolution before CREATE.
In particular a shell startup environment cannot be authenticated by a script
after the shell already interpreted it. Python is invoked by its absolute
image path with isolated mode and bytecode writes disabled.
The helper also rejects daemon/runner or supervisor-utility shadowing through
the writable tools prefix. Bootstrap launches the daemon by its fixed absolute
image link, not a fresh PATH lookup.

The host is trusted root authority, including its manifest, inspect and
independent artifact verification. The helper cannot authenticate a registry
digest, host bind-source pathname, host mount-binding hash, host COMMITTED
record, filesystem durability implementation or hostile privileged peer.
Host and container state device/inode must match, not be adapted or guessed.
Unknown mount layouts, consumers, ACL semantics or competing writers reject
the host plan. The fixed controls directory is a directory bind, read-only in
the container. File binds hide atomic host publication and are unsupported.

Before any payload process, the helper acquires the existing exclusive
`/state/releases/service.lock` with no-follow/type/owner/link/mode checks,
inherits it as fd 9, and completes mandatory init. The entrypoint never runs
its separate harness installation in bootstrap, including after admission.
Immediately before each supervised child, including crash retry and upgrade
resume, validation checks image metadata, bundle, descriptor, state binding,
receipt, complete selected tree, current link, marker and admission. Validation
never stages, selects, repairs, recreates probation or starts another program.
Failure terminates the supervisor without a child. Existing signal handling
and runner shutdown remain unchanged. Generic hot-swap commands reject held
bootstrap generations, so they cannot commit/prune/remove probation.

This assumes the host stops the updater and excludes every concurrent payload
or selection writer through validation/exec. It does not defend against a
compromised daemon or host root. Unchanged old-daemon internal reexec, delayed
harness/config activation and runner compatibility require process traces,
not just model results.

## Control ABI

`docker/first-front/bootstrap-v1.schema.json` describes exact object fields.
The parser additionally requires canonical ASCII JSON, sorted keys, no
whitespace or trailing newline, no duplicate/unknown keys, no float/constant,
and a maximum of 16384 bytes per object. Schema is integer 1, not a Boolean.
Device/inode decimal strings are bounded to unsigned 64-bit values.

Object identity is SHA-256 of the exact ASCII domain, one NUL, then canonical
object bytes. Domains are `terrarium-bootstrap-descriptor-v1`,
`terrarium-bootstrap-receipt-v1`, `terrarium-bootstrap-marker-v1` and
`terrarium-bootstrap-admission-v1`. There is no own-hash field.
`manifest_sha256` and `commit_sha256` are hashes of immutable external bytes,
not constructed self-references. Host manifest/commit verification remains
host work. The container checks their syntax and transaction bindings, not
their external contents or OCI attestations.

`descriptor.json` and optional `admission.json` are host-root owned. Their
source modes are 0600, directory 0700, with exactly the actual service UID's
read and search ACL when needed. Linux ACL masks expose mode bits 0640/0750
but the base group entry must grant nothing. Only user_obj, this service user,
empty group_obj, exact read/search mask and empty other are accepted. No
service write, named groups, extra users, default ACL or unrecognized xattr.
The helper checks effective ACL entries and read-only filesystem flags.
Ancestor traversal and source ownership/ACL checks outside the container
remain mandatory host checks. Unsupported security-label/xattr layouts fail
closed, not by widening permissions.

Receipt `bootstrap-v1.json` and marker `probation` live under
`/state/releases`, service-owned 0600, regular, single-link, no-follow and
without extended ACL/xattrs. Directory components are opened without following
symlinks. Failed writes, file sync, rename or directory sync abort startup.
Local control reads sync the exact opened file and validation syncs releases,
so a surviving exact object from an interrupted sync is made durable before
launch, without repairing its contents or inferring admission.

| Admission | Marker | Child launch |
|---|---|---|
| absent | exact matching | probation |
| absent | absent/foreign/unreadable | reject |
| matching | exact matching | allowed, host still must unlink |
| matching | absent | admitted, never re-probation |
| invalid/unreadable | any | reject |
| any | foreign/unreadable | reject |

Host durable COMMITTED or RESTORED_COMMITTED precedes durable admission
publication, which precedes exact-marker unlink and parent sync. The helper
never publishes admission or unlinks the marker. A child may activate before
unlink-parent sync returns. Such a failure is forward-only host recovery,
not rollback permission. Publication racing a read may reject, never grant
admission from an inconsistent observation.

## Complete payload identity and private initialization

Image-owned metadata has exactly `schema`, `protocol`, `mode`, numeric `uid`
and `gid`, `release_id`, `payload_sha256`, and `inventory`. It is canonical,
root-owned, non-peer-writable, single-link and at most 16 MiB. Destination
metadata is authored in the image build from the actual bundle, never from
caller controls. Bundle and metadata remain in one final filesystem layer.
This source change does not build that image. Ordinary hot-swap provenance
behavior is unchanged, and a later bootstrap requires a full container
replacement whose metadata and bundle match the desired payload.

Payload inventory is a JSON array sorted by raw relative pathname bytes,
including the root directory as empty path. Every record has `path` (standard
base64 bytes), `type` (`directory`, `file`, `symlink`), and `mode` (four-digit
octal string). Files also have integer `size` and lowercase `sha256`. Symlinks
also have `target` (base64 literal target bytes). No inode/ctime/atime, mutable
image identity or self-hash is part of this content/mode/link inventory.
Payload hash is SHA-256 of `terrarium-payload-manifest-v1`, NUL, canonical
inventory bytes. This is the payload subset of D29's byte-path/content/mode/
link encoding, not the richer cold-root checkpoint format.

The fd walk rejects payload xattrs/ACLs rather than silently dropping metadata
it cannot preserve. It does not follow links while hashing. A separate complete-graph
resolution rejects dangling links, loops, escapes, absolute links and paths
traversing regular files. Special files, unexpected hardlinks, set-ID/sticky
and peer-writable payload entries reject. The only selected-tree bookkeeping
allowlist entry is root-level `image.json`: a bounded regular single-link
service-owned non-peer-writable JSON object with no xattrs. It is excluded
from payload identity and has no selection, executable or digest authority.
The bundle has no such exclusion. Any other extra entry, including an
`image.json.new`, rejects. A host plan must inventorize effective old payload
and hot-swap bookkeeping separately, establish its
artifact provenance before mutation and reject incompatible layouts rather
than delete unknown evidence to make validation pass.

Private `bootstrap-intent-v1.json` binds descriptor hash, release ID and the
observed prior current release ID/tree hash, or explicit prior absence. It
is not another caller API. The host must already have independently verified
the old effective tree before delivering the descriptor. The helper's local
prior-tree observation is an interrupted-selection binding, NOT provenance
proof or authority to execute it. No old payload process is run by init.
Only a fully verified selected tree matching the image-owned inventory may
launch. On matching intent resume, the prior selection must remain unchanged
or already be the fully verified desired release. An unrelated current link,
foreign intent, invalid same-ID tree or stale initialized sentinel rejects.

Initialization creates a fresh intent-owned generation stage, copies the
verified bundle through directory descriptors, preserves bytes/modes/literal
links, verifies it, syncs files and directories bottom-up, renames it and
syncs releases. Existing final trees are fully verified, never overwritten.
Current selection is atomically renamed and releases synced before the
prepared receipt. Exact probation is durable before receipt completion.
Interrupted `.new` files or partial stages remain evidence and may require
host escalation. No cleanup or speculative stage reuse is attempted.

## Recovery recipe and evidence boundary

There is intentionally no runnable historical-image assembly recipe or
placeholder base digest here. A separately approved recovery recipe must
name the independently verified original platform/index/config/layer/source
identities and retain its runtime dependencies and numeric users. Patch only
the exact old entrypoint/bootstrap integration, not the current hot-swap
implementation wholesale. Use recovery-mode image metadata from the unchanged
extracted old bundle. Do not rebuild old source and call it unchanged payload.
Record recipe/toolchain/patch/control-schema hashes and two assembly
comparisons. A required old-payload modification or unsupported ABI stops for
a decision, not a compatibility bypass.

Recovery-only publication belongs to `ghcr.io/wiltaylor/terrarium-recovery`,
with reviewed signatures/attestations, platform digest/index mapping and the
D30 recovery tag convention. Prove exclusion from both old/new normal
discovery. A name or label is not that proof. Destination needs a fresh
ordinary signed release with three explicit pins, never relabelled 0.64.0.
No publication or verification has occurred for this source card.

Recovery recreations remain owned by the fixed host recovery service and
external durable effective-runtime override. Preserve historical pin bytes
and report effective recovery digest separately. Missing/corrupt override
rejects recreation. Ordinary launchers, old updater, sockets and timers stay
held until a separately authorized forward handoff. Bootstrap neither installs
those holds nor releases them. Host isolation, old DB/key/config/runner ABI,
offline restart and process traces are TER-1056 evidence gates.

## Model validation

After explicit lead RESUME and target grant, on the clean pushed SHA:

```sh
uv run --with pytest pytest scripts/test_bootstrap.py scripts/test_hot_swap.py scripts/test_dockerfile.py --junitxml=/absolute/results/bootstrap.xml
TERRARIUM_CHANGED_FILES=/absolute/changed-paths.txt just ci::validate-affected
terrarium test push /absolute/results/bootstrap.xml --name bootstrap-models --card TER-1054 --commit <pushed-sha> --branch ter-1054
```

Run the affected recipe under the repository's target slot rules. Models cover
schema, controls, ACL masks, bindings, complete payload mutation, admitted
restarts, lock exclusion and exception injection before/after init sync/
rename/selection boundaries. Static source assertions check spawn placement
and harness suppression. They are not process traces, power-loss emulation,
old runtime compatibility, OCI proof, boot isolation or production acceptance.

# Releases

This document tells you how to make a Terrarium release and how to verify
what a release publishes. Releases use the local release job (TER-623),
`terrarium job release`, which runs `just release::publish` on the release
client and signs with cosign and the release key pair. GitHub Actions no
longer tests or releases Terrarium. See "The local release build" below.

The host agent self-update that uses the release binary is in
[host-protocol.md, "Self-update"](host-protocol.md#self-update).

## What a release publishes

| Item | Where |
|---|---|
| The image | `ghcr.io/wiltaylor/terrarium:<version>` |
| The channel tag | `:latest` for a stable release, `:pre` for a pre-release |
| The host agent | `terrarium-host-<version>-linux-x86_64` on the GitHub release |
| The host helper | `terrarium-host-helper-<version>-linux-x86_64` on the GitHub release ([native-gpu-helper.md](native-gpu-helper.md)) |
| The checksums | `SHA256SUMS` on the GitHub release (the host agent and the host helper) |
| The tag | `v<version>` on the commit |

The image has these parts:

- The runtime stage of `docker/Dockerfile`, for `linux/amd64`.
- The OCI labels `org.opencontainers.image.version`, `.revision` (the
  commit), and `.source` (the repo URL), and also `.title`,
  `.description`, and `.licenses`.
- A BuildKit SBOM and a BuildKit provenance attestation (`mode=max`) in
  the image index.
- A cosign signature for the image digest, checked with the release public key.
- Each binary reports `<version>+<commit>` (`TERRARIUM_GIT_REF` is the
  commit; [CONVENTIONS.md](CONVENTIONS.md), "Build version").

The host agent asset comes out of the pushed image
(`/opt/terrarium/dist/terrarium-host-x86_64-linux`). Thus the release
binary and the binary that a daemon of this image serves at
`GET /install/terrarium-host` have the same bytes. The local release build does these checks
before the upload:

1. The SHA-256 of the binary is the same as the `.sha256` file in the
   image. The daemon serves that file, and the self-update offer holds
   that hash.
2. `terrarium-host --version` reports the release version.

The host helper (`terrarium-host-helper`, the root helper of native GPU
enforcement) is built in the same Dockerfile stage as the host agent
(static musl, `cargo build --locked`) and comes out of the same directory
(`/opt/terrarium/dist/terrarium-host-helper-x86_64-linux`). The local release build makes the
same two checks for it. `SHA256SUMS` lists both binaries.

Both binaries and SHA256SUMS also get cosign signature bundles.

## How to make a release

### Version branch e2e at the cut

After pushing `release/X.Y.Z`, the project admin queues the cut-time gate
on the open version group's header card (not a member card):

```text
POST /api/cards/TER-<header>/e2e
{"branch":"release/X.Y.Z","repo":"terrarium","eta":"20m"}
```

The daemon pins the remote branch head and runs
`TERRARIUM_DOCKER_CI=1 python3 scripts/docker_ci.py` on that tree. The job
shares the merge lane. Only e2e jobs receive the Docker relay. Merge and
validate jobs cannot request a reset or tunnel, whatever their recipe sets.
It does not merge or push a branch, move the header or change membership.
It records the job and tested commit on the group's `version branch e2e`
step, marking it done only on success, failed on failure or cancellation.
Do not pre-release until this step is done on the final release commit.
After a blocker fix, queue it again on the same group and release branch.

The gate runs the full validation suite, then the two ignored live Docker
tests named in `scripts/docker_ci.py`. It rejects skipped or unmatched live
tests. Results are written under `results/` in the job slot. Upload
`results/docker.json` and the suite's result files with
`terrarium test push --card <header> --label version:X.Y.Z`.
Device-IO and Claude live tests still need their separate hardware or
credentials. If applicable, run them on the same release commit and record
their evidence on the group before publishing. They are not silently
enabled by the Docker job.

The delivery principal needs the existing Docker CI tunnel and reset run
grants and pinned endpoints. Both keys stay inside the daemon. Branch code
gets only a job-scoped Unix relay endpoint. Never fetch a VM key or run the
gate manually against the real VM. Card validation remains ineligible.

### Publishing

After the version cut passes, the lead merges the release branch to `main`
with `tested-at-cut: <test run id>` and a release trailer, then starts
`terrarium job release <version card> --commit <main commit>`. A push alone
publishes nothing. The release job reads these trailers:

| Trailer | Result |
|---|---|
| `release: true` | A stable release `X.Y.Z`. The image gets `:latest`. |
| `pre-release: true` | A pre-release `X.Y.Z-alpha`. The image gets `:pre`. |
| `bump: major\|minor\|patch` | Optional. It overrides the bump that the release script finds in the conventional-commit subjects since the last `v*` tag. |

A `release: true` commit on top of the last tag `vX.Y.Z-alpha` promotes that
pre-release: the version is `X.Y.Z` with no bump. A `bump:` trailer on the same
commit still bumps. A release also bumps when `vX.Y.Z` already exists. With a
`pre-release: true` trailer too, the commit stays a pre-release. The last tag
`vX.Y.Z` (a full release) gives the normal bump.

The full suite and end-to-end tests run on the release branch at the version
cut. Run `TERRARIUM_JOB_KIND=merge just ci::validate` for the full suite,
including the uid 1001 pass, and push the results with the version label.
Only a green cut permits the `tested-at-cut` trailer. Card checks still run
before review. The merge queue merges without tests.

With no release trailer the local release job publishes nothing. See the
local release steps below for build, signing and publication order.

### Make the package public (one time)

A new GHCR package is private, and the REST API cannot change the
visibility of a package. After the first release, do these steps one
time:

1. Open `https://github.com/users/wiltaylor/packages/container/terrarium/settings`.
2. In **Danger Zone**, select **Change visibility**, then **Public**.

To check the visibility, pull the image with no login:
`docker logout ghcr.io && docker pull ghcr.io/wiltaylor/terrarium:latest`.

## How to verify a release

### A release of the local build (cosign)

You need cosign 3 and the release public key, `docker/release/cosign.pub`
in the repo (the image has it at `/opt/terrarium/release/cosign.pub`):

```sh
cosign verify --key cosign.pub ghcr.io/wiltaylor/terrarium@sha256:<digest>
gh release download v<version> --repo wiltaylor/terrarium
cosign verify-blob --key cosign.pub --bundle terrarium-host-<version>-linux-x86_64.sigstore.json terrarium-host-<version>-linux-x86_64
cosign verify-blob --key cosign.pub --bundle terrarium-host-helper-<version>-linux-x86_64.sigstore.json terrarium-host-helper-<version>-linux-x86_64
cosign verify-blob --key cosign.pub --bundle SHA256SUMS.sigstore.json SHA256SUMS
sha256sum -c SHA256SUMS
```

cosign also checks the entry of each signature in the public transparency
log. The release notes give the digest and these commands.

### Historical workflow releases (GitHub attestation)

You need the GitHub CLI (`gh`) 2.49 or later. `gh attestation verify`
checks the Sigstore bundle. It also checks that the certificate identity
is a workflow of the repository.

**The image:**

```sh
gh attestation verify oci://ghcr.io/wiltaylor/terrarium:0.3.0 \
  --repo wiltaylor/terrarium \
  --signer-workflow wiltaylor/terrarium/.github/workflows/ci.yml
```

The command prints the digest, the workflow, and the commit. Use the
digest from the release notes to pull the exact image:
`docker pull ghcr.io/wiltaylor/terrarium@sha256:<digest>`.

To read the labels and the BuildKit SBOM:

```sh
docker buildx imagetools inspect ghcr.io/wiltaylor/terrarium:0.3.0 --format '{{json .Image.Config.Labels}}'
docker buildx imagetools inspect ghcr.io/wiltaylor/terrarium:0.3.0 --format '{{json .SBOM}}'
```

**The host agent:**

```sh
gh release download v0.3.0 --repo wiltaylor/terrarium
sha256sum -c SHA256SUMS
gh attestation verify terrarium-host-0.3.0-linux-x86_64 \
  --repo wiltaylor/terrarium \
  --signer-workflow wiltaylor/terrarium/.github/workflows/ci.yml
```

The host helper is verified the same way, with
`terrarium-host-helper-0.3.0-linux-x86_64` (releases after v0.35.0-alpha; see
[native-gpu-helper.md](native-gpu-helper.md), install step 3).

A host agent that updates itself checks the SHA-256 that the daemon
sends. The daemon reads that hash from the image. When the dist dir of
the daemon has the `SHA256SUMS` of the release, the host agent also
checks the release hash ([host-protocol.md, "Self-update"](host-protocol.md#self-update)).
The host agent does not verify the attestation itself: the image that
holds the binary is the item to verify.

## The full suite at the version cut

Merge jobs run no tests: the full suite runs once at the version cut, on
the version branch, as a release validation (TER-683):

```sh
mkdir -p /src/<cut>-results
TERRARIUM_VALIDATE_CONTEXT=release RESULTS_DIR=/src/<cut>-results just ci::validate
just ci::flaky-cards /src/<cut>-results/reruns.json "the <X.Y.Z> cut"   # read it, then run it with | sh
```

`TERRARIUM_VALIDATE_CONTEXT=release` runs the uid 1001 pass after
`ci::test` (as root; the host needs uid 1001, see AGENTS.md) and lets both
test passes rerun their failed tests once. A test that passes on the rerun
keeps the run green and is listed in `reruns.json`; one that fails again is
red. A build error, a crash, a failed doctest, or more than 5 failed tests
gets no rerun ([api.md](api.md) 35.3). `ci::flaky-cards` prints the
`terrarium` commands that file or comment on the `flaky` card of each test
that passed on the rerun; it runs nothing itself.

## The local release build

`just release::publish` runs `scripts/release.py` on a clean checkout of the
commit to release. The release job (TER-623) runs it on the release client:
a small VM (META-119), as the non-root user `release`, with only git,
python3, curl, jq, the docker CLI with buildx (no daemon), crane, cosign,
and gh (`docker/release.Dockerfile` pins them). The client runs no code of
the release. The gate and the image build run on the release VM's Docker
daemon, which the Terrarium daemon reaches through its Docker relay: the
recipe gets only `TERRARIUM_DOCKER_HOST=unix://<job socket>`, as merge jobs
get the Docker CI VM ([api.md](api.md), the delivery `docker_ci` settings).
The VM gets no secret. The release uses the same trailers and version rules
as the workflow (`scripts/release_version.py`). The steps:

1. `version`: the version from the trailers of HEAD. No trailer: no
   release, exit 0.
2. `check`: a clean checkout, the secrets in the env, the tools on PATH,
   `docker/release/cosign.pub`, and no tag or release `v<version>` yet. It
   logs the `X-OAuth-Scopes` and `github-authentication-token-expiration`
   headers of `GET https://api.github.com/user` (never the token), and stops
   when the token lacks `repo` or `write:packages`.
3. `gate`: `just ci::validate` as uid 1001 in a container on the release
   VM. The image is `docker/release-gate.Dockerfile` (the toolchain of the
   old CI: rust 1.97.1, rustfmt, clippy, just, uv, node 22, bubblewrap), and
   a clone of the commit with its `.git` is copied in as `/src`. The
   container gets only `RESULTS_DIR` and `HOME`. Its `/results` comes back
   to `release-out/results`, also from a red gate. A red gate publishes
   nothing. For a release or pre-release commit with a non-empty
   `tested-at-cut: <test run ids>` trailer, this gate is skipped. The lead
   adds that trailer only after the full suite passes on the release branch
   (MEM-209). The summary records the skip reason and run ids, and the
   release notes name those runs. Without the trailer the gate runs normally.
4. `stamp`: the version into `[workspace.package]` of `Cargo.toml` and the
   workspace crates of `Cargo.lock` (the entries with no `source`), as
   `.github/stamp-version.sh` did, without cargo. The image build's
   `cargo build --locked` checks the result.
5. `image`: `docker buildx build` of the runtime stage of
   `docker/Dockerfile` for `linux/amd64`, with the OCI labels, an SBOM,
   and provenance (`mode=max`). The export is an OCI layout that comes back
   to the client (`release-out/image`); the VM pushes nothing.
6. `extract`: the host agent and the host helper out of that layout, with
   the two checks of "What a release publishes", and `SHA256SUMS`. The
   `--version` check runs on the release VM, in a container of the pinned
   `debian:trixie-slim` with no network, uid 65534, and no capabilities:
   the client never runs a binary of the release.
7. `push`: crane pushes the same layout as `:<version>` (the same bytes, so
   the digest of the build) and tags `:pre` or `:latest`.
8. `sign`: cosign signs the image digest, both binaries, and `SHA256SUMS`
   with the release key and the public transparency log (Rekor), and then
   verifies each signature, with its log entry, with
   `docker/release/cosign.pub` of the checkout. A key that does not match
   stops the release here.
9. `release`: `gh release create v<version> --target <commit>` (the API
   makes the tag), with the binaries, `SHA256SUMS`, and the cosign bundles
   (`<asset>.sigstore.json`).

`release-out/summary.json` records each step: the version, the commit, the
token scopes and expiry, the gate, the digest, the asset hashes, and the
release URL. The lead pushes `release-out/results` with `terrarium test
push`.

`just release::dry-run` runs steps 1 to 6 with no secret: nothing is pushed,
signed, or released. `--no-gate` skips the gate in a dry run only.

### The release job

The daemon runs the release as a job of kind `release` (TER-623,
[api.md §35.6](api.md)). The setup, once, by the operator or meta/lead:

1. Define the release client (META-119) as a global ssh target (the
   operator), user `release`, with its pinned host key. Install the
   release tools system-wide (the job's `PATH` has no dir under the
   release user's home). Once it is the release
   target, the daemon refuses every agent on it (exec, shell, copy,
   policy entries, leases, reservations); only the operator and the
   release job use it. Each release runs in a new HOME that goes after
   it.
2. Grant `release-github`, `release-cosign`, and `docker-release-vm` to
   `delivery:terrarium` with `--level run --target <release client>`, and
   to nobody else.
3. Set the delivery config (the operator only: the release fields are
   the operator's):

   ```sh
   terrarium project delivery set --release-target <release client> \
     --release-secret GHCR_TOKEN=@secret:release-github \
     --release-secret GH_TOKEN=@secret:release-github \
     --release-secret COSIGN_PRIVATE_KEY=@secret:release-cosign.private_key \
     --release-secret COSIGN_PASSWORD=@secret:release-cosign.password \
     --docker-release-destination root@10.3.0.2 --docker-release-port 2251 \
     --docker-release-known-hosts '<the pinned known_hosts line>' \
     --docker-release-key @secret:docker-release-vm
   ```

   The daemon refuses a release target that is the job target of any
   project.

The lead's release step, after the release merge onto `main`:
`terrarium job release <version card>` (the head of `main`, or
`--commit <sha>` to make sure it is that commit). The job clones the
commit into a new tree on the client, opens the relay to the release VM,
and runs `just release::publish` with the secrets in its env. The end comes
as a comment on the card. A lost run does not run again by itself.

### Secrets

The secrets are Terrarium secrets, granted to the release job's principal
(`delivery:terrarium`) with `--level run` and the release client target only.
That target runs no merge or validate job, so the code of a card cannot
resolve them. The job passes them into the env of the release process only,
never into a sandbox or to the VM.

| Env | Secret | What it is | Who gets it |
|---|---|---|---|
| `GHCR_TOKEN` (`GHCR_USER`, default `wiltaylor`) and `GH_TOKEN` | `release-github` (field `value`) | one classic token with `repo` and `write:packages` (the operator's choice; it reaches all of the operator's repos) | crane and the image commands of cosign (`cosign sign` and `cosign verify`; not `sign-blob`, `verify-blob`, or gh) through a mode-0600 `config.json` in a private mode-0700 directory in RAM (`XDG_RUNTIME_DIR` or `/dev/shm` on tmpfs; no tmpfs stops the release), which also holds the state of cosign (`TUF_ROOT`) and gh (`GH_CONFIG_DIR`) and which the script removes at the end, also when the run is stopped by SIGTERM, SIGHUP or SIGINT; `gh` and the scope check as `GH_TOKEN` |
| `COSIGN_PRIVATE_KEY`, `COSIGN_PASSWORD` | `release-cosign` (fields `private_key`, `password`) | the encrypted private key of the release key pair (cosign 3.1.4), base64-encoded in one line, and its password | `cosign sign` and `cosign sign-blob` only. The script decodes the key in memory and passes the PEM in the env (`--key env://COSIGN_PRIVATE_KEY`): no key file |

No secret is on a command line, in the output, in `release-out/`, in the
build context, in an image layer, or on the release VM: docker (the gate and
the image build, and the `--version` checks) gets an env without them, and the client runs no code of the
release (`scripts/test_release.py` checks each of these).

### Fallback if the local release fails

Revert the TER-622 merge commit on both `next` and `main` to restore
`.github/workflows/ci.yml`, then release through that workflow. The lead
performs those reverts through the delivery process. Workflow releases have
GitHub attestations, not cosign signatures. Daemons must use
`TERRARIUM_UPDATE_VERIFY=gh` for that fallback release. Switch back to
`cosign` before installing a local release. An older updater that only
supports `gh` refuses local releases until it is upgraded.

## Not in this document

The updater that pulls a new image and goes back to the old image when
the update fails is [issue #19](https://github.com/wiltaylor/terrarium/issues/19),
part B. A development deploy from the source (`just deploy::*`) does not
use the release image.

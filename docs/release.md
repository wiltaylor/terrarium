# Releases

This document tells you how to make a Terrarium release and how to verify
what a release publishes. Two paths make a release:

- The local release build, `just release::publish` (`scripts/release.py`):
  the lead's release step runs it in the release build container on a
  build target. It signs with cosign and the release key pair. See
  "The local release build" below.
- The GitHub workflow `.github/workflows/ci.yml`: it publishes the releases
  until the switch to the local build is done (TER-622), and it stays the
  fallback until then. It attests with GitHub build provenance.

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
- A GitHub build provenance attestation for the image digest. It is a
  Sigstore bundle with a keyless certificate for the identity of the
  workflow (`actions/attest-build-provenance`). The bundle is pushed to
  the registry and is also stored on GitHub.
- Each binary reports `<version>+<commit>` (`TERRARIUM_GIT_REF` is the
  commit; [CONVENTIONS.md](CONVENTIONS.md), "Build version").

The host agent asset comes out of the pushed image
(`/opt/terrarium/dist/terrarium-host-x86_64-linux`). Thus the release
binary and the binary that a daemon of this image serves at
`GET /install/terrarium-host` have the same bytes. CI does these checks
before the upload:

1. The SHA-256 of the binary is the same as the `.sha256` file in the
   image. The daemon serves that file, and the self-update offer holds
   that hash.
2. `terrarium-host --version` reports the release version.

The host helper (`terrarium-host-helper`, the root helper of native GPU
enforcement) is built in the same Dockerfile stage as the host agent
(static musl, `cargo build --locked`) and comes out of the same directory
(`/opt/terrarium/dist/terrarium-host-helper-x86_64-linux`). CI makes the
same two checks for it. `SHA256SUMS` lists both binaries.

Both binaries also get a GitHub build provenance attestation.

## How to make a release

A release occurs only for a push to `main` when the head commit has a
trailer:

| Trailer | Result |
|---|---|
| `release: true` | A stable release `X.Y.Z`. The image gets `:latest`. |
| `pre-release: true` | A pre-release `X.Y.Z-alpha`. The image gets `:pre`. |
| `bump: major\|minor\|patch` | Optional. It overrides the bump that CI finds in the conventional-commit subjects since the last `v*` tag. |

A `release: true` commit on top of the last tag `vX.Y.Z-alpha` promotes that
pre-release: the version is `X.Y.Z` with no bump. A `bump:` trailer on the same
commit still bumps. A release also bumps when `vX.Y.Z` already exists. With a
`pre-release: true` trailer too, the commit stays a pre-release. The last tag
`vX.Y.Z` (a full release) gives the normal bump.

With no trailer, CI runs only the tests. Example:

```sh
git commit -m "feat(board): card templates" -m "release: true"
git push origin main
```

The jobs run in this sequence:

1. `check`: `just ci::check`, one part in each step.
2. `version`: it calculates the version from the last `v*` tag (0.x
   semantics: a breaking change bumps the minor).
3. `image`: it stamps the version into `Cargo.toml`, builds and pushes
   the image, attests it, takes the host agent and the host helper out
   of it, writes `SHA256SUMS`, and attests both binaries.
4. `release`: it pushes the tag and makes the GitHub release with the
   host agent, the host helper, `SHA256SUMS`, and notes that give the image digest.

The `image` job pushes the image before the `release` job makes the tag.
If `release` fails, the image exists with no release. Run the `release`
job again (re-run failed jobs), or delete the package version.

### Permissions

The workflow gives `contents: read` to each job by default. Only these
jobs get more:

| Job | Permissions | For |
|---|---|---|
| `image` | `packages: write` | the push to `ghcr.io` |
| | `id-token: write` | the Sigstore certificate of the attestations |
| | `attestations: write` | the storage of the attestations on GitHub |
| `release` | `contents: write` | the tag and the release |

Each action is pinned to a full commit SHA. A comment gives the version.
To update an action, find the commit of the new tag
(`gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`). Then change the
SHA and the comment together.

### Make the package public (one time)

A new GHCR package is private, and the REST API cannot change the
visibility of a package. After the first release, do these steps one
time:

1. Open `https://github.com/users/wiltaylor/packages/container/terrarium/settings`.
2. In **Danger Zone**, select **Change visibility**, then **Public**.
3. Make sure that **Manage Actions access** gives the repository
   `wiltaylor/terrarium` the role **Write**. The
   `org.opencontainers.image.source` label links the package to the
   repository, and the first push from the workflow usually sets this
   role.

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

### A release of the workflow (GitHub attestation)

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

## The local release build

`just release::publish` runs `scripts/release.py` on a clean checkout of
the commit to release. The release job runs it on the release client target
(the image of `docker/release.Dockerfile`: root in the container for the
recipe, the user `release` (uid 1001) for the gate, the docker CLI with
buildx, crane, cosign, gh, and the toolchain of the gate). The image build
runs on the release VM's Docker daemon, which the Terrarium daemon reaches
through its Docker relay: the recipe gets only
`TERRARIUM_DOCKER_HOST=unix://<job socket>`, as merge jobs get the Docker CI
VM ([api.md](api.md), the delivery `docker_ci` settings). The VM gets no
secret. It uses the same trailers and version rules as the workflow
(`scripts/release_version.py`). The steps:

1. `version`: the version from the trailers of HEAD. No trailer: no
   release, exit 0.
2. `check`: a clean checkout, the secrets in the env, the tools on PATH,
   `docker/release/cosign.pub`, and no tag or release `v<version>` yet. It
   logs the `X-OAuth-Scopes` and `github-authentication-token-expiration`
   headers of `GET https://api.github.com/user` (never the token), and stops
   when the token lacks `repo` or `write:packages`.
3. `gate`: `just ci::validate` on its own clone of the commit
   (`release-out/gate-src`) as uid 1001 (`setpriv`). Publish runs as root
   in the client container and refuses to run as another user: a process of
   the same uid could read the secrets in `/proc/<pid>/environ` of the
   recipe's process tree, and the gate runs the code and the build scripts
   of the release. The gate's env has no secret, and `RESULTS_DIR` is
   `release-out/results`. A red gate publishes nothing.
4. `stamp`: the version into `Cargo.toml` and `Cargo.lock`.
5. `image`: `docker buildx build` of the runtime stage of
   `docker/Dockerfile` for `linux/amd64`, with the OCI labels, an SBOM,
   and provenance (`mode=max`). The export is an OCI layout that comes back
   to the client (`release-out/image`); the VM pushes nothing.
6. `extract`: the host agent and the host helper out of that layout, with
   the two checks of "What a release publishes", and `SHA256SUMS`.
7. `push`: crane pushes the same layout as `:<version>` (the same bytes, so
   the digest of the build) and tags `:pre` or `:latest`.
8. `sign`: cosign signs the image digest, both binaries, and `SHA256SUMS`
   with the release key, and then verifies each signature with
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

### Secrets

The secrets are Terrarium secrets, granted to the release job's principal
(`delivery:terrarium`) with `--level run` and the release client target only.
That target runs no merge or validate job, so the code of a card cannot
resolve them. The job passes them into the env of the release process only,
never into a sandbox or to the VM.

| Env | Secret | What it is | Who gets it |
|---|---|---|---|
| `GHCR_TOKEN` (`GHCR_USER`, default `wiltaylor`) and `GH_TOKEN` | `release-github` (field `value`) | one classic token with `repo` and `write:packages` (the operator's choice; it reaches all of the operator's repos) | crane and cosign through a mode-0600 `config.json` in a private directory in RAM (`XDG_RUNTIME_DIR` or `/dev/shm` on tmpfs; no tmpfs stops the release) that the script removes at the end, also when the run is stopped by SIGTERM, SIGHUP or SIGINT; `gh` and the scope check as `GH_TOKEN` |
| `COSIGN_PRIVATE_KEY`, `COSIGN_PASSWORD` | `release-cosign` (fields `private_key`, `password`) | the encrypted private key of the release key pair (cosign 3.1.4), base64-encoded in one line, and its password | `cosign sign` and `cosign sign-blob` only. The script decodes the key in memory and passes the PEM in the env (`--key env://COSIGN_PRIVATE_KEY`): no key file |

No secret is on a command line, in the output, in `release-out/`, in the
build context, or in an image layer: the gate, the stamp, and the image
build get an env without them (`scripts/test_release.py` checks each of
these).

### The switch from the workflow to the local build

A daemon checks a new release with the updater of the image that it runs.
An updater before TER-613 checks with `gh` and can only accept a release
that the workflow made. The order:

1. Merge TER-613 (cosign and `docker/release/cosign.pub` in the image, the
   updater's `cosign` default, the release build). The workflow stays.
2. Release N with the workflow. The daemons install it with their `gh`
   check.
3. On each daemon on N, set `TERRARIUM_UPDATE_VERIFY=cosign` in `.env`
   (an older `compose.yaml` passes `gh` by itself) and recreate the
   updater.
4. Remove the workflow (TER-622).
5. Release N+1 with `just release::publish`. The updaters of N check it
   with cosign.

A daemon that still checks with `gh` at step 5 refuses N+1 (fail closed)
until step 3. If the local build fails after step 4, the fallback is a
revert of the TER-622 commit and a release with the workflow; that release
has no cosign signature, so the daemons set `TERRARIUM_UPDATE_VERIFY=gh`
for it.

## Not in this document

The updater that pulls a new image and goes back to the old image when
the update fails is [issue #19](https://github.com/wiltaylor/terrarium/issues/19),
part B. A development deploy from the source (`just deploy::*`) does not
use the release image.
